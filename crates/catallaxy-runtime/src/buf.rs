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

use std::alloc::Layout;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ownership::{Access, PROMOTE_ON_ACCESS};

/// A capacity word followed by `cap` atomic slots.
///
/// The capacity is the low half of the word. A buffer embedded in a container's own object may
/// keep a tag in the high half (a list's element type, see [`WordBuf::tag`]); buffers this module
/// allocates have none.
#[repr(C)]
pub struct WordBuf {
    cap: AtomicU64,
    slots: [AtomicU64; 0],
}

/// The capacity bits of the capacity word.
const CAP_MASK: u64 = u32::MAX as u64;

/// A buffer with its written-range word in front, as this module allocates them.
#[repr(C)]
struct Prefixed {
    written: AtomicU64,
    buf: WordBuf,
}

/// Words in front of the capacity word of a buffer this module allocates.
const PREFIX: usize = 1;

/// Written-range words: the high half is the first slot written, the low half one past the last.
/// (So a plain count `n` below 2^32 reads as the range `0..n`; see [`WordBuf::range_covers`].)
/// Every slot, including any index too large for the encoding.
const WRITTEN_ALL: u64 = u32::MAX as u64;
/// No slot.
const WRITTEN_NONE: u64 = (u32::MAX as u64) << 32;

/// The buffer of every container with no capacity. Never written, never freed.
static EMPTY: Prefixed =
    Prefixed { written: AtomicU64::new(WRITTEN_ALL), buf: WordBuf { cap: AtomicU64::new(0), slots: [] } };

impl WordBuf {
    fn layout(cap: usize) -> Layout {
        Layout::from_size_align(8 * (cap + 1 + PREFIX), 8).expect("container too large")
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
        // A size past `isize::MAX`, or a capacity that does not fit the capacity bits, is
        // refused like any other.
        let layout = Some(cap)
            .filter(|&c| c as u64 <= CAP_MASK)
            .and_then(|c| c.checked_add(1 + PREFIX))
            .and_then(|n| Layout::array::<u64>(n).ok())
            .ok_or_else(|| Layout::from_size_align(isize::MAX as usize & !7, 8).unwrap())?;
        if !crate::cap_allows(layout.size()) {
            return Err(layout);
        }
        unsafe {
            let base = std::alloc::alloc(layout) as *mut Prefixed;
            if base.is_null() {
                crate::allocator_refused();
                return Err(layout);
            }
            std::ptr::addr_of_mut!((*base).written).write(AtomicU64::new(WRITTEN_ALL));
            let p = std::ptr::addr_of_mut!((*base).buf);
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
        debug_assert!(cap as u64 <= CAP_MASK);
        std::ptr::addr_of_mut!((*p).cap).write(AtomicU64::new(cap as u64));
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
            let fits = cap as u64 <= CAP_MASK;
            let Some(new) = cap.checked_add(1 + PREFIX).and_then(|n| Layout::array::<u64>(n).ok()).filter(|_| fits)
            else {
                std::alloc::dealloc(ptr as *mut u8, old);
                return Err(old);
            };
            // The vector's own bytes are not counted yet either.
            if !crate::cap_allows(new.size()) {
                std::alloc::dealloc(ptr as *mut u8, old);
                return Err(new);
            }
            let base = std::alloc::realloc(ptr as *mut u8, old, new.size()) as *mut Prefixed;
            if base.is_null() {
                // The vector's allocation is still valid; free it before reporting.
                std::alloc::dealloc(ptr as *mut u8, old);
                crate::allocator_refused();
                return Err(new);
            }
            let words = base as *mut u64;
            std::ptr::copy(words, words.add(1 + PREFIX), len);
            std::ptr::addr_of_mut!((*base).written).write(AtomicU64::new(WRITTEN_ALL));
            let p = std::ptr::addr_of_mut!((*base).buf);
            std::ptr::addr_of_mut!((*p).cap).write(AtomicU64::new(cap as u64));
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
        let cap = Self::cap(b);
        if cap > 0 {
            std::alloc::dealloc(p.sub(8 * PREFIX), Self::layout(cap));
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
        std::slice::from_raw_parts(Self::slots_ptr(p), Self::cap(p))
    }

    /// # Safety
    /// As for [`WordBuf::slots`].
    #[inline(always)]
    pub unsafe fn cap(p: *const WordBuf) -> usize {
        ((*p).cap.load(Ordering::Relaxed) & CAP_MASK) as usize
    }

    /// The tag kept in the high half of an embedded buffer's capacity word (0: none).
    ///
    /// # Safety
    /// `p` must be a live buffer.
    #[inline(always)]
    pub unsafe fn tag(p: *const WordBuf) -> u32 {
        ((*p).cap.load(Ordering::Relaxed) >> 32) as u32
    }

    /// Replaces the tag `from` with `to`; false if the tag was not `from`. Readers of the
    /// capacity are not disturbed: the capacity bits are kept.
    ///
    /// # Safety
    /// `p` must be a buffer embedded in a live container object (not one this module allocated,
    /// which may be freed while the container lives).
    #[inline]
    pub unsafe fn set_tag(p: *const WordBuf, from: u32, to: u32) -> bool {
        let word = &(*p).cap;
        let mut w = word.load(Ordering::Relaxed);
        loop {
            if (w >> 32) as u32 != from {
                return false;
            }
            let new = (w & CAP_MASK) | ((to as u64) << 32);
            match word.compare_exchange_weak(w, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(cur) => w = cur,
            }
        }
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
        if !Self::range_covers(Self::range_word(p), lo, hi) {
            Self::widen_written(p, lo, hi);
        }
    }

    /// The slow path of [`WordBuf::note_written`]: widens the range to cover `lo..hi`.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`].
    #[cold]
    #[inline(never)]
    pub unsafe fn widen_written(p: *const WordBuf, lo: usize, hi: usize) {
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
        if Self::cap(to) > 0 {
            Self::written_word(to).store(Self::written_word(from).load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    /// The slots written since the last call (within the capacity), which it then forgets.
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`]; called by a minor collection of the owning thread.
    #[inline]
    pub unsafe fn take_written(p: *const WordBuf) -> std::ops::Range<usize> {
        let r = Self::written(p);
        if Self::cap(p) > 0 {
            Self::written_word(p).store(WRITTEN_NONE, Ordering::Relaxed);
        }
        r
    }

    /// The slots written since the last [`WordBuf::take_written`] (within the capacity).
    ///
    /// # Safety
    /// As for [`WordBuf::note_written`].
    #[inline]
    pub unsafe fn written(p: *const WordBuf) -> std::ops::Range<usize> {
        let cap = Self::cap(p);
        if cap == 0 {
            return 0..0;
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
