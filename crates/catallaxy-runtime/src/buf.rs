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
        if cap == 0 {
            return Self::empty();
        }
        unsafe {
            let p = std::alloc::alloc(Self::layout(cap)) as *mut WordBuf;
            if p.is_null() {
                std::alloc::handle_alloc_error(Self::layout(cap));
            }
            std::ptr::addr_of_mut!((*p).cap).write(cap);
            let slots = Self::slots_ptr(p);
            for i in 0..cap {
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
