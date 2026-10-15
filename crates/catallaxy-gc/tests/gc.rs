//! Collector tests using a minimal object model: a "cons" cell with two reference slots.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use catallaxy_gc::*;

#[repr(C)]
struct Cons {
    header: Header,
    car: *mut Cons,
    cdr: *mut Cons,
    val: usize,
}

static FINALIZED: AtomicUsize = AtomicUsize::new(0);

unsafe fn trace_cons(obj: *const Header, t: &mut Tracer) {
    let c = obj as *const Cons;
    t.mark((*c).car as *const Header);
    t.mark((*c).cdr as *const Header);
}
unsafe fn fin_cons(_obj: *mut Header) {
    FINALIZED.fetch_add(1, Ordering::Relaxed);
}
unsafe fn size_cons(_obj: *const Header) -> usize {
    std::mem::size_of::<Cons>()
}

static CONS_VT: GcVTable =
    GcVTable { trace: trace_cons, finalize: Some(fin_cons), size: size_cons, trace_remembered: None, leaf: false };

#[repr(C)]
struct Big {
    header: Header,
    data: [usize; 600],
}
unsafe fn trace_none(_: *const Header, _: &mut Tracer) {}
unsafe fn size_big(_: *const Header) -> usize {
    std::mem::size_of::<Big>()
}
static BIG_VT: GcVTable =
    GcVTable { trace: trace_none, finalize: None, size: size_big, trace_remembered: None, leaf: true };

/// A mutator: a shadow stack of roots plus a local heap.
struct Mutator {
    rec: Arc<ThreadRecord>,
    /// Shared with the collector, so only accessed through the raw pointer.
    heap: *mut LocalHeap,
    roots: *mut Roots,
}

struct Roots(RefCell<Vec<*mut Cons>>);
impl RootSource for Roots {
    fn trace_roots(&self, t: &mut Tracer) {
        for &r in self.0.borrow().iter() {
            t.mark(r as *const Header);
        }
    }
}

impl Mutator {
    fn new() -> Self {
        let rec = register_thread();
        let heap = Box::into_raw(Box::new(LocalHeap::new(rec.tid)));
        let roots = Box::into_raw(Box::new(Roots(RefCell::new(Vec::new()))));
        unsafe { install(&rec, roots as *const dyn RootSource, heap) };
        leave_native(&rec);
        Mutator { rec, heap, roots }
    }

    fn roots(&self) -> &Roots {
        unsafe { &*self.roots }
    }

    fn alloc(&mut self, size: usize, vt: &'static GcVTable) -> *mut Header {
        unsafe { (*self.heap).alloc(size, vt) }
    }

    fn cons(&mut self, car: *mut Cons, cdr: *mut Cons, val: usize) -> *mut Cons {
        let p = self.alloc(std::mem::size_of::<Cons>(), &CONS_VT) as *mut Cons;
        unsafe {
            (*p).car = car;
            (*p).cdr = cdr;
            (*p).val = val;
        }
        p
    }

    fn finish(self) {
        enter_native(&self.rec);
        unregister_thread(&self.rec);
        unsafe {
            (*self.heap).release_pages();
            drop(Box::from_raw(self.heap));
            drop(Box::from_raw(self.roots));
        }
    }
}

fn list_sum(mut p: *mut Cons) -> usize {
    let mut s = 0;
    while !p.is_null() {
        unsafe {
            s += (*p).val;
            p = (*p).cdr;
        }
    }
    s
}

/// Smaller workloads under Miri, which interprets every instruction.
fn scale(n: usize) -> usize {
    if cfg!(miri) {
        (n / 500).max(4)
    } else {
        n
    }
}
const ROUNDS: usize = if cfg!(miri) { 3 } else { 30 };
const PER_ROUND: usize = if cfg!(miri) { 40 } else { 2000 };

// Tests share global collector state, so they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn rooted_list_survives_and_garbage_is_reclaimed() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mutator::new();
    let mut list: *mut Cons = std::ptr::null_mut();
    for i in 0..1000 {
        list = m.cons(std::ptr::null_mut(), list, i);
    }
    m.roots().0.borrow_mut().push(list);
    // Lots of garbage.
    for i in 0..scale(200_000) {
        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
        safepoint(&m.rec);
    }
    let before = FINALIZED.load(Ordering::Relaxed);
    collect_now(&m.rec);
    collect_now(&m.rec);
    // Allocate again so lazy sweeping runs.
    for i in 0..scale(200_000) {
        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
    }
    assert!(FINALIZED.load(Ordering::Relaxed) > before, "garbage was finalized");
    assert_eq!(list_sum(list), (0..1000).sum());
    m.roots().0.borrow_mut().clear();
    m.finish();
}

#[test]
#[cfg_attr(miri, ignore = "needs tens of megabytes of garbage to exceed the kept reserve")]
fn empty_segments_are_returned_to_the_os() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mutator::new();
    let mut list: *mut Cons = std::ptr::null_mut();
    for i in 0..1000 {
        list = m.cons(std::ptr::null_mut(), list, i);
    }
    m.roots().0.borrow_mut().push(list);
    let start = total_heap_bytes();
    let released = stats::released_bytes();
    // Garbage until the heap has grown by 64 MB. (Earlier tests may have left empty pages in the
    // pool, which are used first, so the amount of garbage needed varies.) No safepoints, so no
    // collection reclaims any of it early.
    let mut i = 0;
    while total_heap_bytes() < start + (64 << 20) && i < 20_000_000 {
        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
        i += 1;
    }
    let grown = total_heap_bytes();
    // The first collection finds the garbage dead; the second sweeps it, leaving whole
    // segments empty, and returns all but a reserve for the next allocation budget.
    collect_now(&m.rec);
    collect_now(&m.rec);
    let after = total_heap_bytes();
    let freed = stats::released_bytes() - released;
    // The heap keeps working, including when it has to map segments again.
    for i in 0..1_400_000 {
        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
    }
    collect_now(&m.rec);
    collect_now(&m.rec);
    let sum = list_sum(list);
    m.roots().0.borrow_mut().clear();
    // Unregister before asserting: a failed assertion must not leave a running mutator behind
    // for the other tests' collections to wait on.
    m.finish();
    assert!(grown >= start + (64 << 20), "heap grew from {start} to {grown}");
    assert!(freed >= 40 << 20, "released {freed} bytes");
    assert!(after + (40 << 20) <= grown, "heap went from {grown} to {after}");
    assert_eq!(sum, (0..1000).sum());
}

#[test]
fn large_objects_are_collected() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mutator::new();
    let keep = m.alloc(std::mem::size_of::<Big>(), &BIG_VT) as *mut Big;
    unsafe { (*keep).data[599] = 42 };
    // Hold the big object through a cons cell's car slot (cast; tracing only needs the header).
    let holder = m.cons(keep as *mut Cons, std::ptr::null_mut(), 0);
    m.roots().0.borrow_mut().push(holder);
    for _ in 0..scale(1000) {
        let _ = m.alloc(std::mem::size_of::<Big>(), &BIG_VT);
    }
    collect_now(&m.rec);
    assert_eq!(unsafe { (*keep).data[599] }, 42);
    m.roots().0.borrow_mut().clear();
    m.finish();
}

#[test]
fn parallel_mutators_with_collections() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let start = stats::collections();
    let handles: Vec<_> = (0..if cfg!(miri) { 2 } else { 6 })
        .map(|t| {
            std::thread::spawn(move || {
                let mut m = Mutator::new();
                let mut total = 0;
                for round in 0..ROUNDS {
                    let mut list: *mut Cons = std::ptr::null_mut();
                    m.roots().0.borrow_mut().push(std::ptr::null_mut());
                    for i in 0..PER_ROUND {
                        list = m.cons(std::ptr::null_mut(), list, i + t + round);
                        m.roots().0.borrow_mut()[0] = list;
                        // garbage
                        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), 0);
                        safepoint(&m.rec);
                    }
                    if round % (ROUNDS / 3).max(1) == 0 {
                        collect_now(&m.rec);
                    }
                    total += list_sum(list);
                    m.roots().0.borrow_mut().clear();
                }
                // Simulate blocking I/O: the collector must not wait for us.
                blocking(&m.rec, || std::thread::sleep(std::time::Duration::from_millis(5)));
                m.finish();
                total
            })
        })
        .collect();
    for (t, h) in handles.into_iter().enumerate() {
        let expect: usize = (0..ROUNDS).map(|r| (0..PER_ROUND).map(|i| i + t + r).sum::<usize>()).sum();
        assert_eq!(h.join().unwrap(), expect);
    }
    assert!(stats::collections() > start);
}

#[test]
fn object_lock_excludes() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mutator::new();
    let c = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), 0) as usize;
    let counter = Arc::new(AtomicUsize::new(0));
    let hs: Vec<_> = (0..4)
        .map(|i| {
            let counter = counter.clone();
            std::thread::spawn(move || {
                let h = unsafe { &*(c as *const Header) };
                for _ in 0..scale(10_000) {
                    let _g = ObjLockGuard::new(h, 10 + i as u16);
                    // Non-atomic read-modify-write protected by the object lock.
                    let v = counter.load(Ordering::Relaxed);
                    counter.store(v + 1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(counter.load(Ordering::Relaxed), 4 * scale(10_000));
    let h = unsafe { &*(c as *const Header) };
    // Reentrancy.
    h.lock(3);
    assert!(h.try_lock(3));
    assert!(!h.try_lock(4));
    h.unlock();
    assert!(h.is_locked_by(3));
    h.unlock();
    assert!(!h.is_locked());
    assert!(h.identity_hash() != 0);
    assert_eq!(h.identity_hash(), h.identity_hash());
    m.finish();
}

fn tree(m: &mut Mutator, depth: u32, val: usize) -> *mut Cons {
    if depth == 0 {
        return std::ptr::null_mut();
    }
    let l = tree(m, depth - 1, val * 2);
    // Rooted while the right subtree is built: allocation does not collect, but be explicit.
    let r = tree(m, depth - 1, val * 2 + 1);
    m.cons(l, r, val)
}

fn tree_sum(p: *mut Cons) -> usize {
    if p.is_null() {
        return 0;
    }
    unsafe { (*p).val + tree_sum((*p).car) + tree_sum((*p).cdr) }
}

/// Resets the mark stack limit even if the test fails.
struct StackLimit;
impl Drop for StackLimit {
    fn drop(&mut self) {
        set_mark_stack_limit(0);
    }
}

/// A mark stack that cannot grow (here: capped at 4 entries) leaves objects unmarked and the
/// collection rescans until it has marked everything live, in major and minor collections
/// (issue #139).
#[test]
fn marking_survives_a_mark_stack_that_cannot_grow() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mutator::new();
    let depth = if cfg!(miri) { 6 } else { 12 };
    let want: usize = (1..1 << depth).sum();
    let _limit = StackLimit;
    set_mark_stack_limit(4);
    // A young tree: a minor collection marks it from the roots.
    let a = tree(&mut m, depth, 1);
    m.roots().0.borrow_mut().push(a);
    let before = stats::mark_rescans();
    collect_minor(&m.rec);
    assert!(stats::mark_rescans() > before, "the minor collection rescanned");
    // An old tree, and a young one hung under an old node (found through the remembered set
    // only if the barrier ran; here the old root is rooted, so through the roots).
    let before = stats::mark_rescans();
    collect_now(&m.rec);
    assert!(stats::mark_rescans() > before, "the major collection rescanned");
    let b = tree(&mut m, depth, 1);
    m.roots().0.borrow_mut().push(b);
    for round in 0..3 {
        // Garbage, then collections; then reuse what they freed, overwriting anything wrongly
        // freed.
        for i in 0..scale(100_000) {
            let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
        }
        if round == 1 {
            collect_now(&m.rec);
        } else {
            collect_minor(&m.rec);
        }
        for i in 0..scale(100_000) {
            let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
        }
        assert_eq!(tree_sum(a), want);
        assert_eq!(tree_sum(b), want);
    }
    m.roots().0.borrow_mut().clear();
    m.finish();
}
