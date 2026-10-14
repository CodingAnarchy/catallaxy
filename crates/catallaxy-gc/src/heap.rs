//! Segmented heap with size-class pages and thread-local allocation.
//!
//! The heap is a set of 256 KB segments, each split into eight 32 KB pages. A page serves one size
//! class. A mutator thread owns one current page per size class and allocates from it without
//! atomics (bump allocation on fresh pages, a free list on swept pages). Only taking a new page
//! touches the per-size-class global lists. Objects larger than the biggest size class get their own
//! allocation. Pages are swept lazily: after a collection every page is "unswept", and a thread sweeps
//! a page right before allocating from it. Segments are mapped from the OS directly, and a major
//! collection returns wholly empty ones beyond a reserve for the next allocation budget.
//!
//! Running out of memory: allocation cannot fail or collect, so the pool keeps an emergency
//! reserve of mapped segments. When the OS refuses a new segment, a reserve segment takes its
//! place, the headroom block goes back to the OS (see [`release_headroom`]) and a major
//! collection is requested. That collection refills the reserve, first from segments it emptied,
//! then from the OS; if it cannot, it leaves a pending `MemoryError` that the next thread to
//! reach a safepoint raises (see `limits::take_pending_error`). Only an allocation that finds the
//! reserve used up aborts. A large object the allocator refuses is retried after handing reserve
//! segments back to the OS, the same way, if it is small enough; callers that can raise an error
//! where they allocate use [`LocalHeap::try_alloc_uninit`] instead.

use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::header::{Header, HEADER_SIZE, MINOR_MARK_BIT, YOUNG_BIT};
use crate::stats;

pub const SEGMENT_SIZE: usize = 256 * 1024;
pub const PAGE_SIZE: usize = 32 * 1024;
pub const PAGES_PER_SEGMENT: usize = SEGMENT_SIZE / PAGE_SIZE;
pub const MAX_SMALL: usize = 2048;

/// Slot sizes: 16-byte steps up to 512, then coarser steps up to 2048.
pub const SIZE_CLASSES: [usize; 40] = [
    16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 272, 288, 304, 320, 336, 352, 368, 384,
    400, 416, 432, 448, 464, 480, 496, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048,
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
    /// Base of the segment holding the page.
    seg: *mut u8,
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
    base: *mut u8,
    /// The segment's page descriptors, as allocated (null in a reserve segment). Pages are
    /// shared as `&'static Page`; these pointers are kept to free them.
    pages: [*mut Page; PAGES_PER_SEGMENT],
}
unsafe impl Send for Segment {}

impl Segment {
    fn unused(base: *mut u8) -> Segment {
        Segment { base, pages: [ptr::null_mut(); PAGES_PER_SEGMENT] }
    }
}

/// Segments kept mapped for when the OS refuses more (see the module docs): 4 MB, untouched.
const RESERVE_SEGMENTS: usize = 16;

/// The headroom block: memory mapped (never touched) at startup and handed back to the OS when
/// an allocation is refused, so that the runtime's own small allocations while it reports the
/// error (the message, the trace, unwinding, a collection) still succeed. 2 MB covers the C
/// allocator growing its heap (glibc falls back to 1 MB mappings).
const HEADROOM: usize = 2 << 20;
static HEADROOM_BLOCK: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());

/// Hands the headroom block back to the OS, if it is still held, and returns whether it was.
/// Called where an allocation is refused, before reporting or retrying it; the next major
/// collection maps it again, and if it cannot, the heap is out of memory. Takes no locks and
/// does not allocate, so the global allocator can call it.
pub fn release_headroom() -> bool {
    let p = HEADROOM_BLOCK.swap(ptr::null_mut(), Ordering::AcqRel);
    if p.is_null() {
        return false;
    }
    // SAFETY: the block came from `os::map` and was never used.
    unsafe { os::unmap(p, HEADROOM) };
    // Collect soon: that restores the block, or finds the heap out of memory.
    stats::force_collection();
    true
}

/// Whether the headroom block is held.
pub(crate) fn headroom_held() -> bool {
    !HEADROOM_BLOCK.load(Ordering::Acquire).is_null()
}

/// Maps the headroom block if it is not held (at startup, and in collections). Returns whether
/// it is held now.
pub(crate) fn restore_headroom() -> bool {
    if headroom_held() {
        return true;
    }
    let p = os::map(HEADROOM);
    if p.is_null() {
        return false;
    }
    if HEADROOM_BLOCK.compare_exchange(ptr::null_mut(), p, Ordering::AcqRel, Ordering::Acquire).is_err() {
        // SAFETY: just mapped, never used.
        unsafe { os::unmap(p, HEADROOM) };
    }
    true
}

/// The global allocator for programs that embed the runtime (`catx` uses it): the system
/// allocator, except that a refused request releases the headroom block and is retried once.
/// The refusal also schedules a collection, which raises a `MemoryError` at a safepoint if
/// memory does not come back (see the module docs). So allocations the runtime does not check
/// itself, in the standard library's collections, get one more chance before the process aborts.
pub struct Allocator;

// SAFETY: every call is forwarded to the system allocator with the same arguments.
unsafe impl std::alloc::GlobalAlloc for Allocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = std::alloc::System.alloc(layout);
        if p.is_null() && release_headroom() {
            return std::alloc::System.alloc(layout);
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = std::alloc::System.alloc_zeroed(layout);
        if p.is_null() && release_headroom() {
            return std::alloc::System.alloc_zeroed(layout);
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        std::alloc::System.dealloc(p, layout)
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = std::alloc::System.realloc(p, layout, new_size);
        if q.is_null() && release_headroom() {
            return std::alloc::System.realloc(p, layout, new_size);
        }
        q
    }
}

/// Makes segment mapping fail on the calling thread, for tests of the reserve.
#[cfg(test)]
pub(crate) mod fail {
    use std::cell::Cell;
    thread_local! {
        pub(crate) static MAPS: Cell<bool> = const { Cell::new(false) };
    }
}

/// Maps a segment, or returns null if the OS refuses.
fn map_segment() -> *mut u8 {
    #[cfg(test)]
    if fail::MAPS.with(|f| f.get()) {
        return ptr::null_mut();
    }
    os::map(SEGMENT_SIZE)
}

/// Segment memory comes straight from the OS, so that a segment can be handed back whole.
mod os {
    /// Maps `size` zeroed, read-write bytes (a multiple of 4096), or returns null.
    #[cfg(all(unix, not(miri)))]
    pub(super) fn map(size: usize) -> *mut u8 {
        // SAFETY: an anonymous private mapping has no preconditions.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            std::ptr::null_mut()
        } else {
            p as *mut u8
        }
    }

    /// # Safety
    /// `p` must come from [`map`] with this `size` and nothing may use it afterwards.
    #[cfg(all(unix, not(miri)))]
    pub(super) unsafe fn unmap(p: *mut u8, size: usize) {
        libc::munmap(p as *mut libc::c_void, size);
    }

    #[cfg(all(windows, not(miri)))]
    mod win {
        pub const MEM_COMMIT: u32 = 0x1000;
        pub const MEM_RESERVE: u32 = 0x2000;
        pub const MEM_RELEASE: u32 = 0x8000;
        pub const PAGE_READWRITE: u32 = 0x04;
        #[link(name = "kernel32")]
        extern "system" {
            pub fn VirtualAlloc(addr: *mut u8, size: usize, alloc_type: u32, protect: u32) -> *mut u8;
            pub fn VirtualFree(addr: *mut u8, size: usize, free_type: u32) -> i32;
        }
    }

    #[cfg(all(windows, not(miri)))]
    pub(super) fn map(size: usize) -> *mut u8 {
        // SAFETY: reserving and committing fresh pages has no preconditions; they read as zero.
        unsafe {
            win::VirtualAlloc(std::ptr::null_mut(), size, win::MEM_RESERVE | win::MEM_COMMIT, win::PAGE_READWRITE)
        }
    }

    /// # Safety
    /// `p` must come from [`map`] and nothing may use it afterwards.
    #[cfg(all(windows, not(miri)))]
    pub(super) unsafe fn unmap(p: *mut u8, _size: usize) {
        win::VirtualFree(p, 0, win::MEM_RELEASE);
    }

    // Elsewhere (and under Miri, which models the global allocator but not every mapping
    // call), segments come from the global allocator.
    #[cfg(any(miri, not(any(unix, windows))))]
    fn layout(size: usize) -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(size, 4096).unwrap()
    }

    #[cfg(any(miri, not(any(unix, windows))))]
    pub(super) fn map(size: usize) -> *mut u8 {
        // SAFETY: the layout has nonzero size.
        unsafe { std::alloc::alloc_zeroed(layout(size)) }
    }

    /// # Safety
    /// `p` must come from [`map`] with this `size` and nothing may use it afterwards.
    #[cfg(any(miri, not(any(unix, windows))))]
    pub(super) unsafe fn unmap(p: *mut u8, size: usize) {
        std::alloc::dealloc(p, layout(size));
    }
}

/// Global page pools.
pub(crate) struct PagePool {
    segments: Mutex<Vec<Segment>>,
    /// The emergency reserve: mapped segments outside the heap.
    reserve: Mutex<Vec<Segment>>,
    /// Set when the reserve has been drawn on; the next major collection refills it.
    reserve_short: AtomicBool,
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

pub(crate) static POOL: std::sync::LazyLock<PagePool> = std::sync::LazyLock::new(|| {
    restore_headroom();
    PagePool::new()
});

static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);

impl PagePool {
    fn new() -> PagePool {
        let pool = PagePool {
            segments: Mutex::new(Vec::new()),
            reserve: Mutex::new(Vec::new()),
            reserve_short: AtomicBool::new(true),
            empty: Mutex::new(Vec::new()),
            unswept: (0..NUM_CLASSES).map(|_| Mutex::new(Vec::new())).collect(),
            full: (0..NUM_CLASSES).map(|_| Mutex::new(Vec::new())).collect(),
            large: Mutex::new(Vec::new()),
            live_color: AtomicU8::new(1),
        };
        pool.refill_reserve();
        pool
    }

    fn new_segment(&self) {
        let mut base = map_segment();
        if base.is_null() {
            base = self.draw_reserve();
        }
        let pages: [*mut Page; PAGES_PER_SEGMENT] = std::array::from_fn(|i| {
            Box::into_raw(Box::new(Page {
                base: unsafe { base.add(i * PAGE_SIZE) },
                seg: base,
                slot_size: UnsafeCell::new(0),
                class: UnsafeCell::new(0),
                state: AtomicU8::new(PageState::Empty as u8),
            }))
        });
        self.segments.lock().push(Segment { base, pages });
        let mut empty = self.empty.lock();
        for &page in pages.iter().rev() {
            // SAFETY: freed only by `detach_segments`, once no list holds the page.
            empty.push(unsafe { &*page });
        }
        TOTAL_PAGES.fetch_add(PAGES_PER_SEGMENT, Ordering::Relaxed);
    }

    /// The OS refused a segment: takes one from the reserve, releases the headroom block and asks
    /// for a collection, which refills the reserve or leaves a pending `MemoryError`. Aborts if
    /// the reserve is used up.
    #[cold]
    fn draw_reserve(&self) -> *mut u8 {
        let mut reserve = self.reserve.lock();
        let Some(seg) = reserve.pop() else {
            drop(reserve);
            std::alloc::handle_alloc_error(Layout::from_size_align(SEGMENT_SIZE, 4096).unwrap());
        };
        drop(reserve);
        release_headroom();
        self.reserve_short.store(true, Ordering::Release);
        crate::collector::request_gc();
        seg.base
    }

    /// The allocator refused a large object of `bytes`: if the reserve can spare that much and
    /// still keep a few segments, returns enough of it to the OS for a retry to have a chance,
    /// asks for a collection (as [`PagePool::draw_reserve`] does) and returns true.
    #[cold]
    fn release_reserve_for(&self, bytes: usize) -> bool {
        let need = bytes.div_ceil(SEGMENT_SIZE) + 1;
        let mut reserve = self.reserve.lock();
        if reserve.len() < need + 2 {
            return false;
        }
        for _ in 0..need {
            let seg = reserve.pop().expect("reserve segment");
            // SAFETY: reserve segments are mapped and unused.
            unsafe { os::unmap(seg.base, SEGMENT_SIZE) };
        }
        drop(reserve);
        release_headroom();
        self.reserve_short.store(true, Ordering::Release);
        crate::collector::request_gc();
        true
    }

    /// Whether the reserve has been drawn on since it was last full.
    pub(crate) fn reserve_short(&self) -> bool {
        self.reserve_short.load(Ordering::Acquire)
    }

    /// Restores the reserve if it has been drawn on, from wholly empty segments first and then
    /// from the OS. Returns false if it could not (the heap is out of memory). Called by the
    /// collector with the world stopped and every page swept, and at startup.
    pub(crate) fn refill_reserve(&self) -> bool {
        if !self.reserve_short.load(Ordering::Acquire) {
            return true;
        }
        let mut reserve = self.reserve.lock();
        let want = RESERVE_SEGMENTS.saturating_sub(reserve.len());
        if want > 0 {
            let mut empty = self.empty.lock();
            let taken: std::collections::HashSet<*mut u8> = empty_counts(&empty)
                .into_iter()
                .filter(|&(_, n)| n == PAGES_PER_SEGMENT)
                .map(|(seg, _)| seg)
                .take(want)
                .collect();
            if !taken.is_empty() {
                self.detach_segments(&mut empty, &taken);
                reserve.extend(taken.into_iter().map(Segment::unused));
            }
        }
        let had = reserve.len();
        while reserve.len() < RESERVE_SEGMENTS {
            let base = map_segment();
            if base.is_null() {
                // Out of memory: what was just mapped goes back to the OS, as headroom for
                // raising the error.
                for seg in reserve.drain(had..) {
                    // SAFETY: just mapped, never used.
                    unsafe { os::unmap(seg.base, SEGMENT_SIZE) };
                }
                return false;
            }
            reserve.push(Segment::unused(base));
        }
        self.reserve_short.store(false, Ordering::Release);
        true
    }

    /// Removes the segments in `doomed`, all of whose pages are in `empty`, from the heap:
    /// forgets their pages and drops them from the segment list. Their memory stays mapped.
    fn detach_segments(&self, empty: &mut Vec<&'static Page>, doomed: &std::collections::HashSet<*mut u8>) {
        empty.retain(|p| !doomed.contains(&p.seg));
        let mut segments = self.segments.lock();
        let (gone, kept): (Vec<Segment>, Vec<Segment>) =
            std::mem::take(&mut *segments).into_iter().partition(|s| doomed.contains(&s.base));
        *segments = kept;
        drop(segments);
        for seg in gone {
            for p in seg.pages {
                // SAFETY: allocated by `new_segment`; an empty page is referenced only by the
                // empty list, which no longer holds it, and no mutator runs.
                drop(unsafe { Box::from_raw(p) });
            }
        }
        TOTAL_PAGES.fetch_sub(doomed.len() * PAGES_PER_SEGMENT, Ordering::Relaxed);
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

    /// Returns wholly empty segments to the OS, keeping about `keep` bytes of empty pages for
    /// the allocation expected before the next collection (so a program in a steady state does
    /// not map and unmap segments every cycle). Returns the bytes released. World stopped.
    pub(crate) fn release_empty_segments(&self, keep: usize) -> usize {
        let mut empty = self.empty.lock();
        let empty_bytes = empty.len() * PAGE_SIZE;
        if empty_bytes < keep + SEGMENT_SIZE {
            return 0;
        }
        // Empty pages per segment, in order of first appearance: the bottom of the stack holds
        // the pages unused the longest, so their segments go first.
        let mut excess = empty_bytes - keep;
        let mut doomed: std::collections::HashSet<*mut u8> = std::collections::HashSet::new();
        for (seg, n) in empty_counts(&empty) {
            if excess < SEGMENT_SIZE {
                break;
            }
            if n == PAGES_PER_SEGMENT {
                doomed.insert(seg);
                excess -= SEGMENT_SIZE;
            }
        }
        if doomed.is_empty() {
            return 0;
        }
        self.detach_segments(&mut empty, &doomed);
        drop(empty);
        for &seg in &doomed {
            // SAFETY: every page of the segment was empty and has just been forgotten.
            unsafe { os::unmap(seg, SEGMENT_SIZE) };
        }
        doomed.len() * SEGMENT_SIZE
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
static GENERATIONAL: std::sync::LazyLock<AtomicBool> =
    std::sync::LazyLock::new(|| AtomicBool::new(!matches!(std::env::var("CATX_GC_GENERATIONAL").as_deref(), Ok("0"))));

/// Turns generational collection on or off for threads registered from now on.
pub fn set_generational(on: bool) {
    GENERATIONAL.store(on, Ordering::Relaxed);
}

/// Young bytes a thread allocates before it asks for a minor collection.
fn nursery_bytes() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        if stats::stress() {
            return 64 * 1024;
        }
        // CATX_GC_NURSERY (bytes) overrides the default; see docs/gc-minor-cost.md.
        std::env::var("CATX_GC_NURSERY").ok().and_then(|v| v.parse().ok()).unwrap_or(4 * 1024 * 1024)
    })
}

/// Empty pages to keep for `mutators` threads' nurseries when releasing segments.
pub(crate) fn nursery_reserve(mutators: usize) -> usize {
    if GENERATIONAL.load(Ordering::Relaxed) {
        nursery_bytes() * mutators.max(1)
    } else {
        0
    }
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
    pub fn remember(&mut self, h: &Header) {
        if self.generational && h.set_remembered() {
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
            return self.alloc_large(size, vtable, true);
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

    /// As [`LocalHeap::alloc`], but a large object's bytes after the header are left
    /// uninitialized (small ones are still zeroed), saving a pass over memory the caller is about
    /// to overwrite anyway.
    ///
    /// # Safety
    /// The caller must initialize every byte of the object after the header before the thread's
    /// next safepoint, and must not read any byte before writing it.
    #[inline]
    pub unsafe fn alloc_uninit(&mut self, size: usize, vtable: *const crate::GcVTable) -> *mut Header {
        if size > MAX_SMALL {
            return self.alloc_large(size, vtable, false);
        }
        self.alloc(size, vtable)
    }

    /// As [`LocalHeap::alloc_uninit`], but returns null instead of aborting when the allocator
    /// refuses a large object, for callers that can raise a `MemoryError` where they allocate
    /// (the reserve is left for those that cannot). Small objects come from pages as usual.
    ///
    /// # Safety
    /// As for [`LocalHeap::alloc_uninit`].
    #[inline]
    pub unsafe fn try_alloc_uninit(&mut self, size: usize, vtable: *const crate::GcVTable) -> *mut Header {
        if size <= MAX_SMALL {
            return self.alloc(size, vtable);
        }
        self.try_alloc_large(size, vtable)
    }

    #[cold]
    unsafe fn try_alloc_large(&mut self, size: usize, vtable: *const crate::GcVTable) -> *mut Header {
        let Ok(layout) = Layout::from_size_align((size + 15) & !15, 16) else {
            return ptr::null_mut();
        };
        let p = alloc(layout);
        if p.is_null() {
            return ptr::null_mut();
        }
        self.init_large(p, layout, vtable)
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
    fn alloc_large(&mut self, size: usize, vtable: *const crate::GcVTable, zeroed: bool) -> *mut Header {
        let layout = Layout::from_size_align((size + 15) & !15, 16).unwrap();
        // Uninitialized memory spares the system allocator from clearing reused memory (on
        // macOS, an madvise call per allocation).
        let p = unsafe {
            if zeroed {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };
        if p.is_null() {
            return self.alloc_large_refused(layout, vtable, zeroed);
        }
        self.init_large(p, layout, vtable)
    }

    /// The allocator refused a large object: retries after handing reserve segments back to the
    /// OS (see the module docs), or aborts.
    #[cold]
    #[inline(never)]
    fn alloc_large_refused(&mut self, layout: Layout, vtable: *const crate::GcVTable, zeroed: bool) -> *mut Header {
        if POOL.release_reserve_for(layout.size()) {
            // SAFETY: the layout has nonzero size.
            let p = unsafe {
                if zeroed {
                    alloc_zeroed(layout)
                } else {
                    alloc(layout)
                }
            };
            if !p.is_null() {
                return self.init_large(p, layout, vtable);
            }
        }
        std::alloc::handle_alloc_error(layout)
    }

    fn init_large(&mut self, p: *mut u8, layout: Layout, vtable: *const crate::GcVTable) -> *mut Header {
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

    /// Addresses of the young large objects that are minor-marked, sorted. Called after a minor
    /// collection has marked from the roots alone: these are the ones the roots reach directly.
    pub(crate) fn marked_young_large(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .young_large
            .iter()
            .filter(|o| unsafe { &*(o.ptr as *const Header) }.meta_relaxed() & MINOR_MARK_BIT != 0)
            .map(|o| o.ptr as usize)
            .collect();
        v.sort_unstable();
        v
    }

    /// Minor collection, part 2: with every young object reachable from the thread's roots and
    /// remembered set minor-marked, frees the rest and tenures the survivors. Returns (bytes
    /// freed, bytes promoted).
    ///
    /// Large survivors in `keep_young` (sorted addresses) stay young instead: the collector found
    /// them referenced only by roots, never by a heap object. A heap object that comes to
    /// reference one later is either young or goes through the write barrier, so the next minor
    /// collection finds that reference too. This way a large temporary that was live only in a
    /// register when the collection ran (the previous string in `s = s + x`) is freed by the next
    /// minor collection instead of being promoted to wait for a major one.
    ///
    /// # Safety
    /// Called by the owning thread at a safepoint, after marking.
    pub(crate) unsafe fn sweep_young(&mut self, keep_young: &[usize]) -> (usize, usize) {
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
        let mut kept = Vec::new();
        let mut tenured = Vec::new();
        for obj in std::mem::take(&mut self.young_large) {
            let h = &*(obj.ptr as *const Header);
            if h.meta_relaxed() & MINOR_MARK_BIT != 0 {
                if keep_young.binary_search(&(obj.ptr as usize)).is_ok() {
                    h.clear_minor_mark();
                    kept.push(obj);
                } else {
                    h.tenure();
                    promoted += obj.layout.size();
                    tenured.push(obj);
                }
            } else {
                if let Some(fin) = (*h.vtable()).finalize {
                    fin(obj.ptr as *mut Header);
                }
                dealloc(obj.ptr, obj.layout);
                freed += obj.layout.size();
            }
        }
        self.young_large = kept;
        if !tenured.is_empty() {
            POOL.large.lock().append(&mut tenured);
        }
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

/// The segments holding pages in `empty`, in order of first appearance, with how many of their
/// pages are there.
fn empty_counts(empty: &[&'static Page]) -> Vec<(*mut u8, usize)> {
    let mut order: Vec<(*mut u8, usize)> = Vec::new();
    let mut index: std::collections::HashMap<*mut u8, usize> = std::collections::HashMap::new();
    for p in empty {
        let i = *index.entry(p.seg).or_insert_with(|| {
            order.push((p.seg, 0));
            order.len() - 1
        });
        order[i].1 += 1;
    }
    order
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reserve_len(pool: &PagePool) -> usize {
        pool.reserve.lock().len()
    }

    #[test]
    fn the_reserve_stands_in_for_refused_segments() {
        // A pool of its own (the global one serves other tests), on this thread, whose segment
        // mappings are made to fail.
        let pool = PagePool::new();
        assert!(!pool.reserve_short());
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS);
        fail::MAPS.with(|f| f.set(true));
        // No empty pages and the OS refuses a segment: a reserve segment serves.
        let page = pool.take_empty(0);
        // SAFETY: the page is ours and mapped.
        unsafe { ptr::write_bytes(page.base, 0xA5, PAGE_SIZE) };
        assert!(pool.reserve_short());
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 1);
        assert_eq!(pool.segments.lock().len(), 1);
        // The segment's other pages serve the next requests; after them, a second refusal
        // before any collection draws one more segment.
        let page2 = pool.take_empty(0);
        let rest: Vec<&'static Page> = (2..PAGES_PER_SEGMENT).map(|_| pool.take_empty(0)).collect();
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 1);
        let page3 = pool.take_empty(0);
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 1 - 1);
        assert_eq!(pool.segments.lock().len(), 2);
        // The segment is in use and the OS still refuses: the reserve cannot be refilled.
        assert!(!pool.refill_reserve());
        assert!(pool.reserve_short());
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 1 - 1);
        // Once its pages are empty again the first segment returns to the reserve, which is
        // still short.
        for p in [page, page2].into_iter().chain(rest) {
            p.state.store(PageState::Empty as u8, Ordering::Relaxed);
            pool.empty.lock().push(p);
        }
        assert!(!pool.refill_reserve());
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 1);
        assert_eq!(pool.segments.lock().len(), 1);
        let _ = page3;
        // The OS gives memory again: the reserve is whole.
        fail::MAPS.with(|f| f.set(false));
        assert!(pool.refill_reserve());
        assert!(!pool.reserve_short());
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS);
    }

    #[test]
    fn the_reserve_makes_room_for_large_objects_it_can_cover() {
        let pool = PagePool::new();
        assert!(pool.release_reserve_for(SEGMENT_SIZE + 1));
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 3);
        assert!(pool.reserve_short());
        // Too large for what is left: the reserve is kept.
        assert!(!pool.release_reserve_for(RESERVE_SEGMENTS * SEGMENT_SIZE));
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS - 3);
        assert!(pool.refill_reserve());
        assert_eq!(reserve_len(&pool), RESERVE_SEGMENTS);
    }
}
