//! A Rust panic inside a collection (issue #198): a trace hook that panics while the world is
//! stopped must end the process through the fatal handler, not leave the other threads parked
//! forever. Each case runs in a child process (this test binary, re-run with `FATAL_CASE` set)
//! with a timeout, since the failure being tested is a hang.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use catallaxy_gc::*;

#[repr(C)]
struct Obj {
    header: Header,
    next: *mut Obj,
}

static PANIC_IN_TRACE: AtomicBool = AtomicBool::new(false);

unsafe fn trace_obj(obj: *const Header, t: &mut Tracer) {
    if PANIC_IN_TRACE.load(Ordering::SeqCst) {
        panic!("trace hook failed");
    }
    t.mark((*(obj as *const Obj)).next as *const Header);
}
unsafe fn size_obj(_: *const Header) -> usize {
    std::mem::size_of::<Obj>()
}
static VT: GcVTable =
    GcVTable { trace: trace_obj, finalize: None, size: size_obj, trace_remembered: None, leaf: false };

struct Roots(*const Vec<*mut Obj>);
impl RootSource for Roots {
    fn trace_roots(&self, t: &mut Tracer) {
        for &r in unsafe { &*self.0 } {
            t.mark(r as *const Header);
        }
    }
}

struct Mutator {
    rec: Arc<ThreadRecord>,
    heap: *mut LocalHeap,
    roots: *mut Vec<*mut Obj>,
}

impl Mutator {
    fn new() -> Self {
        let rec = register_thread();
        let heap = Box::into_raw(Box::new(LocalHeap::new(rec.tid)));
        let roots = Box::into_raw(Box::new(Vec::new()));
        let src: Box<dyn RootSource> = Box::new(Roots(roots));
        unsafe { install(&rec, Box::into_raw(src), heap) };
        leave_native(&rec);
        Mutator { rec, heap, roots }
    }
    /// A rooted chain of two objects.
    fn root_chain(&mut self) {
        unsafe {
            let a = (*self.heap).alloc(std::mem::size_of::<Obj>(), &VT) as *mut Obj;
            let b = (*self.heap).alloc(std::mem::size_of::<Obj>(), &VT) as *mut Obj;
            (*a).next = b;
            (*self.roots).push(a);
        }
    }
}

fn handler(payload: &(dyn std::any::Any + Send)) -> ! {
    let msg = payload.downcast_ref::<&'static str>().copied().unwrap_or("?");
    eprintln!("fatal handler: {msg}");
    std::process::exit(101);
}

/// The child side.
#[test]
fn child() {
    let Ok(case) = std::env::var("FATAL_CASE") else { return };
    if case != "no-handler" {
        set_fatal_handler(handler);
    }
    let mut m = Mutator::new();
    m.root_chain();
    // Another mutator, running and polling safepoints: a major collection stops it.
    let started = Arc::new(AtomicBool::new(false));
    let s2 = started.clone();
    let _other = std::thread::spawn(move || {
        let mut o = Mutator::new();
        o.root_chain();
        s2.store(true, Ordering::SeqCst);
        loop {
            safepoint(&o.rec);
            std::thread::yield_now();
        }
    });
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    PANIC_IN_TRACE.store(true, Ordering::SeqCst);
    match case.as_str() {
        "major" | "no-handler" => collect_now(&m.rec),
        "minor" => collect_minor(&m.rec),
        _ => unreachable!(),
    }
    eprintln!("collection returned");
    std::process::exit(0);
}

fn run(case: &str) -> (Option<i32>, String) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child", "--exact", "--nocapture", "--test-threads=1"])
        .env("FATAL_CASE", case)
        .env("CATX_GC_THREADS", "2")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("case {case}: the child hung");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
#[cfg_attr(miri, ignore = "runs child processes")]
fn a_panic_in_a_major_collection_ends_the_process_through_the_handler() {
    let (code, err) = run("major");
    assert_eq!(code, Some(101), "{err}");
    assert!(err.contains("fatal handler: trace hook failed"), "{err}");
    assert!(!err.contains("collection returned"), "{err}");
}

#[test]
#[cfg_attr(miri, ignore = "runs child processes")]
fn a_panic_in_a_minor_collection_ends_the_process_through_the_handler() {
    let (code, err) = run("minor");
    assert_eq!(code, Some(101), "{err}");
    assert!(err.contains("fatal handler: trace hook failed"), "{err}");
}

#[test]
#[cfg_attr(miri, ignore = "runs child processes")]
fn without_a_handler_the_collector_reports_and_aborts() {
    let (code, err) = run("no-handler");
    assert_ne!(code, Some(0), "{err}");
    assert!(err.contains("fatal: internal error during a garbage collection: trace hook failed"), "{err}");
}
