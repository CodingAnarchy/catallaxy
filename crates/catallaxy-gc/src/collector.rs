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

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use parking_lot::{Condvar, Mutex};

use crate::header::Header;
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
    state: AtomicU8,
    roots: UnsafeCell<Option<*const dyn RootSource>>,
    heap: UnsafeCell<*mut LocalHeap>,
}

// SAFETY: the raw pointers are dereferenced by other threads only while this thread is stopped.
unsafe impl Send for ThreadRecord {}
unsafe impl Sync for ThreadRecord {}

impl ThreadRecord {
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
    injector: Injector<usize>,
    stealers: Mutex<Vec<Stealer<usize>>>,
    active: AtomicUsize,
    color: AtomicU8,
    marked_bytes: AtomicUsize,
    workers_in: AtomicUsize,
}

static GLOBAL: std::sync::LazyLock<Global> = std::sync::LazyLock::new(|| Global {
    threads: Mutex::new(Vec::new()),
    free_tids: Mutex::new(Vec::new()),
    next_tid: AtomicUsize::new(1),
    coord: Mutex::new(Coord { collecting: false, epoch: 0, mark_epoch: 0, marking: false }),
    cv: Condvar::new(),
    global_roots: Mutex::new(Vec::new()),
    retired: Mutex::new(Vec::new()),
    mark: MarkShared {
        injector: Injector::new(),
        stealers: Mutex::new(Vec::new()),
        active: AtomicUsize::new(0),
        color: AtomicU8::new(1),
        marked_bytes: AtomicUsize::new(0),
        workers_in: AtomicUsize::new(0),
    },
});

/// Asks for a collection at the next safepoint of any running thread.
pub fn request_gc() {
    GC_REQUESTED.store(true, Ordering::Release);
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
        state: AtomicU8::new(ThreadState::InNative as u8),
        roots: UnsafeCell::new(None),
        heap: UnsafeCell::new(std::ptr::null_mut()),
    });
    GLOBAL.threads.lock().push(rec.clone());
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
}

/// Removes the thread from the registry. The thread must be in native state, and its heap pages
/// must already have been released.
pub fn unregister_thread(rec: &Arc<ThreadRecord>) {
    debug_assert!(matches!(rec.state(), ThreadState::InNative | ThreadState::Blocked));
    {
        // A collector holds `threads` for the whole collection, so this waits it out.
        let mut threads = GLOBAL.threads.lock();
        threads.retain(|t| !Arc::ptr_eq(t, rec));
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
    if GC_REQUESTED.load(Ordering::Acquire) {
        safepoint_slow(rec);
    }
}

#[cold]
#[inline(never)]
pub fn safepoint_slow(rec: &ThreadRecord) {
    if GC_DISABLED.load(Ordering::SeqCst) > 0 {
        return;
    }
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
            mark_worker();
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
            .compare_exchange(ThreadState::InNative as u8, ThreadState::Running as u8, Ordering::SeqCst, Ordering::SeqCst)
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

fn collect(me: &ThreadRecord) {
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

    // 3. Flip the color and mark from the roots.
    let old = POOL.live_color.load(Ordering::Relaxed);
    let color = if old == 1 { 2 } else { 1 };
    GLOBAL.mark.color.store(color, Ordering::SeqCst);
    GLOBAL.mark.marked_bytes.store(0, Ordering::SeqCst);
    {
        let mut tracer = Tracer::for_roots(color);
        for t in threads.iter() {
            if let Some(r) = unsafe { *t.roots.get() } {
                unsafe { (*r).trace_roots(&mut tracer) };
            }
        }
        for r in GLOBAL.global_roots.lock().iter() {
            r.trace_roots(&mut tracer);
        }
    }
    {
        let mut coord = GLOBAL.coord.lock();
        coord.marking = true;
        coord.mark_epoch += 1;
        GLOBAL.cv.notify_all();
    }
    mark_worker();
    // Wait for helpers to leave the mark loop before ending the phase.
    {
        let mut coord = GLOBAL.coord.lock();
        coord.marking = false;
        while GLOBAL.mark.workers_in.load(Ordering::SeqCst) > 0 {
            GLOBAL.cv.wait_for(&mut coord, std::time::Duration::from_micros(200));
        }
    }
    GLOBAL.mark.stealers.lock().clear();

    // 4. Publish the new color; sweeping happens lazily.
    POOL.live_color.store(color, Ordering::SeqCst);
    POOL.sweep_large(color);
    POOL.mark_all_unswept();
    stats::after_collection(GLOBAL.mark.marked_bytes.load(Ordering::SeqCst), threads.len());

    // 5. Resume the world.
    for t in threads.iter() {
        let _ = t.state.compare_exchange(
            ThreadState::Blocked as u8,
            ThreadState::InNative as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
    let mut coord = GLOBAL.coord.lock();
    GC_REQUESTED.store(false, Ordering::SeqCst);
    coord.collecting = false;
    coord.epoch += 1;
    drop(threads);
    GLOBAL.cv.notify_all();
}

/// Marks objects and queues them for scanning.
pub struct Tracer {
    color: u8,
    local: Option<Worker<usize>>,
}

impl Tracer {
    fn for_roots(color: u8) -> Self {
        Tracer { color, local: None }
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
        if h.try_mark(self.color) {
            match &self.local {
                Some(w) => w.push(obj as usize),
                None => GLOBAL.mark.injector.push(obj as usize),
            }
        }
    }
}

fn mark_worker() {
    let m = &GLOBAL.mark;
    let worker = Worker::new_lifo();
    m.stealers.lock().push(worker.stealer());
    let mut tracer = Tracer { color: m.color.load(Ordering::SeqCst), local: Some(worker) };
    m.active.fetch_add(1, Ordering::SeqCst);
    let mut bytes = 0usize;
    loop {
        let next = tracer.local.as_ref().unwrap().pop().or_else(|| steal(m, tracer.local.as_ref().unwrap()));
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
                        m.marked_bytes.fetch_add(bytes, Ordering::SeqCst);
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
        match m.injector.steal_batch_and_pop(local) {
            Steal::Success(o) => return Some(o),
            Steal::Retry => continue,
            Steal::Empty => {}
        }
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
    !m.injector.is_empty() || m.stealers.lock().iter().any(|s| !s.is_empty())
}
