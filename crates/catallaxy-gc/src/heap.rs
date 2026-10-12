//! Segmented heap with size-class pages and thread-local allocation.
//!
//! The heap is a set of 256 KB segments, each split into eight 32 KB pages. A page serves one size
//! class. A mutator thread owns one current page per size class and allocates from it without
//! atomics (bump allocation on fresh pages, a free list on swept pages). Only taking a new page
//! touches the per-size-class global lists. Objects larger than the biggest size class get their own
//! allocation. Pages are swept lazily: after a collection every page is "unswept", and a thread sweeps
//! a page right before allocating from it.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::header::{Header, HEADER_SIZE};
use crate::stats;

pub const SEGMENT_SIZE: usize = 256 * 1024;
pub const PAGE_SIZE: usize = 32 * 1024;
pub const PAGES_PER_SEGMENT: usize = SEGMENT_SIZE / PAGE_SIZE;
pub const MAX_SMALL: usize = 2048;

/// Slot sizes: 16-byte steps up to 512, then coarser steps up to 2048.
pub const SIZE_CLASSES: [usize; 40] = [
    16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 272, 288, 304, 320, 336,
    352, 368, 384, 400, 416, 432, 448, 464, 480, 496, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048,
];
pub const NUM_CLASSES: usize = SIZE_CLASSES.len();

#[inline]
pub fn size_class_of(size: usize) -> usize {
    if size <= 512 {
        (size.max(16) + 15) / 16 - 1
    } else {
        32 + SIZE_CLASSES[32..].iter().position(|&s| s >= size).expect("size within small range")
    }
}

/// Page lifecycle, used only for debugging assertions and statistics.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum PageState {
    Empty = 0,
    Current = 1,
    Unswept = 2,
    Full = 3,
}

pub(crate) struct Page {
    pub base: *mut u8,
    slot_size: UnsafeCell<usize>,
    class: UnsafeCell<usize>,
    pub state: AtomicU8,
}

// SAFETY: a page's mutable fields are touched only by the thread that currently owns it (via the
// global lists' mutexes, which give happens-before) or by the collector while the world is stopped.
unsafe impl Send for Page {}
unsafe impl Sync for Page {}

impl Page {
    fn slot_size(&self) -> usize {
        unsafe { *self.slot_size.get() }
    }
    fn n_slots(&self) -> usize {
        PAGE_SIZE / self.slot_size()
    }

    /// Frees every allocated slot not marked with `live_color`, finalizing it. Returns the head of
    /// a free list threaded through free slots and the number of free slots.
    ///
    /// # Safety
    /// The caller must own the page exclusively.
    unsafe fn sweep(&self, live_color: u8) -> (*mut u8, usize) {
        let size = self.slot_size();
        let mut head: *mut u8 = ptr::null_mut();
        let mut free = 0;
        let mut freed_bytes = 0;
        // Build the list back to front so allocation proceeds in address order.
        for i in (0..self.n_slots()).rev() {
            let slot = self.base.add(i * size);
            let h = &*(slot as *const Header);
            if !h.is_free() {
                if h.color() == live_color {
                    continue;
                }
                if let Some(fin) = (*h.vtable()).finalize {
                    fin(slot as *mut Header);
                }
                h.clear_vtable();
                freed_bytes += size;
            }
            // Free slot: word 0 is zero, word 1 holds the next pointer.
            *(slot.add(8) as *mut *mut u8) = head;
            head = slot;
            free += 1;
        }
        stats::note_freed(freed_bytes);
        (head, free)
    }
}

struct Segment {
    /// Kept so segments can be returned to the OS once empty (not yet done in v1).
    #[allow(dead_code)]
    base: *mut u8,
}
unsafe impl Send for Segment {}

fn segment_layout() -> Layout {
    Layout::from_size_align(SEGMENT_SIZE, SEGMENT_SIZE).unwrap()
}

/// Global page pools.
pub(crate) struct PagePool {
    segments: Mutex<Vec<Segment>>,
    empty: Mutex<Vec<&'static Page>>,
    unswept: Vec<Mutex<Vec<&'static Page>>>,
    full: Vec<Mutex<Vec<&'static Page>>>,
    pub large: Mutex<Vec<LargeObj>>,
    pub live_color: AtomicU8,
}

pub(crate) struct LargeObj {
    pub ptr: *mut u8,
    pub layout: Layout,
}
unsafe impl Send for LargeObj {}

pub(crate) static POOL: std::sync::LazyLock<PagePool> = std::sync::LazyLock::new(|| PagePool {
    segments: Mutex::new(Vec::new()),
    empty: Mutex::new(Vec::new()),
    unswept: (0..NUM_CLASSES).map(|_| Mutex::new(Vec::new())).collect(),
    full: (0..NUM_CLASSES).map(|_| Mutex::new(Vec::new())).collect(),
    large: Mutex::new(Vec::new()),
    live_color: AtomicU8::new(1),
});

static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);

impl PagePool {
    fn new_segment(&self) {
        // SAFETY: the layout has nonzero size.
        let base = unsafe { alloc_zeroed(segment_layout()) };
        if base.is_null() {
            std::alloc::handle_alloc_error(segment_layout());
        }
        self.segments.lock().push(Segment { base });
        let mut empty = self.empty.lock();
        for i in (0..PAGES_PER_SEGMENT).rev() {
            let page: &'static Page = Box::leak(Box::new(Page {
                base: unsafe { base.add(i * PAGE_SIZE) },
                slot_size: UnsafeCell::new(0),
                class: UnsafeCell::new(0),
                state: AtomicU8::new(PageState::Empty as u8),
            }));
            empty.push(page);
        }
        TOTAL_PAGES.fetch_add(PAGES_PER_SEGMENT, Ordering::Relaxed);
    }

    fn take_empty(&self, class: usize) -> &'static Page {
        loop {
            if let Some(p) = self.empty.lock().pop() {
                unsafe {
                    *p.slot_size.get() = SIZE_CLASSES[class];
                    *p.class.get() = class;
                }
                p.state.store(PageState::Current as u8, Ordering::Relaxed);
                return p;
            }
            self.new_segment();
        }
    }

    /// Moves every page that holds objects to the unswept lists. Called by the collector after
    /// marking, with the world stopped.
    pub(crate) fn mark_all_unswept(&self) {
        for c in 0..NUM_CLASSES {
            let mut full = self.full[c].lock();
            let mut uns = self.unswept[c].lock();
            for p in full.drain(..) {
                p.state.store(PageState::Unswept as u8, Ordering::Relaxed);
                uns.push(p);
            }
        }
    }

    /// Sweeps every unswept page. Called at the start of a collection, with the world stopped, so
    /// that mark colors from the previous cycle are fully consumed before they are reused.
    pub(crate) fn finish_sweeping(&self) {
        let color = self.live_color.load(Ordering::Relaxed);
        for c in 0..NUM_CLASSES {
            let pages: Vec<&'static Page> = std::mem::take(&mut *self.unswept[c].lock());
            let mut full = self.full[c].lock();
            let mut empty = self.empty.lock();
            for p in pages {
                let (_, free) = unsafe { p.sweep(color) };
                if free == p.n_slots() {
                    p.state.store(PageState::Empty as u8, Ordering::Relaxed);
                    empty.push(p);
                } else {
                    p.state.store(PageState::Full as u8, Ordering::Relaxed);
                    full.push(p);
                }
            }
        }
    }

    /// Frees unmarked large objects. World stopped.
    pub(crate) fn sweep_large(&self, live_color: u8) {
        let mut large = self.large.lock();
        let mut freed = 0;
        large.retain(|obj| {
            let h = unsafe { &*(obj.ptr as *const Header) };
            if h.color() == live_color {
                true
            } else {
                unsafe {
                    if let Some(fin) = (*h.vtable()).finalize {
                        fin(obj.ptr as *mut Header);
                    }
                    dealloc(obj.ptr, obj.layout);
                }
                freed += obj.layout.size();
                false
            }
        });
        stats::note_freed(freed);
    }
}

/// A thread's allocation state: one current page per size class.
pub struct LocalHeap {
    current: [*const Page; NUM_CLASSES],
    free: [*mut u8; NUM_CLASSES],
    bump: [*mut u8; NUM_CLASSES],
    end: [*mut u8; NUM_CLASSES],
    owner: u16,
}

unsafe impl Send for LocalHeap {}

impl LocalHeap {
    pub fn new(owner: u16) -> Self {
        LocalHeap {
            current: [ptr::null(); NUM_CLASSES],
            free: [ptr::null_mut(); NUM_CLASSES],
            bump: [ptr::null_mut(); NUM_CLASSES],
            end: [ptr::null_mut(); NUM_CLASSES],
            owner,
        }
    }

    pub fn owner(&self) -> u16 {
        self.owner
    }

    pub fn set_owner(&mut self, owner: u16) {
        self.owner = owner;
    }

    /// Allocates `size` bytes (header included), zeroed, with an initialized header owned by this
    /// thread. Never runs a collection; it may request one.
    #[inline]
    pub fn alloc(&mut self, size: usize, vtable: *const crate::GcVTable) -> *mut Header {
        debug_assert!(size >= HEADER_SIZE);
        if size > MAX_SMALL {
            return self.alloc_large(size, vtable);
        }
        let class = size_class_of(size);
        let slot_size = SIZE_CLASSES[class];
        let slot = {
            let f = self.free[class];
            if !f.is_null() {
                // SAFETY: free-list slots live in a page this thread owns; word 1 holds the next link.
                unsafe {
                    self.free[class] = *(f.add(8) as *const *mut u8);
                    ptr::write_bytes(f, 0, slot_size);
                }
                f
            } else {
                let b = self.bump[class];
                if !b.is_null() && (b as usize) + slot_size <= self.end[class] as usize {
                    self.bump[class] = unsafe { b.add(slot_size) };
                    b
                } else {
                    self.refill(class)
                }
            }
        };
        let h = slot as *mut Header;
        unsafe { Header::init(h, vtable, self.owner, POOL.live_color.load(Ordering::Relaxed)) };
        h
    }

    #[cold]
    fn refill(&mut self, class: usize) -> *mut u8 {
        let slot_size = SIZE_CLASSES[class];
        if !self.current[class].is_null() {
            let p = unsafe { &*self.current[class] };
            p.state.store(PageState::Full as u8, Ordering::Relaxed);
            POOL.full[class].lock().push(p);
            self.current[class] = ptr::null();
            self.bump[class] = ptr::null_mut();
            self.end[class] = ptr::null_mut();
        }
        // Prefer reusing swept space.
        loop {
            let next = POOL.unswept[class].lock().pop();
            let Some(p) = next else { break };
            let color = POOL.live_color.load(Ordering::Relaxed);
            let (head, free) = unsafe { p.sweep(color) };
            if head.is_null() {
                p.state.store(PageState::Full as u8, Ordering::Relaxed);
                POOL.full[class].lock().push(p);
                continue;
            }
            stats::note_allocated(free * slot_size);
            p.state.store(PageState::Current as u8, Ordering::Relaxed);
            self.current[class] = p;
            unsafe {
                self.free[class] = *(head.add(8) as *const *mut u8);
                ptr::write_bytes(head, 0, slot_size);
            }
            return head;
        }
        let p = POOL.take_empty(class);
        stats::note_allocated(PAGE_SIZE);
        self.current[class] = p;
        unsafe {
            // Empty pages may hold stale headers from a previous life; a stale header would be
            // finalized twice by a later sweep, so the whole page is zeroed.
            ptr::write_bytes(p.base, 0, PAGE_SIZE);
            self.bump[class] = p.base.add(slot_size);
            self.end[class] = p.base.add(PAGE_SIZE - PAGE_SIZE % slot_size);
        }
        p.base
    }

    #[cold]
    fn alloc_large(&mut self, size: usize, vtable: *const crate::GcVTable) -> *mut Header {
        let layout = Layout::from_size_align((size + 15) & !15, 16).unwrap();
        let p = unsafe { alloc_zeroed(layout) };
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        stats::note_allocated(layout.size());
        let h = p as *mut Header;
        unsafe { Header::init(h, vtable, self.owner, POOL.live_color.load(Ordering::Relaxed)) };
        POOL.large.lock().push(LargeObj { ptr: p, layout });
        h
    }

    /// Returns all current pages to the global lists (before a collection or on thread exit).
    pub fn release_pages(&mut self) {
        for class in 0..NUM_CLASSES {
            if self.current[class].is_null() {
                continue;
            }
            let p = unsafe { &*self.current[class] };
            // Slots never handed out stay zeroed (free); slots on the free list have word 0 == 0.
            // Both are recognized as free by the next sweep.
            p.state.store(PageState::Full as u8, Ordering::Relaxed);
            POOL.full[class].lock().push(p);
            self.current[class] = ptr::null();
            self.free[class] = ptr::null_mut();
            self.bump[class] = ptr::null_mut();
            self.end[class] = ptr::null_mut();
        }
    }
}

impl Drop for LocalHeap {
    fn drop(&mut self) {
        self.release_pages();
    }
}

pub fn total_heap_bytes() -> usize {
    TOTAL_PAGES.load(Ordering::Relaxed) * PAGE_SIZE
}
