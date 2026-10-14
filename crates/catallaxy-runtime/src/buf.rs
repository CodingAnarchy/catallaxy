//! Arrays of atomic words for containers whose readers do not lock (lists, and dicts read
//! through their seqlock). Every slot is an `AtomicU64`, so a reader racing a writer sees some
//! value that was stored there, never a torn one. A writer that outgrows a buffer copies it into
//! a new one, publishes the new pointer with `Release`, and hands the old one to [`release`],
//! which frees it only once no other thread can be reading it.

use std::alloc::Layout;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ownership::{Access, PROMOTE_ON_ACCESS};

/// A capacity word followed by `cap` atomic slots.
#[repr(C)]
pub struct WordBuf {
    cap: usize,
    slots: [AtomicU64; 0],
}

/// The buffer of every container with no capacity. Never written, never freed.
static EMPTY: WordBuf = WordBuf { cap: 0, slots: [] };

impl WordBuf {
    fn layout(cap: usize) -> Layout {
        Layout::from_size_align(8 * (cap + 1), 8).expect("container too large")
    }

    /// The shared empty buffer.
    pub fn empty() -> *mut WordBuf {
        &EMPTY as *const WordBuf as *mut WordBuf
    }

    /// A new buffer of `cap` slots, each holding `fill`.
    pub fn alloc(cap: usize, fill: u64) -> *mut WordBuf {
        Self::alloc_from(cap, std::iter::empty(), fill)
    }

    /// A new buffer of `cap` slots holding the words of `prefix` (at most `cap` of them) and
    /// then `fill`, written in one pass.
    pub fn alloc_from(cap: usize, prefix: impl IntoIterator<Item = u64>, fill: u64) -> *mut WordBuf {
        if cap == 0 {
            return Self::empty();
        }
        unsafe {
            let p = std::alloc::alloc(Self::layout(cap)) as *mut WordBuf;
            if p.is_null() {
                std::alloc::handle_alloc_error(Self::layout(cap));
            }
            Self::init_in_place(p, cap, prefix, fill);
            crate::note_external(cap * 8);
            p
        }
    }

    /// Writes a buffer of `cap` slots holding the words of `prefix` (at most `cap` of them) and
    /// then `fill` at `p`, which is not allocated by this module: containers use it for a
    /// buffer embedded in their own heap object. Such a buffer must never be passed to
    /// [`WordBuf::free`] or [`release`].
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
    /// takes over the vector's allocation instead of copying it: the allocation is grown by one
    /// word for the capacity header and the items are moved up within it, so building a large
    /// container from a vector does not need memory for two copies.
    pub fn from_vec(items: Vec<u64>, fill: u64) -> *mut WordBuf {
        let mut items = std::mem::ManuallyDrop::new(items);
        let (ptr, len, cap) = (items.as_mut_ptr(), items.len(), items.capacity());
        if cap == 0 {
            return Self::empty();
        }
        unsafe {
            // `Vec<u64>` allocated `cap` words with the global allocator; `layout(cap)` has
            // the same alignment and one more word, so `realloc` may grow it in place.
            let old = Layout::array::<u64>(cap).expect("container too large");
            let new = Self::layout(cap);
            let p = std::alloc::realloc(ptr as *mut u8, old, new.size()) as *mut WordBuf;
            if p.is_null() {
                // The vector's allocation is still valid; free it before reporting.
                std::alloc::dealloc(ptr as *mut u8, old);
                std::alloc::handle_alloc_error(new);
            }
            let words = p as *mut u64;
            std::ptr::copy(words, words.add(1), len);
            std::ptr::addr_of_mut!((*p).cap).write(cap);
            let slots = Self::slots_ptr(p);
            for i in len..cap {
                slots.add(i).write(AtomicU64::new(fill));
            }
            crate::note_external(cap * 8);
            p
        }
    }

    /// # Safety
    /// `p` must come from [`WordBuf::alloc`] and be unreachable by any reader.
    pub unsafe fn free(p: *mut u8) {
        let b = p as *mut WordBuf;
        let cap = (*b).cap;
        if cap > 0 {
            std::alloc::dealloc(p, Self::layout(cap));
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
