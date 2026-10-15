//! Thread registry, safepoints and the parallel stop-the-world mark phase.
//!
//! Every mutator thread registers a [`ThreadRecord`]. A collection is requested by setting
//! `GC_REQUESTED`; mutators poll it at safepoints (backward jumps and calls in the interpreter).
//! The first thread to reach a safepoint becomes the collector. It waits until every other thread
//! is parked at a safepoint or running native code (`InNative`), then marks with all parked
//! threads as workers sharing a work-stealing deque, and resumes the world. Sweeping is lazy.
//!
//! A thread in native code must not touch the heap. Leaving native code waits for any running
//! collection to finish, which is what makes treating `InNative` threads as stopped sound.
//!
//! Between major collections each thread collects its own young objects (a *minor*
//! collection) at its own safepoints, without stopping anyone: see [`minor_collect`]. Each thread
//! polls its own flag ([`ThreadRecord::poll_flag`]); a major request raises every thread's flag,
//! a full nursery raises only its owner's.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_deque::{Steal, Stealer, Worker};
use parking_lot::{Condvar, Mutex};

use crate::header::Header;
use crate::header::{MINOR_MARK_BIT, YOUNG_BIT};
use crate::heap::{LocalHeap, POOL};
use crate::stats;

/// Supplies a set of roots to the collector.
pub trait RootSource {
    /// Marks every root. Called only while the owning thread is stopped.
    fn trace_roots(&self, tracer: &mut Tracer);
}

#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ThreadState {
    Running = 0,
    Parked = 1,
    InNative = 2,
    /// In native code and held there by an ongoing collection.
    Blocked = 3,
}

pub struct ThreadRecord {
    pub tid: u16,
    /// Raised when this thread should enter [`safepoint_slow`]: a major collection was
    /// requested, or its nursery is full.
    poll: AtomicBool,
    state: AtomicU8,
    roots: UnsafeCell<Option<*const dyn RootSource>>,
    heap: UnsafeCell<*mut LocalHeap>,
}

// SAFETY: the raw pointers are dereferenced by other threads only while this thread is stopped.
unsafe impl Send for ThreadRecord {}
unsafe impl Sync for ThreadRecord {}

impl ThreadRecord {
    /// The flag safepoint polls load. Interpreters may cache a reference to it.
    #[inline(always)]
    pub fn poll_flag(&self) -> &AtomicBool {
        &self.poll
    }

    pub fn state(&self) -> ThreadState {
        match self.state.load(Ordering::SeqCst) {
            0 => ThreadState::Running,
            1 => ThreadState::Parked,
            2 => ThreadState::InNative,
            _ => ThreadState::Blocked,
        }
    }
}

pub(crate) static GC_REQUESTED: AtomicBool = AtomicBool::new(false);
static GC_DISABLED: AtomicUsize = AtomicUsize::new(0);

struct Coord {
    collecting: bool,
    epoch: u64,
    /// Incremented when a mark phase starts; parked threads join it once.
    mark_epoch: u64,
    marking: bool,
}

/// A retired buffer's address and the function that frees it.
type Retired = (usize, unsafe fn(*mut u8));

struct Global {
    threads: Mutex<Vec<Arc<ThreadRecord>>>,
    /// Every registered thread, for raising poll flags. Never held while blocking, unlike
    /// `threads`, which a collector holds for a whole collection.
    pollers: Mutex<Vec<Arc<ThreadRecord>>>,
    free_tids: Mutex<Vec<u16>>,
    next_tid: AtomicUsize,
    coord: Mutex<Coord>,
    cv: Condvar,
    global_roots: Mutex<Vec<Box<dyn RootSource + Send + Sync>>>,
    /// Out-of-heap buffers awaiting the next stop-the-world (see [`retire`]).
    retired: Mutex<Vec<Retired>>,
    mark: MarkShared,
}

struct MarkShared {
    /// Each marker's shared deque, for stealing. A marker keeps its work on a private stack and
    /// moves a few items here at a time (see [`mark_worker`]), so these stay small.
    stealers: Mutex<Vec<Stealer<usize>>>,
    /// Set when a marker's stack could not grow and it left an object unmarked (see
    /// [`Tracer::mark`]); the collector then rescans (see [`rescan_major`]).
    overflowed: AtomicBool,
    active: AtomicUsize,
    color: AtomicU8,
    marked_bytes: AtomicUsize,
    workers_in: AtomicUsize,
    /// Threads that took part in the last mark phase (for tracing).
    markers_seen: AtomicUsize,
}

static GLOBAL: std::sync::LazyLock<Global> = std::sync::LazyLock::new(|| Global {
    threads: Mutex::new(Vec::new()),
    pollers: Mutex::new(Vec::new()),
    free_tids: Mutex::new(Vec::new()),
    next_tid: AtomicUsize::new(1),
    coord: Mutex::new(Coord { collecting: false, epoch: 0, mark_epoch: 0, marking: false }),
    cv: Condvar::new(),
    global_roots: Mutex::new(Vec::new()),
    retired: Mutex::new(Vec::new()),
    mark: MarkShared {
        stealers: Mutex::new(Vec::new()),
        overflowed: AtomicBool::new(false),
        active: AtomicUsize::new(0),
        color: AtomicU8::new(1),
        marked_bytes: AtomicUsize::new(0),
        workers_in: AtomicUsize::new(0),
        markers_seen: AtomicUsize::new(0),
    },
});

/// Asks for a collection at the next safepoint of any running thread.
pub fn request_gc() {
    if GC_REQUESTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Pairs with the fence in `safepoint_slow` (store-buffering: see tests/loom.rs).
    std::sync::atomic::fence(Ordering::SeqCst);
    for t in GLOBAL.pollers.lock().iter() {
        t.poll.store(true, Ordering::SeqCst);
    }
}

/// Registers the calling thread as a mutator. The thread starts in the `InNative` state; call
/// [`leave_native`] before touching the heap. `roots` and `heap` must stay valid until
/// [`unregister_thread`].
pub fn register_thread() -> Arc<ThreadRecord> {
    let tid = GLOBAL.free_tids.lock().pop().unwrap_or_else(|| {
        let t = GLOBAL.next_tid.fetch_add(1, Ordering::Relaxed);
        assert!(t < u16::MAX as usize, "too many live threads");
        t as u16
    });
    let rec = Arc::new(ThreadRecord {
        tid,
        poll: AtomicBool::new(false),
        state: AtomicU8::new(ThreadState::InNative as u8),
        roots: UnsafeCell::new(None),
        heap: UnsafeCell::new(std::ptr::null_mut()),
    });
    GLOBAL.threads.lock().push(rec.clone());
    {
        let mut pollers = GLOBAL.pollers.lock();
        pollers.push(rec.clone());
        // A request raised before we were listed still reaches us.
        rec.poll.store(GC_REQUESTED.load(Ordering::SeqCst), Ordering::SeqCst);
    }
    rec
}

/// Installs the thread's root source and heap. Must be called by the owning thread while it is
/// in native state (so no collection can observe a half-installed record).
///
/// # Safety
/// The pointers must stay valid until they are replaced or the thread unregisters.
pub unsafe fn install(rec: &ThreadRecord, roots: *const dyn RootSource, heap: *mut LocalHeap) {
    debug_assert!(matches!(rec.state(), ThreadState::InNative | ThreadState::Blocked));
    // Wait out a collection that may be reading the record.
    let _g = GLOBAL.threads.lock();
    *rec.roots.get() = Some(roots);
    *rec.heap.get() = heap;
    if !heap.is_null() {
        (*heap).set_poll(&rec.poll);
    }
}

/// Removes the thread from the registry. The thread must be in native state, and its heap pages
/// must already have been released.
pub fn unregister_thread(rec: &Arc<ThreadRecord>) {
    debug_assert!(matches!(rec.state(), ThreadState::InNative | ThreadState::Blocked));
    {
        // A collector holds `threads` for the whole collection, so this waits it out.
        let mut threads = GLOBAL.threads.lock();
        threads.retain(|t| !Arc::ptr_eq(t, rec));
        GLOBAL.pollers.lock().retain(|t| !Arc::ptr_eq(t, rec));
        unsafe {
            *rec.roots.get() = None;
            *rec.heap.get() = std::ptr::null_mut();
        }
    }
    GLOBAL.free_tids.lock().push(rec.tid);
    GLOBAL.cv.notify_all();
}

/// Registers roots that live outside any thread (globals, constants, interned strings).
pub fn add_global_roots(src: Box<dyn RootSource + Send + Sync>) {
    GLOBAL.global_roots.lock().push(src);
}

/// Frees `ptr` with `free` at the next collection, once the world is stopped. Containers whose
/// readers do not lock use this for replaced buffers: a reader never holds a buffer across a
/// safepoint, so no reader can still see one by then.
pub fn retire(ptr: *mut u8, free: unsafe fn(*mut u8)) {
    GLOBAL.retired.lock().push((ptr as usize, free));
}

/// Disables collection while the guard lives (used while loading programs).
pub struct NoGcGuard(());
impl NoGcGuard {
    pub fn new() -> Self {
        GC_DISABLED.fetch_add(1, Ordering::SeqCst);
        NoGcGuard(())
    }
}
impl Default for NoGcGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for NoGcGuard {
    fn drop(&mut self) {
        GC_DISABLED.fetch_sub(1, Ordering::SeqCst);
    }
}

/// True if a collection has been requested (the inline part of a safepoint poll).
#[inline(always)]
pub fn is_gc_requested() -> bool {
    GC_REQUESTED.load(Ordering::Acquire)
}

/// The safepoint poll: one load and a predictable branch.
#[inline(always)]
pub fn safepoint(rec: &ThreadRecord) {
    if rec.poll.load(Ordering::Acquire) {
        safepoint_slow(rec);
    }
}

#[cold]
#[inline(never)]
pub fn safepoint_slow(rec: &ThreadRecord) {
    if GC_DISABLED.load(Ordering::SeqCst) > 0 {
        return;
    }
    // Lower the flag before looking at the reasons, so a request racing with us re-raises it.
    rec.poll.store(false, Ordering::SeqCst);
    std::sync::atomic::fence(Ordering::SeqCst);
    if GC_REQUESTED.load(Ordering::SeqCst) {
        major_safepoint(rec);
    }
    let heap = unsafe { *rec.heap.get() };
    if !heap.is_null() && unsafe { (*heap).minor_wanted } {
        minor_collect(rec);
    }
}

/// Collects the calling thread's young generation. Other threads keep running: the ownership
/// rules guarantee none of them can reach these objects.
fn minor_collect(rec: &ThreadRecord) {
    let start = std::time::Instant::now();
    let heap = unsafe { &mut **rec.heap.get() };
    let Some(roots) = (unsafe { *rec.roots.get() }) else { return };
    let mut tracer = Tracer::minor(rec.tid);
    unsafe { (*roots).trace_roots(&mut tracer) };
    // Large objects the roots reach directly. Marking from the heap below removes the ones a
    // heap object also references; the rest may stay young (see `LocalHeap::sweep_young`).
    if let Some(m) = tracer.minor.as_mut() {
        m.root_large = heap.marked_young_large();
    }
    for &o in heap.remembered() {
        if !remembered_and_owned(o, rec.tid) {
            continue;
        }
        unsafe {
            let vt = &*(*o).vtable();
            (vt.trace_remembered.unwrap_or(vt.trace))(o, &mut tracer)
        };
    }
    tracer.drain();
    // The mark stack could not grow: some young objects were left unmarked, each referenced by
    // a root, a remembered object or a marked object. Trace all of those again until a pass
    // completes. Remembered objects get their full trace: the written-range scan above may have
    // forgotten the slots it visited.
    let mut rescans = 0;
    while std::mem::take(&mut tracer.overflowed) {
        rescans += 1;
        if let Some(m) = tracer.minor.as_mut() {
            // The roots are traced again: a root reference does not make a large object
            // heap-referenced.
            m.roots_again = true;
        }
        unsafe { (*roots).trace_roots(&mut tracer) };
        if let Some(m) = tracer.minor.as_mut() {
            m.roots_again = false;
        }
        tracer.drain();
        for &o in heap.remembered() {
            if remembered_and_owned(o, rec.tid) {
                unsafe { ((*(*o).vtable()).trace)(o, &mut tracer) };
                tracer.drain();
            }
        }
        unsafe {
            heap.for_each_minor_marked(|h| {
                ((*(*h).vtable()).trace)(h, &mut tracer);
                tracer.drain();
            })
        };
    }
    if rescans > 0 {
        stats::note_mark_overflow(rescans);
    }
    let marked = start.elapsed();
    let remembered = heap.remembered().len();
    let keep_young = tracer.minor.take().map(|m| m.root_large).unwrap_or_default();
    let (freed, promoted) = unsafe { heap.sweep_young(&keep_young) };
    stats::note_minor(start.elapsed().as_nanos() as usize, freed, promoted);
    if trace_enabled() {
        let total = start.elapsed();
        eprintln!(
            "gc: minor (thread {}): mark {:.3} ms ({} remembered), sweep {:.3} ms, freed {} KB, promoted {} KB",
            rec.tid,
            marked.as_secs_f64() * 1e3,
            remembered,
            (total - marked).as_secs_f64() * 1e3,
            freed / 1024,
            promoted / 1024
        );
    }
}

/// Whether the remembered object `o` is still owned by `tid`. One it shared, froze or handed to
/// another thread since remembering it had its young referents tenured first (`escape` in the
/// runtime), and may be written by other threads now.
fn remembered_and_owned(o: *const Header, tid: u16) -> bool {
    let meta = unsafe { (*o).meta() };
    Header::state_of(meta) == crate::ObjState::Owned && Header::owner_of(meta) == tid
}

/// Runs a minor collection of the calling thread now (tests and embedders). The thread must be
/// running and its roots installed.
pub fn collect_minor(rec: &ThreadRecord) {
    if GC_DISABLED.load(Ordering::SeqCst) > 0 {
        return;
    }
    minor_collect(rec);
}

fn major_safepoint(rec: &ThreadRecord) {
    let mut coord = GLOBAL.coord.lock();
    if !GC_REQUESTED.load(Ordering::Acquire) {
        return;
    }
    if !coord.collecting {
        coord.collecting = true;
        drop(coord);
        collect(rec);
        return;
    }
    // Someone else is collecting: park and help mark.
    let mut my_epoch = coord.epoch;
    let mut joined_mark = 0u64;
    rec.state.store(ThreadState::Parked as u8, Ordering::SeqCst);
    GLOBAL.cv.notify_all();
    loop {
        if coord.marking && coord.mark_epoch != joined_mark {
            joined_mark = coord.mark_epoch;
            GLOBAL.mark.workers_in.fetch_add(1, Ordering::SeqCst);
            drop(coord);
            mark_worker(None);
            GLOBAL.mark.workers_in.fetch_sub(1, Ordering::SeqCst);
            GLOBAL.cv.notify_all();
            coord = GLOBAL.coord.lock();
            continue;
        }
        if coord.epoch != my_epoch {
            if !coord.collecting {
                break;
            }
            // A new collection started before we resumed: it counts us as stopped (we are
            // still parked), so stay parked for it too.
            my_epoch = coord.epoch;
        }
        GLOBAL.cv.wait(&mut coord);
    }
    // Still holding the coordination lock: no collection can start until we are running.
    rec.state.store(ThreadState::Running as u8, Ordering::SeqCst);
}

/// Marks the thread as running native code that does not touch the heap.
#[inline]
pub fn enter_native(rec: &ThreadRecord) {
    rec.state.store(ThreadState::InNative as u8, Ordering::SeqCst);
    if GC_REQUESTED.load(Ordering::SeqCst) {
        // A collector may be waiting for us to stop.
        let _c = GLOBAL.coord.lock();
        GLOBAL.cv.notify_all();
    }
}

/// Returns from native code, waiting for any collection in progress.
#[inline]
pub fn leave_native(rec: &ThreadRecord) {
    if rec
        .state
        .compare_exchange(ThreadState::InNative as u8, ThreadState::Running as u8, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return;
    }
    leave_native_slow(rec);
}

#[cold]
fn leave_native_slow(rec: &ThreadRecord) {
    let mut coord = GLOBAL.coord.lock();
    loop {
        if rec
            .state
            .compare_exchange(
                ThreadState::InNative as u8,
                ThreadState::Running as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return;
        }
        GLOBAL.cv.wait(&mut coord);
    }
}

/// Runs a collection now from a running mutator thread (waits for any in-progress one instead).
pub fn collect_now(rec: &ThreadRecord) {
    request_gc();
    safepoint_slow(rec);
}

/// Set to make the next major collection sweep every page itself instead of leaving that to
/// allocation, so that garbage (and memory its finalizers free) is released by the time it ends.
static SWEEP_ALL: AtomicBool = AtomicBool::new(false);

/// As [`collect_now`], for when the allocator refused memory: the collection also sweeps every
/// page, running finalizers, so that buffers held by garbage are freed before the caller retries.
/// (A collection already in progress when this is called may not sweep; one retry is the
/// caller's best effort either way.)
pub fn collect_to_free(rec: &ThreadRecord) {
    SWEEP_ALL.store(true, Ordering::SeqCst);
    collect_now(rec);
}

/// `CATX_GC_TRACE=1` prints the phases of every major collection.
fn trace_enabled() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("CATX_GC_TRACE").is_some())
}

/// Dedicated marking threads, so a program with few mutators still marks on every core.
/// `CATX_GC_THREADS` sets how many (default: available cores minus one).
fn start_mark_helpers() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let n = std::env::var("CATX_GC_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1));
        for i in 0..n.min(64) {
            let _ = std::thread::Builder::new().name(format!("catx-gc-{i}")).spawn(mark_helper_main);
        }
    });
}

/// A helper sleeps until a mark phase starts, marks, and sleeps again. It is not a mutator: it
/// never touches objects outside a mark phase, so collections need not wait for it.
fn mark_helper_main() {
    let mut joined = 0u64;
    let mut coord = GLOBAL.coord.lock();
    loop {
        if coord.marking && coord.mark_epoch != joined {
            joined = coord.mark_epoch;
            GLOBAL.mark.workers_in.fetch_add(1, Ordering::SeqCst);
            drop(coord);
            mark_worker(None);
            GLOBAL.mark.workers_in.fetch_sub(1, Ordering::SeqCst);
            GLOBAL.cv.notify_all();
            coord = GLOBAL.coord.lock();
            continue;
        }
        GLOBAL.cv.wait(&mut coord);
    }
}

fn collect(me: &ThreadRecord) {
    start_mark_helpers();
    let start = std::time::Instant::now();
    let threads = GLOBAL.threads.lock();
    // 1. Stop the world.
    {
        let mut coord = GLOBAL.coord.lock();
        loop {
            let mut all_stopped = true;
            for t in threads.iter() {
                if std::ptr::eq(&**t, me) {
                    continue;
                }
                let _ = t.state.compare_exchange(
                    ThreadState::InNative as u8,
                    ThreadState::Blocked as u8,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                if t.state() == ThreadState::Running {
                    all_stopped = false;
                }
            }
            if all_stopped {
                break;
            }
            GLOBAL.cv.wait_for(&mut coord, std::time::Duration::from_millis(1));
        }
    }

    let t_stopped = start.elapsed();
    // 2. Free retired buffers (no mutator is mid-read), retire every thread's current pages,
    //    then consume last cycle's colors.
    for (p, free) in std::mem::take(&mut *GLOBAL.retired.lock()) {
        unsafe { free(p as *mut u8) };
    }
    for t in threads.iter() {
        let heap = unsafe { *t.heap.get() };
        if !heap.is_null() {
            unsafe { (*heap).release_pages() };
        }
    }
    POOL.finish_sweeping();
    let t_swept = start.elapsed();

    // 3. Flip the color and mark from the roots.
    let old = POOL.live_color.load(Ordering::Relaxed);
    let color = if old == 1 { 2 } else { 1 };
    GLOBAL.mark.color.store(color, Ordering::SeqCst);
    GLOBAL.mark.marked_bytes.store(0, Ordering::SeqCst);
    GLOBAL.mark.markers_seen.store(0, Ordering::Relaxed);
    GLOBAL.mark.overflowed.store(false, Ordering::SeqCst);
    let mut tracer = Tracer::major(color);
    trace_all_roots(&threads, &mut tracer);
    {
        let mut coord = GLOBAL.coord.lock();
        coord.marking = true;
        coord.mark_epoch += 1;
        GLOBAL.cv.notify_all();
    }
    // The collector marks from the roots, sharing work with the helpers as it goes.
    mark_worker(Some(tracer));
    // Wait for helpers to leave the mark loop before ending the phase.
    {
        let mut coord = GLOBAL.coord.lock();
        coord.marking = false;
        while GLOBAL.mark.workers_in.load(Ordering::SeqCst) > 0 {
            GLOBAL.cv.wait_for(&mut coord, std::time::Duration::from_micros(200));
        }
    }
    GLOBAL.mark.stealers.lock().clear();
    let mut rescans = 0;
    while GLOBAL.mark.overflowed.swap(false, Ordering::SeqCst) {
        rescans += 1;
        rescan_major(&threads, color);
    }
    if rescans > 0 {
        stats::note_mark_overflow(rescans);
    }
    let t_marked = start.elapsed();

    // 4. Publish the new color; sweeping happens lazily.
    POOL.live_color.store(color, Ordering::SeqCst);
    POOL.sweep_large(color);
    POOL.mark_all_unswept();
    stats::after_collection(GLOBAL.mark.marked_bytes.load(Ordering::SeqCst), threads.len());
    // If the OS refused memory (an allocation drew on the emergency reserve, or released the
    // headroom block), sweep now, so that the memory this collection freed is known, then refill
    // the reserve and map the headroom again (`collect_to_free` asks for the sweep too). If that
    // fails the heap is out of memory: the next thread at a safepoint raises a MemoryError, and
    // the next collection comes soon. A collection that finds memory again withdraws an error no
    // thread has raised yet.
    let sweep_all = SWEEP_ALL.swap(false, Ordering::SeqCst);
    let refused = POOL.reserve_short() || !crate::heap::headroom_held();
    if sweep_all || refused {
        POOL.finish_sweeping();
    }
    let out_of_memory = refused && !(POOL.refill_reserve() && crate::heap::restore_headroom());
    crate::limits::note_out_of_memory(out_of_memory);
    if out_of_memory {
        stats::collect_soon();
    }
    // Every page is swept now (step 2), so wholly empty segments are known; keep enough empty
    // pages for the allocation budget until the next collection and return the rest.
    let released = POOL.release_empty_segments(stats::threshold() + crate::heap::nursery_reserve(threads.len()));
    stats::note_released(released);

    // 5. Resume the world.
    for t in threads.iter() {
        let _ = t.state.compare_exchange(
            ThreadState::Blocked as u8,
            ThreadState::InNative as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
    stats::note_pause(start.elapsed().as_nanos() as usize);
    if trace_enabled() {
        eprintln!(
            "gc: major #{}: stop {:.2} ms, finish sweep {:.2} ms, mark {:.2} ms ({} markers, {} rescans), total {:.2} ms, released {:.1} MB",
            stats::collections(),
            t_stopped.as_secs_f64() * 1e3,
            (t_swept - t_stopped).as_secs_f64() * 1e3,
            (t_marked - t_swept).as_secs_f64() * 1e3,
            GLOBAL.mark.markers_seen.load(Ordering::Relaxed),
            rescans,
            start.elapsed().as_secs_f64() * 1e3,
            released as f64 / 1e6
        );
    }
    let mut coord = GLOBAL.coord.lock();
    GC_REQUESTED.store(false, Ordering::SeqCst);
    coord.collecting = false;
    coord.epoch += 1;
    drop(threads);
    GLOBAL.cv.notify_all();
}

fn trace_all_roots(threads: &[Arc<ThreadRecord>], tracer: &mut Tracer) {
    for t in threads.iter() {
        if let Some(r) = unsafe { *t.roots.get() } {
            unsafe { (*r).trace_roots(tracer) };
        }
    }
    for r in GLOBAL.global_roots.lock().iter() {
        r.trace_roots(tracer);
    }
}

/// After a mark phase in which a marker's stack could not grow: every unmarked live object is
/// referenced by a root or by a marked object, so tracing all of those again (the collector
/// alone, world still stopped) marks it. Another overflow sets the flag for another pass; each
/// pass marks at least what its stack held, so passes end.
fn rescan_major(threads: &[Arc<ThreadRecord>], color: u8) {
    let mut tracer = Tracer::major(color);
    trace_all_roots(threads, &mut tracer);
    let mut bytes = tracer.drain();
    // SAFETY: the world is stopped; the pool's pages and large objects are all there is.
    unsafe {
        POOL.for_each_object(|h| {
            let vt = &*(*h).vtable();
            if !vt.leaf && (*h).color() == color {
                (vt.trace)(h, &mut tracer);
                bytes += tracer.drain();
            }
        })
    };
    let m = &GLOBAL.mark;
    m.marked_bytes.fetch_add(bytes + tracer.leaf_bytes, Ordering::SeqCst);
    if tracer.overflowed {
        m.overflowed.store(true, Ordering::SeqCst);
    }
}

/// Under `CATX_GC_STRESS`, mark stacks hold at most this many entries, so that the stress runs
/// exercise the overflow rescans too.
const STRESS_MARK_STACK: usize = 64;

/// The longest a mark stack may grow (`set_mark_stack_limit`, tests).
static MARK_STACK_LIMIT: AtomicUsize = AtomicUsize::new(0);

/// Caps every mark stack at `entries` (0: no cap but the allocator's), so that tests can make
/// marking overflow and rescan without exhausting memory. `CATX_GC_MARK_STACK` sets it too, and
/// `CATX_GC_STRESS` makes it 64 unless that is set.
#[doc(hidden)]
pub fn set_mark_stack_limit(entries: usize) {
    MARK_STACK_LIMIT.store(entries, Ordering::SeqCst);
}

fn mark_stack_limit() -> usize {
    static ENV: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let set = MARK_STACK_LIMIT.load(Ordering::Relaxed);
    if set != 0 {
        return set;
    }
    let env = *ENV.get_or_init(|| std::env::var("CATX_GC_MARK_STACK").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    match env {
        0 if stats::stress() => STRESS_MARK_STACK,
        0 => usize::MAX,
        n => n,
    }
}

/// Marks objects and queues them for scanning.
///
/// Marking runs when memory is short, so it must not abort for want of memory (issue #139).
/// Objects without references ([`GcVTable::leaf`](crate::GcVTable::leaf)) are marked without
/// being queued. The others go on a private stack that grows fallibly: when it cannot grow, the
/// object is left unmarked and the tracer records an overflow, after which the collection traces
/// the roots and every marked object again to find what it left (see [`rescan_major`] and
/// `minor_collect`).
pub struct Tracer {
    color: u8,
    stack: Vec<usize>,
    /// Bytes of the leaf objects this tracer marked.
    leaf_bytes: usize,
    /// An object was left unmarked because the stack could not grow.
    overflowed: bool,
    minor: Option<MinorMark>,
}

/// Minor-collection marking: only `tid`'s young objects.
struct MinorMark {
    tid: u16,
    /// Sorted addresses of young large objects marked from the roots and not (yet) found
    /// referenced by a heap object.
    root_large: Vec<usize>,
    /// The roots are being traced again after an overflow: their references are not heap ones.
    roots_again: bool,
}

impl Tracer {
    fn major(color: u8) -> Self {
        Tracer { color, stack: Vec::new(), leaf_bytes: 0, overflowed: false, minor: None }
    }

    fn minor(tid: u16) -> Self {
        Tracer {
            color: 0,
            stack: Vec::new(),
            leaf_bytes: 0,
            overflowed: false,
            minor: Some(MinorMark { tid, root_large: Vec::new(), roots_again: false }),
        }
    }

    /// Room on the stack for one more entry, growing it if it can.
    #[inline(always)]
    fn reserve(&mut self) -> bool {
        self.stack.len() < self.stack.capacity() || self.grow()
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self) -> bool {
        let room = mark_stack_limit().saturating_sub(self.stack.len());
        if room == 0 {
            return false;
        }
        let want = self.stack.capacity().max(256).min(room);
        self.stack.try_reserve_exact(want).is_ok()
    }

    /// Traces every object on the stack (and what that pushes); returns their bytes.
    fn drain(&mut self) -> usize {
        let mut bytes = 0;
        while let Some(o) = self.stack.pop() {
            let h = o as *const Header;
            unsafe {
                let vt = &*(*h).vtable();
                bytes += (vt.size)(h);
                (vt.trace)(h, self);
            }
        }
        bytes
    }

    /// Marks `obj` reachable. Null pointers are ignored. Tracers are only handed to root sources
    /// and trace hooks, whose contract is to pass live object pointers.
    #[inline]
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn mark(&mut self, obj: *const Header) {
        if obj.is_null() {
            return;
        }
        // SAFETY: the runtime only hands us pointers to live heap objects.
        let h = unsafe { &*obj };
        if let Some(m) = &mut self.minor {
            // Old objects are not traced: their young referents are found through the
            // remembered set.
            let meta = h.meta_relaxed();
            if meta & YOUNG_BIT == 0 || Header::owner_of(meta) != m.tid {
                return;
            }
            if meta & MINOR_MARK_BIT != 0 {
                if !m.roots_again && !m.root_large.is_empty() {
                    // A heap reference to a large object the roots also reach.
                    if let Ok(i) = m.root_large.binary_search(&(obj as usize)) {
                        m.root_large.remove(i);
                    }
                }
                return;
            }
            let leaf = unsafe { (*h.vtable()).leaf };
            if !leaf && !self.reserve() {
                self.overflowed = true;
                return;
            }
            if h.try_minor_mark() && !leaf {
                self.stack.push(obj as usize);
            }
            return;
        }
        if h.color() == self.color {
            return;
        }
        let vt = unsafe { &*h.vtable() };
        if vt.leaf {
            if h.try_mark(self.color) {
                self.leaf_bytes += unsafe { (vt.size)(obj) };
            }
            return;
        }
        if !self.reserve() {
            self.overflowed = true;
            return;
        }
        if h.try_mark(self.color) {
            self.stack.push(obj as usize);
        }
    }
}

/// How many entries a marker moves to its shared deque at a time, when that is empty.
const SHARE_BATCH: usize = 16;

/// Marks until every marker is out of work. Each marker traces from its private stack (the
/// collector's starts with what the roots reached) and, every few objects, moves a few entries
/// to its shared deque if that is empty, for idle markers to steal. The deques stay at most a
/// batch or two long, so their buffers never grow past their initial size.
fn mark_worker(start: Option<Tracer>) {
    let m = &GLOBAL.mark;
    let worker = Worker::new_lifo();
    m.stealers.lock().push(worker.stealer());
    let mut tracer = start.unwrap_or_else(|| Tracer::major(m.color.load(Ordering::SeqCst)));
    m.markers_seen.fetch_add(1, Ordering::Relaxed);
    m.active.fetch_add(1, Ordering::SeqCst);
    let mut bytes = 0usize;
    let mut n = 0usize;
    loop {
        if n % 32 == 0 && tracer.stack.len() > 1 && worker.is_empty() {
            for _ in 0..SHARE_BATCH.min(tracer.stack.len() / 2) {
                if let Some(o) = tracer.stack.pop() {
                    worker.push(o);
                }
            }
        }
        n += 1;
        let next = tracer.stack.pop().or_else(|| worker.pop()).or_else(|| steal(m, &worker));
        match next {
            Some(obj) => {
                let h = obj as *const Header;
                unsafe {
                    let vt = (*h).vtable();
                    bytes += ((*vt).size)(h);
                    ((*vt).trace)(h, &mut tracer);
                }
            }
            None => {
                // Termination: go idle, and finish only when every worker is idle with no work left.
                m.active.fetch_sub(1, Ordering::SeqCst);
                loop {
                    if has_work(m) {
                        m.active.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                    if m.active.load(Ordering::SeqCst) == 0 {
                        m.marked_bytes.fetch_add(bytes + tracer.leaf_bytes, Ordering::SeqCst);
                        if tracer.overflowed {
                            m.overflowed.store(true, Ordering::SeqCst);
                        }
                        return;
                    }
                    std::hint::spin_loop();
                    std::thread::yield_now();
                }
            }
        }
    }
}

fn steal(m: &MarkShared, local: &Worker<usize>) -> Option<usize> {
    loop {
        let stealers = m.stealers.lock();
        let mut retry = false;
        for s in stealers.iter() {
            match s.steal_batch_and_pop(local) {
                Steal::Success(o) => return Some(o),
                Steal::Retry => retry = true,
                Steal::Empty => {}
            }
        }
        if !retry {
            return None;
        }
    }
}

fn has_work(m: &MarkShared) -> bool {
    m.stealers.lock().iter().any(|s| !s.is_empty())
}
