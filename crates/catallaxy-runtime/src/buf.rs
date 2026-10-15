//! Arrays of atomic words for containers whose readers do not lock (lists, and dicts read
//! through their seqlock). Every slot is an `AtomicU64`, so a reader racing a writer sees some
//! value that was stored there, never a torn one. A writer that outgrows a buffer copies it into
//! a new one, publishes the new pointer with `Release`, and hands the old one to [`release`],
//! which frees it only once no other thread can be reading it.
//!
//! **Written ranges.** A buffer this module allocates is preceded by one more word: the range of
//! slots written since the owner's last minor collection, which a minor collection of an old
//! container scans instead of every slot (issue #120). See [`WordBuf::note_written`]. Buffers
//! embedded in a container's own object ([`WordBuf::init_in_place`]) have no such word.
//!
//! **Cards.** A buffer of at least [`CARD_MIN`] slots also has a card table in front of that
//! word: one bit per [`CARD`] slots, set when a young reference is stored into the card's slots.
//! A minor collection scans only the dirty cards, so writes scattered over a large buffer cost
//! what they touched rather than the span between them (issue #144). Such a buffer's range word
//! always reads as covering nothing, so every recorded store takes the slow path that marks the
//! card.

use std::alloc::Layout;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ownership::{Access, PROMOTE_ON_ACCESS};

/// A capacity word followed by `cap` atomic slots.
#[repr(C)]
pub struct WordBuf {
    cap: usize,
    slots: [AtomicU64; 0],
}

/// A buffer with its written-range word in front, as this module allocates them.
#[repr(C)]
struct Prefixed {
    written: AtomicU64,
    buf: WordBuf,
}

/// Words in front of the capacity word of a buffer this module allocates (besides its cards).
const PREFIX: usize = 1;

/// Slots per card.
pub const CARD: usize = 128;
/// The smallest buffer with a card table, in slots.
pub const CARD_MIN: usize = 4096;

/// Words the card table of a buffer of `cap` slots takes (none below [`CARD_MIN`]).
#[inline(always)]
fn card_words(cap: usize) -> usize {
    if cap >= CARD_MIN {
        cap.div_ceil(CARD).div_ceil(64)
    } else {
        0
    }
}

/// Words in front of the capacity word of a buffer of `cap` slots.
#[inline(always)]
fn head_words(cap: usize) -> usize {
    PREFIX + card_words(cap)
}

/// Written-range words: the high half is the first slot written, the low half one past the last.
/// (So a plain count `n` below 2^32 reads as the range `0..n`; see [`WordBuf::range_covers`].)
/// Every slot, including any index too large for the encoding.
const WRITTEN_ALL: u64 = u32::MAX as u64;
/// No slot.
const WRITTEN_NONE: u64 = (u32::MAX as u64) << 32;
/// The range word of a buffer with cards: like [`WRITTEN_NONE`], it covers no slot, so every
/// recorded store reaches [`WordBuf::mark_card`].
pub const CARDED: u64 = ((u32::MAX as u64) << 32) | 1;

/// The buffer of every container with no capacity. Never written, never freed.
static EMPTY: Prefixed = Prefixed { written: AtomicU64::new(WRITTEN_ALL), buf: WordBuf { cap: 0, slots: [] } };

impl WordBuf {
    fn layout(cap: usize) -> Layout {
        Layout::from_size_align(8 * (cap + 1 + head_words(cap)), 8).expect("container too large")
    }

    /// The layout of a buffer of `cap` slots, if it can have one.
    fn try_layout(cap: usize) -> Option<Layout> {
        cap.checked_add(1 + head_words(cap)).and_then(|n| Layout::array::<u64>(n).ok())
    }

    /// Writes the head of a buffer of `cap` slots allocated at `base`: its cards (all dirty, as
    /// a new buffer's range covers everything) and its range word. Returns the buffer.
    ///
    /// # Safety
    /// `base` must be valid for writes of `8 * head_words(cap)` bytes and 8-aligned.
    unsafe fn init_head(base: *mut u8, cap: usize) -> *mut WordBuf {
        let cw = card_words(cap);
        std::ptr::write_bytes(base, 0xff, 8 * cw);
        let n = cap.div_ceil(CARD);
        if cw > 0 && n % 64 != 0 {
            // No bits past the last card.
            *(base as *mut u64).add(cw - 1) = (1u64 << (n % 64)) - 1;
        }
        let pre = base.add(8 * cw) as *mut Prefixed;
        let word = if cw > 0 { CARDED } else { WRITTEN_ALL };
        std::ptr::addr_of_mut!((*pre).written).write(AtomicU64::new(word));
        std::ptr::addr_of_mut!((*pre).buf)
    }

    /// The card table of `p`, a live buffer this module allocated: one bit per [`CARD`] slots,
    /// card `c` being bit `c % 64` of word `c / 64`; empty below [`CARD_MIN`]. Only the
    /// container's writer (its owner, or a shared container's lock holder) and its owner's minor
    /// collections touch it, one at a time, so a load and a store set a bit.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`].
    #[inline]
    unsafe fn cards<'a>(p: *const WordBuf) -> &'a [AtomicU64] {
        let cw = card_words((*p).cap);
        let base = (p as *const u8).sub(8 * (PREFIX + cw)) as *const AtomicU64;
        std::slice::from_raw_parts(base, cw)
    }

    /// Marks cards `lo..hi` dirty.
    fn set_cards(cards: &[AtomicU64], lo: usize, hi: usize) {
        for c in lo..hi {
            let w = &cards[c / 64];
            w.store(w.load(Ordering::Relaxed) | 1 << (c % 64), Ordering::Relaxed);
        }
    }

    /// The shared empty buffer.
    pub fn empty() -> *mut WordBuf {
        // Derived from the whole static, so the word in front stays in bounds.
        unsafe { (&EMPTY as *const Prefixed as *mut u8).add(8 * PREFIX) as *mut WordBuf }
    }

    /// A new buffer of `cap` slots, each holding `fill`.
    pub fn alloc(cap: usize, fill: u64) -> *mut WordBuf {
        Self::alloc_from(cap, std::iter::empty(), fill)
    }

    /// A new buffer of `cap` slots holding the words of `prefix` (at most `cap` of them) and
    /// then `fill`, written in one pass.
    pub fn alloc_from(cap: usize, prefix: impl IntoIterator<Item = u64>, fill: u64) -> *mut WordBuf {
        Self::try_alloc_from(cap, prefix, fill).unwrap_or_else(|layout| std::alloc::handle_alloc_error(layout))
    }

    /// As [`WordBuf::alloc`], but returns the layout the allocator refused instead of aborting.
    pub fn try_alloc(cap: usize, fill: u64) -> Result<*mut WordBuf, Layout> {
        Self::try_alloc_from(cap, std::iter::empty(), fill)
    }

    /// As [`WordBuf::alloc_from`], but returns the layout the allocator refused instead of
    /// aborting.
    pub fn try_alloc_from(
        cap: usize,
        prefix: impl IntoIterator<Item = u64>,
        fill: u64,
    ) -> Result<*mut WordBuf, Layout> {
        if cap == 0 {
            return Ok(Self::empty());
        }
        // A size past `isize::MAX` is refused like any other.
        let layout =
            Self::try_layout(cap).ok_or_else(|| Layout::from_size_align(isize::MAX as usize & !7, 8).unwrap())?;
        unsafe {
            let base = std::alloc::alloc(layout);
            if base.is_null() {
                return Err(layout);
            }
            let p = Self::init_head(base, cap);
            Self::init_in_place(p, cap, prefix, fill);
            crate::note_external(cap * 8);
            Ok(p)
        }
    }

    /// Writes a buffer of `cap` slots holding the words of `prefix` (at most `cap` of them) and
    /// then `fill` at `p`, which is not allocated by this module: containers use it for a
    /// buffer embedded in their own heap object. Such a buffer must never be passed to
    /// [`WordBuf::free`] or [`release`], and it has no written range: it must not be passed to
    /// [`WordBuf::note_written`] or the functions that follow it.
    ///
    /// # Safety
    /// `p` must be valid for writes of `8 * (cap + 1)` bytes, 8-aligned, and unpublished.
    pub unsafe fn init_in_place(p: *mut WordBuf, cap: usize, prefix: impl IntoIterator<Item = u64>, fill: u64) {
        std::ptr::addr_of_mut!((*p).cap).write(cap);
        let slots = Self::slots_ptr(p);
        let mut n = 0;
        for w in prefix.into_iter().take(cap) {
            slots.add(n).write(AtomicU64::new(w));
            n += 1;
        }
        for i in n..cap {
            slots.add(i).write(AtomicU64::new(fill));
        }
    }

    /// A buffer holding `items` (with the vector's capacity, spare slots holding `fill`) that
    /// takes over the vector's allocation instead of copying it: the allocation is grown by the
    /// header words and the items are moved up within it, so building a large
    /// container from a vector does not need memory for two copies.
    pub fn from_vec(items: Vec<u64>, fill: u64) -> *mut WordBuf {
        Self::try_from_vec(items, fill).unwrap_or_else(|layout| std::alloc::handle_alloc_error(layout))
    }

    /// As [`WordBuf::from_vec`], but if the allocator refuses, frees the vector and returns
    /// the layout it refused instead of aborting.
    pub fn try_from_vec(items: Vec<u64>, fill: u64) -> Result<*mut WordBuf, Layout> {
        let mut items = std::mem::ManuallyDrop::new(items);
        let (ptr, len, cap) = (items.as_mut_ptr(), items.len(), items.capacity());
        if cap == 0 {
            return Ok(Self::empty());
        }
        unsafe {
            // `Vec<u64>` allocated `cap` words with the global allocator; `layout(cap)` has
            // the same alignment and the header words more, so `realloc` may grow it in place.
            let old = Layout::array::<u64>(cap).expect("container too large");
            // A vector's allocation is at most `isize::MAX` bytes; the header words may not fit.
            let Some(new) = Self::try_layout(cap) else {
                std::alloc::dealloc(ptr as *mut u8, old);
                return Err(old);
            };
            let base = std::alloc::realloc(ptr as *mut u8, old, new.size());
            if base.is_null() {
                // The vector's allocation is still valid; free it before reporting.
                std::alloc::dealloc(ptr as *mut u8, old);
                return Err(new);
            }
            let words = base as *mut u64;
            std::ptr::copy(words, words.add(1 + head_words(cap)), len);
            let p = Self::init_head(base, cap);
            std::ptr::addr_of_mut!((*p).cap).write(cap);
            let slots = Self::slots_ptr(p);
            for i in len..cap {
                slots.add(i).write(AtomicU64::new(fill));
            }
            crate::note_external(cap * 8);
            Ok(p)
        }
    }

    /// # Safety
    /// `p` must come from [`WordBuf::alloc`] and be unreachable by any reader.
    pub unsafe fn free(p: *mut u8) {
        let b = p as *mut WordBuf;
        let cap = (*b).cap;
        if cap > 0 {
            std::alloc::dealloc(p.sub(8 * head_words(cap)), Self::layout(cap));
        }
    }

    #[inline(always)]
    fn slots_ptr(p: *const WordBuf) -> *mut AtomicU64 {
        unsafe { (p as *mut u8).add(8) as *mut AtomicU64 }
    }

    /// # Safety
    /// `p` must be a live buffer, loaded with `Acquire` (or by its only writer); the slice must
    /// not be held across a safepoint.
    #[inline(always)]
    pub unsafe fn slots<'a>(p: *const WordBuf) -> &'a [AtomicU64] {
        std::slice::from_raw_parts(Self::slots_ptr(p), (*p).cap)
    }

    /// # Safety
    /// As for [`WordBuf::slots`].
    #[inline(always)]
    pub unsafe fn cap(p: *const WordBuf) -> usize {
        (*p).cap
    }

    // ---- written ranges ----
    //
    // A minor collection must find every reference to a young object stored in an old container
    // since the previous one. The write barrier remembers the container; these record which slots
    // were written, so that the collection scans those instead of the whole buffer.
    //
    // The range must cover every slot that may hold a young reference of the owning thread. A new
    // buffer covers everything, so a container is scanned whole the first time; a minor
    // collection that scanned the range empties it ([`WordBuf::take_written`]), since everything
    // it found is old afterwards. Only the owner writes an owned container, and minor collections
    // only scan containers their thread owns, so the word needs no ordering. It is atomic because
    // a shared container's writers (holding its lock) update it too.

    #[inline(always)]
    unsafe fn written_word<'a>(p: *const WordBuf) -> &'a AtomicU64 {
        &*(p as *const AtomicU64).sub(PREFIX)
    }

    /// The word before the capacity word: the written range of a buffer this module allocated.
    ///
    /// # Safety
    /// That word must be readable as an atomic word: `p` is a live buffer this module allocated,
    /// or one written in place after an atomic word of the same object.
    #[inline(always)]
    pub unsafe fn range_word(p: *const WordBuf) -> u64 {
        Self::written_word(p).load(Ordering::Relaxed)
    }

    /// Whether the range word `r` covers slots `lo..hi`.
    #[inline(always)]
    pub fn range_covers(r: u64, lo: usize, hi: usize) -> bool {
        lo >= (r >> 32) as usize && hi <= (r as u32) as usize
    }

    /// Records that slots `lo..hi` (`lo < hi`) were written. A load and two compares when the
    /// range already covers them, as it does for every container that is still young.
    ///
    /// # Safety
    /// `p` must be a live buffer this module allocated (not one written in place), and the
    /// caller must have write access to the container that holds it.
    #[inline(always)]
    pub unsafe fn note_written(p: *const WordBuf, lo: usize, hi: usize) {
        let r = Self::range_word(p);
        if !Self::range_covers(r, lo, hi) {
            if r == CARDED && hi == lo + 1 {
                Self::mark_card(p, lo);
            } else {
                Self::widen_written(p, lo, hi);
            }
        }
    }

    /// Marks the card of slot `i` of `p`, a buffer with cards (its range word is [`CARDED`]).
    /// A few instructions, inline: the table sits just before the range word.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`], and `i` is below the capacity.
    #[inline(always)]
    pub unsafe fn mark_card(p: *const WordBuf, i: usize) {
        let cw = (*p).cap.div_ceil(CARD).div_ceil(64);
        let c = i / CARD;
        let w = &*((p as *const u8).sub(8 * (PREFIX + cw)) as *const AtomicU64).add(c / 64);
        w.store(w.load(Ordering::Relaxed) | 1 << (c % 64), Ordering::Relaxed);
    }

    /// The slow path of [`WordBuf::note_written`]: widens the range to cover `lo..hi`.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`].
    #[cold]
    #[inline(never)]
    pub unsafe fn widen_written(p: *const WordBuf, lo: usize, hi: usize) {
        let cards = Self::cards(p);
        if !cards.is_empty() {
            let n = (*p).cap.div_ceil(CARD);
            Self::set_cards(cards, (lo / CARD).min(n), (hi.div_ceil(CARD)).min(n));
            return;
        }
        let w = Self::written_word(p);
        let r = w.load(Ordering::Relaxed);
        let (lo, hi) = (lo.min((r >> 32) as usize), hi.max((r as u32) as usize));
        let new = if hi >= u32::MAX as usize { WRITTEN_ALL } else { ((lo as u64) << 32) | hi as u64 };
        w.store(new, Ordering::Relaxed);
    }

    /// Gives `to`, a new buffer holding the contents of `from` at the same indices, the
    /// written range of `from`.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`], for both buffers.
    #[inline]
    pub unsafe fn copy_written(from: *const WordBuf, to: *const WordBuf) {
        if (*to).cap == 0 {
            return;
        }
        let (fc, tc) = (Self::cards(from), Self::cards(to));
        if tc.is_empty() && fc.is_empty() {
            Self::written_word(to).store(Self::written_word(from).load(Ordering::Relaxed), Ordering::Relaxed);
            return;
        }
        if tc.is_empty() {
            // Only a buffer as large as one with cards could have taken over its items.
            let r = Self::written(from);
            let w = if r.is_empty() { WRITTEN_NONE } else { ((r.start as u64) << 32) | r.end as u64 };
            Self::written_word(to).store(w, Ordering::Relaxed);
            return;
        }
        for c in tc {
            c.store(0, Ordering::Relaxed);
        }
        if fc.is_empty() {
            let r = Self::written(from);
            if !r.is_empty() {
                Self::widen_written(to, r.start, r.end);
            }
        } else {
            // `from` has no bits past its last card, and the slots past its capacity hold only
            // the fill, so their cards stay clear.
            for (t, f) in tc.iter().zip(fc) {
                t.store(f.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
    }

    /// Calls `f` with each run of slots written since the last call (within the capacity), in
    /// order, and forgets them.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`]; called by a minor collection of the owning thread.
    #[inline]
    pub unsafe fn take_written(p: *const WordBuf, mut f: impl FnMut(std::ops::Range<usize>)) {
        let cap = (*p).cap;
        if cap == 0 {
            return;
        }
        let cards = Self::cards(p);
        if cards.is_empty() {
            let r = Self::written(p);
            Self::written_word(p).store(WRITTEN_NONE, Ordering::Relaxed);
            if !r.is_empty() {
                f(r);
            }
            return;
        }
        // Runs of dirty cards, merged across words.
        let mut run: Option<(usize, usize)> = None;
        for (wi, w) in cards.iter().enumerate() {
            let mut bits = w.load(Ordering::Relaxed);
            if bits == 0 {
                continue;
            }
            w.store(0, Ordering::Relaxed);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                let ones = (bits >> b).trailing_ones() as usize;
                let (lo, hi) = (wi * 64 + b, wi * 64 + b + ones);
                bits = if b + ones >= 64 { 0 } else { bits & !(((1u64 << ones) - 1) << b) };
                run = match run {
                    Some((a, e)) if e == lo => Some((a, hi)),
                    Some((a, e)) => {
                        f(a * CARD..(e * CARD).min(cap));
                        Some((lo, hi))
                    }
                    None => Some((lo, hi)),
                };
            }
        }
        if let Some((a, e)) = run {
            if a * CARD < cap {
                f(a * CARD..(e * CARD).min(cap));
            }
        }
    }

    /// The slots written since the last [`WordBuf::take_written`] (within the capacity): with
    /// cards, the span from the first dirty card to the end of the last. For tests.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`].
    #[inline]
    pub unsafe fn written(p: *const WordBuf) -> std::ops::Range<usize> {
        let cap = (*p).cap;
        if cap == 0 {
            return 0..0;
        }
        let cards = Self::cards(p);
        if !cards.is_empty() {
            let n = cap.div_ceil(CARD);
            let dirty = |c: &usize| cards[c / 64].load(Ordering::Relaxed) & (1 << (c % 64)) != 0;
            return match ((0..n).find(dirty), (0..n).rev().find(dirty)) {
                (Some(a), Some(b)) => a * CARD..((b + 1) * CARD).min(cap),
                _ => 0..0,
            };
        }
        let r = Self::range_word(p);
        let hi = ((r as u32) as usize).min(cap);
        ((r >> 32) as usize).min(hi)..hi
    }
}

/// Disposes of a buffer that was just replaced. With local access no other thread can be
/// reading it, so it is freed now; otherwise the collector frees it at the next stop-the-world.
///
/// # Safety
/// `old` must have been unpublished (replaced) by the caller, its only writer.
pub unsafe fn release(old: *mut WordBuf, access: Access) {
    if WordBuf::cap(old) == 0 {
        return;
    }
    if access == Access::Local && !PROMOTE_ON_ACCESS.load(Ordering::Relaxed) {
        WordBuf::free(old as *mut u8);
    } else {
        catallaxy_gc::retire(old as *mut u8, WordBuf::free);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_buffers_are_reported() {
        // Sizes that do not fit an allocation at all. (A size the allocator refuses is covered
        // by the out-of-memory tests of the `catallaxy` crate: Miri and the sanitizers stop at
        // such an attempt instead of returning null.)
        for cap in [usize::MAX / 8, usize::MAX] {
            assert!(WordBuf::try_alloc(cap, 0).is_err(), "{cap}");
        }
        let b = WordBuf::try_alloc_from(4, [1, 2], 9).unwrap();
        // SAFETY: just allocated, never published.
        unsafe {
            let words: Vec<u64> = WordBuf::slots(b).iter().map(|s| s.load(Ordering::Relaxed)).collect();
            assert_eq!(words, [1, 2, 9, 9]);
            WordBuf::free(b as *mut u8);
        }
    }
}
