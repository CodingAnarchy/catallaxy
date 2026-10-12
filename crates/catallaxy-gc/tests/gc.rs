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

static CONS_VT: GcVTable = GcVTable { trace: trace_cons, finalize: Some(fin_cons), size: size_cons };

#[repr(C)]
struct Big {
    header: Header,
    data: [usize; 600],
}
unsafe fn trace_none(_: *const Header, _: &mut Tracer) {}
unsafe fn size_big(_: *const Header) -> usize {
    std::mem::size_of::<Big>()
}
static BIG_VT: GcVTable = GcVTable { trace: trace_none, finalize: None, size: size_big };

/// A mutator: a shadow stack of roots plus a local heap.
struct Mutator {
    rec: Arc<ThreadRecord>,
    heap: Box<LocalHeap>,
    roots: Box<Roots>,
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
        let mut heap = Box::new(LocalHeap::new(rec.tid));
        let roots = Box::new(Roots(RefCell::new(Vec::new())));
        unsafe { install(&rec, &*roots as *const dyn RootSource, &mut *heap as *mut LocalHeap) };
        leave_native(&rec);
        Mutator { rec, heap, roots }
    }

    fn cons(&mut self, car: *mut Cons, cdr: *mut Cons, val: usize) -> *mut Cons {
        let p = self.heap.alloc(std::mem::size_of::<Cons>(), &CONS_VT) as *mut Cons;
        unsafe {
            (*p).car = car;
            (*p).cdr = cdr;
            (*p).val = val;
        }
        p
    }

    fn finish(mut self) {
        enter_native(&self.rec);
        // Pages must be released while the world cannot be collecting.
        self.heap.release_pages();
        unregister_thread(&self.rec);
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
    m.roots.0.borrow_mut().push(list);
    // Lots of garbage.
    for i in 0..200_000 {
        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
        safepoint(&m.rec);
    }
    let before = FINALIZED.load(Ordering::Relaxed);
    collect_now(&m.rec);
    collect_now(&m.rec);
    // Allocate again so lazy sweeping runs.
    for i in 0..200_000 {
        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), i);
    }
    assert!(FINALIZED.load(Ordering::Relaxed) > before, "garbage was finalized");
    assert_eq!(list_sum(list), (0..1000).sum());
    m.roots.0.borrow_mut().clear();
    m.finish();
}

#[test]
fn large_objects_are_collected() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mutator::new();
    let keep = m.heap.alloc(std::mem::size_of::<Big>(), &BIG_VT) as *mut Big;
    unsafe { (*keep).data[599] = 42 };
    // Hold the big object through a cons cell's car slot (cast; tracing only needs the header).
    let holder = m.cons(keep as *mut Cons, std::ptr::null_mut(), 0);
    m.roots.0.borrow_mut().push(holder);
    for _ in 0..1000 {
        let _ = m.heap.alloc(std::mem::size_of::<Big>(), &BIG_VT);
    }
    collect_now(&m.rec);
    assert_eq!(unsafe { (*keep).data[599] }, 42);
    m.roots.0.borrow_mut().clear();
    m.finish();
}

#[test]
fn parallel_mutators_with_collections() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let start = stats::collections();
    let handles: Vec<_> = (0..6)
        .map(|t| {
            std::thread::spawn(move || {
                let mut m = Mutator::new();
                let mut total = 0;
                for round in 0..30 {
                    let mut list: *mut Cons = std::ptr::null_mut();
                    m.roots.0.borrow_mut().push(std::ptr::null_mut());
                    for i in 0..2000 {
                        list = m.cons(std::ptr::null_mut(), list, i + t + round);
                        m.roots.0.borrow_mut()[0] = list;
                        // garbage
                        let _ = m.cons(std::ptr::null_mut(), std::ptr::null_mut(), 0);
                        safepoint(&m.rec);
                    }
                    if round % 10 == 0 {
                        collect_now(&m.rec);
                    }
                    total += list_sum(list);
                    m.roots.0.borrow_mut().clear();
                }
                // Simulate blocking I/O: the collector must not wait for us.
                blocking(&m.rec, || std::thread::sleep(std::time::Duration::from_millis(5)));
                m.finish();
                total
            })
        })
        .collect();
    for (t, h) in handles.into_iter().enumerate() {
        let expect: usize = (0..30).map(|r| (0..2000).map(|i| i + t + r).sum::<usize>()).sum();
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
                for _ in 0..10_000 {
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
    assert_eq!(counter.load(Ordering::Relaxed), 40_000);
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
