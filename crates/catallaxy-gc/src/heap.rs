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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::header::{Header, HEADER_SIZE, MINOR_MARK_BIT, YOUNG_BIT};
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
        size.max(16).div_ceil(16) - 1
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

    /// Minor sweep: frees `tid`'s young objects that are not minor-marked and tenures the
    /// marked ones; old objects are left alone. Returns the head of a list of every free slot
    /// (only newly freed ones if `only_new`), the number of free slots, and the bytes freed and
    /// promoted.
    ///
    /// # Safety
    /// The page must be private to the calling thread `tid`.
    unsafe fn sweep_young(&self, tid: u16, only_new: bool) -> (*mut u8, usize, usize, usize) {
        let size = self.slot_size();
        let mut head: *mut u8 = ptr::null_mut();
        let (mut free, mut freed, mut promoted) = (0, 0, 0);
        for i in (0..self.n_slots()).rev() {
            let slot = self.base.add(i * size);
            let h = &*(slot as *const Header);
            if h.is_free() {
                if only_new {
                    continue;
                }
            } else {
                let m = h.meta_relaxed();
                if m & YOUNG_BIT == 0 || Header::owner_of(m) != tid {
                    continue;
                }
                if m & MINOR_MARK_BIT != 0 {
                    promoted += ((*h.vtable()).size)(h);
                    h.tenure();
                    continue;
                }
                if let Some(fin) = (*h.vtable()).finalize {
                    fin(slot as *mut Header);
                }
                h.clear_vtable();
                freed += size;
            }
            *(slot.add(8) as *mut *mut u8) = head;
            head = slot;
            free += 1;
        }
        (head, free, freed, promoted)
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

/// Whether new objects start young (thread-local minor collection). On by default; off with
/// `CATX_GC_GENERATIONAL=0`, or when `--promote-on-access` lets threads steal owned objects.
static GENERATIONAL: std::sync::LazyLock<AtomicBool> = std::sync::LazyLock::new(|| {
    AtomicBool::new(!matches!(std::env::var("CATX_GC_GENERATIONAL").as_deref(), Ok("0")))
});

/// Turns generational collection on or off for threads registered from now on.
pub fn set_generational(on: bool) {
    GENERATIONAL.store(on, Ordering::Relaxed);
}

/// Young bytes a thread allocates before it asks for a minor collection.
fn nursery_bytes() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| if stats::stress() { 64 * 1024 } else { 4 * 1024 * 1024 })
}

/// A thread's allocation state: one current page per size class, plus its young generation:
/// the pages and large objects it filled since its last minor collection (private to it until
/// then), pages with space reclaimed by that collection, and its remembered set.
pub struct LocalHeap {
    current: [*const Page; NUM_CLASSES],
    free: [*mut u8; NUM_CLASSES],
    bump: [*mut u8; NUM_CLASSES],
    end: [*mut u8; NUM_CLASSES],
    owner: u16,
    /// Allocate young objects (generational mode, not loading a program).
    young_alloc: bool,
    generational: bool,
    young_pages: Vec<&'static Page>,
    partial: Vec<Vec<(&'static Page, *mut u8)>>,
    young_large: Vec<LargeObj>,
    remset: Vec<*const Header>,
    young_bytes: usize,
    /// Set when the nursery is full; the thread collects at its next safepoint.
    pub(crate) minor_wanted: bool,
    /// The owning thread's poll flag, raised to reach that safepoint.
    poll: *const AtomicBool,
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
            young_alloc: GENERATIONAL.load(Ordering::Relaxed),
            generational: GENERATIONAL.load(Ordering::Relaxed),
            young_pages: Vec::new(),
            partial: (0..NUM_CLASSES).map(|_| Vec::new()).collect(),
            young_large: Vec::new(),
            remset: Vec::new(),
            young_bytes: 0,
            minor_wanted: false,
            poll: ptr::null(),
        }
    }

    pub(crate) fn set_poll(&mut self, poll: *const AtomicBool) {
        self.poll = poll;
    }

    /// Whether new objects start young. Program loading turns this off, because loaded constants
    /// are reachable from global roots, which a minor collection does not scan.
    pub fn set_young_alloc(&mut self, on: bool) {
        self.young_alloc = on && self.generational;
    }

    pub fn is_generational(&self) -> bool {
        self.generational
    }

    /// Write barrier: records an old object that may now point to young ones. Call when a
    /// thread writes into an object it owns whose metadata satisfies
    /// [`Header::needs_remember`].
    #[inline]
    pub fn remember(&mut self, h: *const Header) {
        if self.generational && unsafe { (*h).set_remembered() } {
            self.remset.push(h);
        }
    }

    fn note_young(&mut self, bytes: usize) {
        if !self.young_alloc {
            stats::note_allocated(bytes);
            return;
        }
        self.young_bytes += bytes;
        if self.young_bytes >= nursery_bytes() && !self.minor_wanted {
            self.minor_wanted = true;
            if !self.poll.is_null() {
                unsafe { (*self.poll).store(true, Ordering::Release) };
            }
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
    /// `vtable` must point to a `GcVTable` that outlives the object (in practice, a class).
    #[inline]
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
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
        unsafe { Header::init(h, vtable, self.owner, POOL.live_color.load(Ordering::Relaxed), self.young_alloc) };
        h
    }

    #[cold]
    fn refill(&mut self, class: usize) -> *mut u8 {
        let slot_size = SIZE_CLASSES[class];
        if !self.current[class].is_null() {
            let p = unsafe { &*self.current[class] };
            p.state.store(PageState::Full as u8, Ordering::Relaxed);
            if self.generational {
                // May hold young objects: private until the next minor collection.
                self.young_pages.push(p);
            } else {
                POOL.full[class].lock().push(p);
            }
            self.current[class] = ptr::null();
            self.bump[class] = ptr::null_mut();
            self.end[class] = ptr::null_mut();
        }
        // Space a minor collection reclaimed in this thread's pages comes first.
        if let Some((p, head)) = self.partial[class].pop() {
            let free = unsafe { count_list(head) };
            self.note_young(free * slot_size);
            p.state.store(PageState::Current as u8, Ordering::Relaxed);
            self.current[class] = p;
            unsafe {
                self.free[class] = *(head.add(8) as *const *mut u8);
                ptr::write_bytes(head, 0, slot_size);
            }
            return head;
        }
        // Then swept space from the global pool.
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
            self.note_young(free * slot_size);
            p.state.store(PageState::Current as u8, Ordering::Relaxed);
            self.current[class] = p;
            unsafe {
                self.free[class] = *(head.add(8) as *const *mut u8);
                ptr::write_bytes(head, 0, slot_size);
            }
            return head;
        }
        let p = POOL.take_empty(class);
        self.note_young(PAGE_SIZE);
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
        let h = p as *mut Header;
        unsafe { Header::init(h, vtable, self.owner, POOL.live_color.load(Ordering::Relaxed), self.young_alloc) };
        if self.young_alloc {
            self.young_large.push(LargeObj { ptr: p, layout });
        } else {
            POOL.large.lock().push(LargeObj { ptr: p, layout });
        }
        self.note_young(layout.size());
        h
    }

    /// Minor collection, part 2: with every young object reachable from the thread's roots and
    /// remembered set minor-marked, frees the rest and tenures the survivors. Returns (bytes
    /// freed, bytes promoted).
    ///
    /// # Safety
    /// Called by the owning thread at a safepoint, after marking.
    pub(crate) unsafe fn sweep_young(&mut self) -> (usize, usize) {
        let tid = self.owner;
        let (mut freed, mut promoted) = (0, 0);
        // Pages filled since the last minor collection.
        for p in std::mem::take(&mut self.young_pages) {
            let (head, free, f, pr) = p.sweep_young(tid, false);
            freed += f;
            promoted += pr;
            let class = *p.class.get();
            if free == p.n_slots() {
                p.state.store(PageState::Empty as u8, Ordering::Relaxed);
                POOL.empty.lock().push(p);
            } else if !head.is_null() {
                self.partial[class].push((p, head));
            } else {
                POOL.full[class].lock().push(p);
            }
        }
        // Pages still being allocated from: newly freed slots join the free list.
        for class in 0..NUM_CLASSES {
            if self.current[class].is_null() {
                continue;
            }
            let p = &*self.current[class];
            let (mut head, _, f, pr) = p.sweep_young(tid, true);
            freed += f;
            promoted += pr;
            while !head.is_null() {
                let next = *(head.add(8) as *const *mut u8);
                *(head.add(8) as *mut *mut u8) = self.free[class];
                self.free[class] = head;
                head = next;
            }
        }
        // Large objects.
        let mut large = POOL.large.lock();
        for obj in std::mem::take(&mut self.young_large) {
            let h = &*(obj.ptr as *const Header);
            if h.meta_relaxed() & MINOR_MARK_BIT != 0 {
                h.tenure();
                promoted += obj.layout.size();
                large.push(obj);
            } else {
                if let Some(fin) = (*h.vtable()).finalize {
                    fin(obj.ptr as *mut Header);
                }
                dealloc(obj.ptr, obj.layout);
                freed += obj.layout.size();
            }
        }
        drop(large);
        for &o in &self.remset {
            (*o).clear_remembered();
        }
        self.remset.clear();
        self.young_bytes = 0;
        self.minor_wanted = false;
        // Survivors are old now: they count toward the next major collection.
        stats::note_allocated(promoted);
        (freed, promoted)
    }

    /// The remembered set, for marking.
    pub(crate) fn remembered(&self) -> &[*const Header] {
        &self.remset
    }

    /// Returns all current pages to the global lists (before a collection or on thread exit).
    pub fn release_pages(&mut self) {
        // The young generation becomes part of the old one: a major collection marks
        // (and tenures) everything live, and lazily sweeps everything else.
        for p in self.young_pages.drain(..) {
            let class = unsafe { *p.class.get() };
            POOL.full[class].lock().push(p);
        }
        for class in 0..NUM_CLASSES {
            for (p, _) in self.partial[class].drain(..) {
                p.state.store(PageState::Full as u8, Ordering::Relaxed);
                POOL.full[class].lock().push(p);
            }
        }
        POOL.large.lock().append(&mut self.young_large);
        for &o in &self.remset {
            unsafe { (*o).clear_remembered() };
        }
        self.remset.clear();
        self.young_bytes = 0;
        self.minor_wanted = false;
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

/// Length of a free list threaded through word 1 of free slots.
unsafe fn count_list(mut p: *mut u8) -> usize {
    let mut n = 0;
    while !p.is_null() {
        n += 1;
        p = *(p.add(8) as *const *mut u8);
    }
    n
}

pub fn total_heap_bytes() -> usize {
    TOTAL_PAGES.load(Ordering::Relaxed) * PAGE_SIZE
}
