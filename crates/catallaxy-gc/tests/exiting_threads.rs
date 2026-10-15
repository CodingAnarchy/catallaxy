//! A thread that exits hands its heap pages back when it unregisters, with no collection
//! running. Before, it released them afterwards, so a collection could run while live objects
//! sat in pages the heap did not list: after a mark-stack overflow (#139) it could not rescan
//! them and left their children unmarked, and if the pages reached the global lists part way
//! through it swept them with a color they were never marked in. Live objects were freed (the
//! differential fuzzer's crash after #218). Its own test binary: the global roots it adds stay.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use catallaxy_gc::*;

#[repr(C)]
struct Node {
    header: Header,
    left: *mut Node,
    right: *mut Node,
    val: usize,
}

static FINALIZED: AtomicUsize = AtomicUsize::new(0);

unsafe fn trace_node(obj: *const Header, t: &mut Tracer) {
    let n = obj as *const Node;
    t.mark((*n).left as *const Header);
    t.mark((*n).right as *const Header);
}
unsafe fn fin_node(_: *mut Header) {
    FINALIZED.fetch_add(1, Ordering::Relaxed);
}
unsafe fn size_node(_: *const Header) -> usize {
    std::mem::size_of::<Node>()
}
static VT: GcVTable =
    GcVTable { trace: trace_node, finalize: Some(fin_node), size: size_node, trace_remembered: None, leaf: false };

/// Garbage of the deterministic test, counted on its own.
static GARBAGE_FINALIZED: AtomicUsize = AtomicUsize::new(0);
unsafe fn fin_garbage(_: *mut Header) {
    GARBAGE_FINALIZED.fetch_add(1, Ordering::Relaxed);
}
static GARBAGE_VT: GcVTable =
    GcVTable { trace: trace_node, finalize: Some(fin_garbage), size: size_node, trace_remembered: None, leaf: false };

/// Trees published by threads that have exited: a global root.
struct Published(Mutex<Vec<usize>>);
static PUBLISHED: Published = Published(Mutex::new(Vec::new()));
struct PublishedRoots;
impl RootSource for PublishedRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        for &p in PUBLISHED.0.lock().unwrap().iter() {
            t.mark(p as *const Header);
        }
    }
}

struct NoRoots;
impl RootSource for NoRoots {
    fn trace_roots(&self, _: &mut Tracer) {}
}

struct Mutator {
    rec: Arc<ThreadRecord>,
    heap: *mut LocalHeap,
}

impl Mutator {
    fn new() -> Self {
        let rec = register_thread();
        let heap = Box::into_raw(Box::new(LocalHeap::new(rec.tid)));
        let roots: Box<dyn RootSource> = Box::new(NoRoots);
        unsafe { install(&rec, Box::into_raw(roots), heap) };
        leave_native(&rec);
        Mutator { rec, heap }
    }

    fn node(&mut self, left: *mut Node, right: *mut Node, val: usize) -> *mut Node {
        self.node_of(&VT, left, right, val)
    }

    fn node_of(&mut self, vt: &'static GcVTable, left: *mut Node, right: *mut Node, val: usize) -> *mut Node {
        let p = unsafe { (*self.heap).alloc(std::mem::size_of::<Node>(), vt) } as *mut Node;
        unsafe {
            (*p).left = left;
            (*p).right = right;
            (*p).val = val;
        }
        p
    }

    fn tree(&mut self, depth: u32, val: usize) -> *mut Node {
        if depth == 0 {
            return std::ptr::null_mut();
        }
        let l = self.tree(depth - 1, val * 2);
        let r = self.tree(depth - 1, val * 2 + 1);
        self.node(l, r, val)
    }

    /// Exits as `Vm::shutdown` does: unregisters, then releases the heap's pages (a no-op now).
    /// `pause` widens the gap between the two, as a busy machine may.
    fn exit_after(self, release_after: bool, pause: std::time::Duration) {
        enter_native(&self.rec);
        unregister_thread(&self.rec);
        std::thread::sleep(pause);
        if release_after {
            unsafe { (*self.heap).release_pages() };
        }
        // The heap is leaked, as a thread that never releases it would leave it.
    }

    fn exit(self, release_after: bool) {
        self.exit_after(release_after, std::time::Duration::ZERO)
    }
}

fn sum(p: *mut Node) -> usize {
    if p.is_null() {
        return 0;
    }
    unsafe { (*p).val + sum((*p).left) + sum((*p).right) }
}

static SERIAL: Mutex<()> = Mutex::new(());

fn roots_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| add_global_roots(Box::new(PublishedRoots)));
}

/// Deterministic: a thread that unregisters and never releases its heap still has its pages
/// swept, so its garbage is finalized. (Before, the pages were lost to the collector.)
#[test]
fn unregistering_hands_the_heap_pages_back() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n = if cfg!(miri) { 20 } else { 2000 };
    let before = GARBAGE_FINALIZED.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        let mut m = Mutator::new();
        for i in 0..n {
            let _ = m.node_of(&GARBAGE_VT, std::ptr::null_mut(), std::ptr::null_mut(), i);
        }
        m.exit(false);
    })
    .join()
    .unwrap();
    let main = Mutator::new();
    collect_to_free(&main.rec);
    collect_to_free(&main.rec);
    // Allocate enough to make the lazy sweeper visit the pages too.
    let mut main = main;
    for i in 0..n * 4 {
        let _ = main.node(std::ptr::null_mut(), std::ptr::null_mut(), i);
    }
    collect_to_free(&main.rec);
    assert!(GARBAGE_FINALIZED.load(Ordering::Relaxed) >= before + n, "the exited thread's garbage was swept");
    main.exit(true);
}

/// Stress: threads publish trees and exit while the main thread collects with a mark stack of
/// two entries (so every collection overflows and rescans) and parallel mark helpers.
#[test]
fn trees_published_by_exiting_threads_survive_overflowing_collections() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    roots_once();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_mark_stack_limit(0);
        }
    }
    let _reset = Reset;
    set_mark_stack_limit(2);
    let (threads, depth) = if cfg!(miri) { (2, 4) } else { (200, 7) };
    let want: usize = (1..1 << depth).sum();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let collector = std::thread::spawn(move || {
        let m = Mutator::new();
        while !stop2.load(Ordering::Relaxed) {
            collect_now(&m.rec);
            safepoint(&m.rec);
        }
        m.exit(true);
    });
    for round in 0..threads {
        std::thread::spawn(move || {
            let mut m = Mutator::new();
            let t = m.tree(depth, 1);
            PUBLISHED.0.lock().unwrap().push(t as usize);
            // Garbage, so that pages fill and new ones are taken.
            for i in 0..round % 50 {
                let _ = m.node(std::ptr::null_mut(), std::ptr::null_mut(), i);
            }
            safepoint(&m.rec);
            m.exit_after(true, std::time::Duration::from_micros((round % 7) as u64 * 50));
        })
        .join()
        .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    collector.join().unwrap();
    // Reuse whatever was wrongly freed, then check every tree.
    let mut m = Mutator::new();
    collect_now(&m.rec);
    for i in 0..if cfg!(miri) { 100 } else { 200_000 } {
        let _ = m.node(std::ptr::null_mut(), std::ptr::null_mut(), i);
    }
    for &t in PUBLISHED.0.lock().unwrap().iter() {
        assert_eq!(sum(t as *mut Node), want);
    }
    m.exit(true);
}
