//! Fibers: lightweight cooperative threads for I/O-bound work (M:N scheduling).
//!
//! A fiber runs on the OS thread that spawned it, for its whole life. Pinning keeps every rule of
//! the object model intact: a fiber's objects are owned by its thread, so ownership checks,
//! thread-local young collection and `with lock` work unchanged, and a fiber block may capture
//! owned values. Many fibers share one thread and interleave only where one of them waits.
//!
//! Each fiber has its own native stack (a stackful coroutine) and its own [`Vm`] context
//! (registers, frames, handles), registered with the thread's main VM so the collector scans
//! parked fibers. The thread's main context runs the scheduler whenever it waits: in `join`, a
//! channel operation, `sleep`, file or console I/O, or at the end of the thread.
//!
//! Waiting in a fiber parks it instead of blocking the thread:
//! - channel send/receive and fiber `join` register a waker and park;
//! - `sleep` sets a timer and parks;
//! - blocking system calls (files, stdin) run on a small shared pool of I/O threads while the
//!   fiber is parked.
//!
//! A fiber never parks while it holds a lock keyed by its OS thread (`with lock`, `Mutex`
//! closures): a sibling fiber could otherwise enter the same lock. It blocks the whole thread
//! instead.

use std::cell::{Cell, RefCell};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use catallaxy_runtime::object::Wake;
use catallaxy_runtime::{RtResult, Trap, Value};
use catallaxy_vm::Vm;
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use parking_lot::{Condvar, Mutex};

/// Native stack per fiber. Interpreted calls do not use it; only nested native calls do.
const FIBER_NATIVE_STACK: usize = 256 * 1024;
/// Register stack per fiber, in values (lazily committed).
const FIBER_REGISTERS: usize = 64 * 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The main context of a thread, in waker ids.
const ROOT: u64 = 0;

/// Cross-thread side of a scheduler: wakeups arrive here.
struct Shared {
    woken: Mutex<Vec<u64>>,
    cv: Condvar,
}

impl Shared {
    fn wake(&self, id: u64) {
        self.woken.lock().push(id);
        self.cv.notify_one();
    }
}

#[derive(Clone)]
struct Waker {
    shared: Arc<Shared>,
    id: u64,
    /// Set by `wake`; a park that finds it set returns at once (no lost wakeups).
    flag: Arc<AtomicBool>,
}

impl Waker {
    fn wake(&self) {
        self.flag.store(true, Ordering::Release);
        self.shared.wake(self.id);
    }
    fn into_wake(self) -> Wake {
        Arc::new(move || self.wake())
    }
}

enum Outcome {
    Running,
    /// The result is pinned in the thread's main VM under the fiber id.
    Done,
    Panicked(String),
    Joined,
}

/// What other fibers (and the scheduler) know about a fiber.
struct Ctl {
    outcome: Mutex<Outcome>,
    joiners: Mutex<Vec<Wake>>,
}

struct Fiber {
    coro: Coroutine<(), (), (), DefaultStack>,
    vm: *mut Vm,
    flag: Arc<AtomicBool>,
    yielder: Cell<*const Yielder<(), ()>>,
}

struct Scheduler {
    shared: Arc<Shared>,
    root_flag: Arc<AtomicBool>,
    /// Parked or ready fibers (a running fiber is taken out while it runs).
    fibers: HashMap<u64, Fiber>,
    ready: VecDeque<u64>,
    /// (deadline, id) min-heap via `Reverse`.
    timers: BinaryHeap<std::cmp::Reverse<(Instant, u64)>>,
    ctl: HashMap<u64, Arc<Ctl>>,
}

impl Scheduler {
    fn new() -> Self {
        Scheduler {
            shared: Arc::new(Shared { woken: Mutex::new(Vec::new()), cv: Condvar::new() }),
            root_flag: Arc::new(AtomicBool::new(false)),
            fibers: HashMap::new(),
            ready: VecDeque::new(),
            timers: BinaryHeap::new(),
            ctl: HashMap::new(),
        }
    }
}

thread_local! {
    static SCHED: RefCell<Option<Scheduler>> = const { RefCell::new(None) };
    /// The running fiber (0 in the main context) and its yielder.
    static CURRENT: Cell<u64> = const { Cell::new(ROOT) };
    static YIELDER: Cell<*const Yielder<(), ()>> = const { Cell::new(std::ptr::null()) };
}

fn with_sched<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    SCHED.with(|s| f(s.borrow_mut().get_or_insert_with(Scheduler::new)))
}

/// True while a fiber (not the thread's main context) is running.
pub fn in_fiber() -> bool {
    CURRENT.with(|c| c.get()) != ROOT
}

/// True if this thread has fibers that have not finished.
pub fn has_fibers() -> bool {
    SCHED.with(|s| s.borrow().as_ref().is_some_and(|s| !s.fibers.is_empty()))
}

/// A waker for the running fiber, or for the main context.
fn current_waker() -> Waker {
    let id = CURRENT.with(|c| c.get());
    let flag = if id == ROOT {
        with_sched(|s| s.root_flag.clone())
    } else {
        RUNNING_FLAG.with(|f| f.borrow().clone()).expect("running fiber flag")
    };
    Waker { shared: with_sched(|s| s.shared.clone()), id, flag }
}

thread_local! {
    /// The running fiber's wake flag (it is out of the fiber table while it runs).
    static RUNNING_FLAG: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

// ---- spawning and running ----------------------------------------------------------------------

/// Starts a fiber running `closure` on this thread. It first runs when this thread's main
/// context waits (or when another fiber parks). Returns the fiber id.
pub fn spawn(vm: &mut Vm, closure: Value) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let fvm = Box::into_raw(vm.new_fiber(FIBER_REGISTERS));
    unsafe { (*fvm).extra_roots().push(closure) };
    let ctl = Arc::new(Ctl { outcome: Mutex::new(Outcome::Running), joiners: Mutex::new(Vec::new()) });
    let ctl2 = ctl.clone();
    let vm_ptr = fvm as usize;
    let stack = DefaultStack::new(FIBER_NATIVE_STACK).expect("fiber stack");
    let coro = Coroutine::with_stack(stack, move |y: &Yielder<(), ()>, ()| {
        YIELDER.with(|c| c.set(y as *const _));
        // SAFETY: the VM lives until the scheduler releases it after this body returns.
        let vm = unsafe { &mut *(vm_ptr as *mut Vm) };
        let closure = vm.extra_roots()[0];
        let r = vm.call_value(closure, &[]);
        let outcome = match r {
            Ok(v) => {
                vm.pin(id, v);
                Outcome::Done
            }
            Err(Trap::Panic(msg)) => {
                let mut full = msg;
                for line in vm.panic_trace.take().unwrap_or_default().iter().take(8) {
                    full.push_str(&format!("\n    at {line}"));
                }
                Outcome::Panicked(full)
            }
            Err(Trap::Exit(code)) => {
                crate::flush();
                std::process::exit(code);
            }
            Err(Trap::Unwind { .. }) => Outcome::Panicked("break or return out of a fiber block".into()),
        };
        *ctl2.outcome.lock() = outcome;
        for w in std::mem::take(&mut *ctl2.joiners.lock()) {
            w();
        }
    });
    with_sched(|s| {
        s.fibers.insert(
            id,
            Fiber { coro, vm: fvm, flag: Arc::new(AtomicBool::new(false)), yielder: Cell::new(std::ptr::null()) },
        );
        s.ctl.insert(id, ctl);
        s.ready.push_back(id);
    });
    id
}

/// Resumes fiber `id` until it parks or finishes. Main context only.
fn resume(id: u64) {
    let Some(fiber) = with_sched(|s| s.fibers.remove(&id)) else { return };
    let prev_y = YIELDER.with(|c| c.replace(fiber.yielder.get()));
    CURRENT.with(|c| c.set(id));
    RUNNING_FLAG.with(|f| *f.borrow_mut() = Some(fiber.flag.clone()));
    let mut fiber = fiber;
    let r = fiber.coro.resume(());
    fiber.yielder.set(YIELDER.with(|c| c.get()));
    RUNNING_FLAG.with(|f| *f.borrow_mut() = None);
    CURRENT.with(|c| c.set(ROOT));
    YIELDER.with(|c| c.set(prev_y));
    match r {
        CoroutineResult::Yield(()) => {
            let woken = fiber.flag.load(Ordering::Acquire);
            with_sched(|s| {
                s.fibers.insert(id, fiber);
                if woken {
                    s.ready.push_back(id);
                }
            });
        }
        CoroutineResult::Return(()) => {
            let vm = fiber.vm;
            drop(fiber);
            // SAFETY: the coroutine finished; nothing else references its VM.
            Vm::release_fiber(unsafe { Box::from_raw(vm) });
        }
    }
}

/// Moves woken fibers and expired timers to the ready queue.
fn absorb() {
    let now = Instant::now();
    with_sched(|s| {
        let woken = std::mem::take(&mut *s.shared.woken.lock());
        for id in woken {
            if id != ROOT && s.fibers.contains_key(&id) && !s.ready.contains(&id) {
                s.ready.push_back(id);
            }
        }
        while let Some(std::cmp::Reverse((t, id))) = s.timers.peek().copied() {
            if t > now {
                break;
            }
            s.timers.pop();
            if id == ROOT {
                s.root_flag.store(true, Ordering::Release);
            } else if let Some(f) = s.fibers.get(&id) {
                f.flag.store(true, Ordering::Release);
                if !s.ready.contains(&id) {
                    s.ready.push_back(id);
                }
            }
        }
    });
}

/// Runs this thread's fibers from its main context until `done()` holds. While nothing is
/// runnable the thread sleeps in the native state (so collections proceed without it) until a
/// wakeup or the next timer.
fn run_until(vm: &mut Vm, mut done: impl FnMut() -> bool) {
    debug_assert!(!in_fiber());
    loop {
        absorb();
        if done() {
            return;
        }
        if let Some(id) = with_sched(|s| s.ready.pop_front()) {
            resume(id);
            catallaxy_gc::safepoint(&vm.m.rec);
            continue;
        }
        let (shared, deadline) = with_sched(|s| (s.shared.clone(), s.timers.peek().map(|r| r.0 .0)));
        catallaxy_gc::blocking(&vm.m.rec, || {
            let mut w = shared.woken.lock();
            if w.is_empty() {
                match deadline {
                    Some(t) => {
                        let now = Instant::now();
                        if t > now {
                            shared.cv.wait_for(&mut w, t - now);
                        }
                    }
                    None => {
                        shared.cv.wait_for(&mut w, Duration::from_millis(100));
                    }
                }
            }
        });
    }
}

/// Suspends the running fiber until its waker fires. Returns at once if it already fired.
fn park() {
    let flag = RUNNING_FLAG.with(|f| f.borrow().clone()).expect("park outside a fiber");
    if flag.swap(false, Ordering::AcqRel) {
        return;
    }
    let y = YIELDER.with(|c| c.get());
    // SAFETY: set by the running fiber's body; valid while it runs.
    unsafe { (*y).suspend(()) };
    flag.store(false, Ordering::Release);
}

/// Runs every fiber that finished, on any thread, at the end of the thread's main code.
pub fn drain(vm: &mut Vm) {
    if in_fiber() || !has_fibers() {
        return;
    }
    run_until(vm, || !has_fibers());
}

// ---- waiting -----------------------------------------------------------------------------------

/// Waits for an event without blocking the thread when possible. `arm(wake)` must check the
/// condition and, if it does not hold, arrange for `wake` to be called when it may (under the
/// same lock as the check); it returns true if the caller must wait. Returns false if this
/// context cannot wait cooperatively (no fibers, or a thread-keyed lock is held): the caller
/// then blocks the thread as before. After a true return, re-check the condition.
pub fn wait_event(vm: &mut Vm, arm: impl FnOnce(Wake) -> bool) -> bool {
    if !vm.park_allowed() {
        return false;
    }
    if in_fiber() {
        let w = current_waker();
        if arm(w.into_wake()) {
            park();
        }
        return true;
    }
    if !has_fibers() {
        return false;
    }
    let w = current_waker();
    let flag = w.flag.clone();
    if arm(w.into_wake()) {
        run_until(vm, || flag.swap(false, Ordering::AcqRel));
    }
    true
}

/// Sleeps without blocking the thread when it has fibers (or is one).
pub fn sleep(vm: &mut Vm, d: Duration) {
    let deadline = Instant::now() + d;
    if !vm.park_allowed() || (!in_fiber() && !has_fibers()) {
        crate::blocking(vm, || std::thread::sleep(d));
        return;
    }
    let id = CURRENT.with(|c| c.get());
    while Instant::now() < deadline {
        with_sched(|s| s.timers.push(std::cmp::Reverse((deadline, id))));
        if in_fiber() {
            park();
        } else {
            let flag = with_sched(|s| s.root_flag.clone());
            run_until(vm, || flag.swap(false, Ordering::AcqRel) || Instant::now() >= deadline);
        }
    }
}

/// Lets other fibers run. In the main context, runs each ready fiber once.
pub fn yield_now(vm: &mut Vm) {
    if in_fiber() {
        if !vm.park_allowed() {
            return;
        }
        // Suspend with the wake flag raised: the scheduler re-queues us behind the others.
        let flag = RUNNING_FLAG.with(|f| f.borrow().clone()).expect("running fiber flag");
        flag.store(true, Ordering::Release);
        let y = YIELDER.with(|c| c.get());
        unsafe { (*y).suspend(()) };
        flag.store(false, Ordering::Release);
    } else if has_fibers() {
        let n = with_sched(|s| s.ready.len());
        for _ in 0..n {
            absorb();
            match with_sched(|s| s.ready.pop_front()) {
                Some(id) => resume(id),
                None => break,
            }
        }
    }
}

// ---- joining -----------------------------------------------------------------------------------

/// Waits for fiber `id` (of this thread) and returns its result, or the panic message.
pub fn join(vm: &mut Vm, id: u64) -> RtResult<Result<Value, String>> {
    let Some(ctl) = with_sched(|s| s.ctl.get(&id).cloned()) else {
        return Err(Trap::Panic(
            "FiberError: no such fiber on this thread (fibers are joined on the thread that spawned them)".into(),
        ));
    };
    loop {
        if !matches!(*ctl.outcome.lock(), Outcome::Running) {
            break;
        }
        let ctl2 = ctl.clone();
        let cooperative = wait_event(vm, move |wake| {
            let o = ctl2.outcome.lock();
            if !matches!(*o, Outcome::Running) {
                return false;
            }
            ctl2.joiners.lock().push(wake);
            true
        });
        if !cooperative {
            // Only reachable while holding a thread-keyed lock: the fiber cannot run until the
            // lock is released, so this would never finish.
            return Err(Trap::Panic("FiberError: join while holding a lock would deadlock".into()));
        }
    }
    with_sched(|s| s.ctl.remove(&id));
    let outcome = std::mem::replace(&mut *ctl.outcome.lock(), Outcome::Joined);
    Ok(match outcome {
        Outcome::Done => Ok(vm.unpin(id).unwrap_or(Value::NIL)),
        Outcome::Panicked(m) => Err(m),
        Outcome::Joined => Err("fiber was already joined".into()),
        Outcome::Running => unreachable!(),
    })
}

/// Whether fiber `id` of this thread has finished.
pub fn is_done(id: u64) -> bool {
    with_sched(|s| s.ctl.get(&id).is_none_or(|c| !matches!(*c.outcome.lock(), Outcome::Running)))
}

// ---- blocking system calls ---------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Threads that run blocking system calls for parked fibers (`CATX_IO_THREADS`, default 16).
/// They never touch the heap.
struct IoPool {
    queue: Mutex<VecDeque<Job>>,
    cv: Condvar,
}

static IO: LazyLock<IoPool> = LazyLock::new(|| {
    let n = std::env::var("CATX_IO_THREADS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(16).max(1);
    for i in 0..n {
        let _ = std::thread::Builder::new().name(format!("catx-io-{i}")).spawn(|| loop {
            let job = {
                let mut q = IO.queue.lock();
                loop {
                    if let Some(j) = q.pop_front() {
                        break j;
                    }
                    IO.cv.wait(&mut q);
                }
            };
            job();
        });
    }
    IoPool { queue: Mutex::new(VecDeque::new()), cv: Condvar::new() }
});

/// Runs a blocking call that does not touch the heap. In a fiber (or a main context that has
/// fibers) it runs on the I/O pool while other fibers run; otherwise the thread blocks in the
/// native state, as `crate::blocking` does.
pub fn blocking_io<R: Send>(vm: &mut Vm, f: impl FnOnce() -> R + Send) -> R {
    if !vm.park_allowed() || (!in_fiber() && !has_fibers()) {
        return crate::blocking(vm, f);
    }
    let done = Arc::new(AtomicBool::new(false));
    let mut result: Option<std::thread::Result<R>> = None;
    let w = current_waker();
    {
        let slot = &mut result as *mut Option<std::thread::Result<R>> as usize;
        let (done2, w2) = (done.clone(), w.clone());
        let job: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            // SAFETY: the waiting context stays suspended until `done` is set.
            unsafe { *(slot as *mut Option<std::thread::Result<R>>) = Some(r) };
            done2.store(true, Ordering::Release);
            w2.wake();
        });
        // SAFETY: the job borrows from this frame, which stays alive (parked or waiting) until
        // the job has signaled `done`, after which the job touches nothing.
        let job: Job = unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Job>(job) };
        IO.queue.lock().push_back(job);
        IO.cv.notify_one();
    }
    while !done.load(Ordering::Acquire) {
        if in_fiber() {
            park();
        } else {
            let flag = w.flag.clone();
            let d = done.clone();
            run_until(vm, || flag.swap(false, Ordering::AcqRel) || d.load(Ordering::Acquire));
        }
    }
    match result.take().expect("I/O job result") {
        Ok(r) => r,
        Err(p) => std::panic::resume_unwind(p),
    }
}
