//! Threads, channels, mutexes, atomics and the parallel collection operations.
//!
//! Every thread that runs Catallaxy code has its own VM registered with the collector. Blocking
//! waits happen in the native state so they never hold up a collection. Values crossing threads
//! move: their owned object graph is re-owned to the transit owner and claimed by the receiver.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use catallaxy_gc::ObjState;
use catallaxy_gc::{RootSource, Tracer};
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops;
use catallaxy_runtime::ownership::{self, TRANSIT_TID};
use catallaxy_runtime::{panic, RtResult, Trap, Value};
use catallaxy_vm::interp::THREAD_STACK;
use catallaxy_vm::{NativeFn, Vm};
use crossbeam_deque::{Injector, Steal};
use parking_lot::{Condvar, Mutex};

use crate::coll::list_read;
use crate::*;

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "__spawn" => spawn,
        "__fiber_spawn" => |vm, a| {
            crate::fiber::spawn(vm, a[0], a[1])?;
            Ok(Value::NIL)
        },
        "__fiber_join" => |vm, a| match crate::fiber::join(vm, a[0])? {
            Ok(v) => Ok(ok(vm, v)),
            Err(msg) => Ok(err(vm, &msg)),
        },
        "__fiber_done" => |_, a| Ok(Value::bool(crate::fiber::is_done(int(a[0])? as u64))),
        "__fiber_yield" => |vm, _| {
            crate::fiber::yield_now(vm);
            Ok(Value::NIL)
        },
        "Thread.join" => join,
        "Thread.is_done" => |_, a| {
            let inner = unsafe { as_thread(a[0]) }.inner.clone();
            let done = !matches!(*inner.outcome.lock(), ThreadOutcome::Running);
            Ok(Value::bool(done))
        },
        "Channel.new" => |vm, a| {
            let cap = arg(a, 0).map(int).transpose()?.unwrap_or(0);
            if cap < 0 {
                return panic("ArgumentError: negative channel capacity");
            }
            Ok(vm.m.alloc_channel(cap as usize))
        },
        "Channel.send" => chan_send,
        "Channel.recv" => |vm, a| chan_recv(vm, a[0], true),
        "Channel.try_recv" => |vm, a| chan_recv(vm, a[0], false),
        "Channel.close" => |_, a| {
            let inner = &unsafe { as_channel(a[0]) }.inner;
            inner.queue.lock().closed = true;
            inner.not_empty.notify_all();
            inner.not_full.notify_all();
            inner.wake_fibers();
            Ok(Value::NIL)
        },
        "Channel.is_closed" => |_, a| Ok(Value::bool(unsafe { as_channel(a[0]) }.inner.queue.lock().closed)),
        "Channel.length" => {
            |_, a| Ok(Value::small_int(unsafe { as_channel(a[0]) }.inner.queue.lock().items.len() as i64))
        }

        "Mutex.new" => |vm, a| {
            ownership::share_deep(&vm.m, a[0], false)?;
            Ok(vm.m.alloc_mutex(a[0]))
        },
        "Mutex.lock" => mutex_lock,
        "Mutex.update" => mutex_update,
        "Mutex.get" => |vm, a| with_mutex(vm, a[0], |_, m| Ok(Value(m.value.load(Ordering::Acquire)))),
        "Mutex.set" => |vm, a| {
            ownership::share_deep(&vm.m, a[1], false)?;
            let v = a[1];
            with_mutex(vm, a[0], move |_, m| {
                m.value.store(v.0, Ordering::Release);
                Ok(Value::NIL)
            })
        },

        "Atomic.new" => |vm, a| {
            ownership::publish_into_shared(&vm.m, a[0])?;
            Ok(vm.m.alloc_atomic(a[0]))
        },
        "Atomic.get" => |_, a| Ok(Value(unsafe { as_atomic(a[0]) }.value.load(Ordering::Acquire))),
        "Atomic.set" => |vm, a| {
            ownership::publish_into_shared(&vm.m, a[1])?;
            unsafe { as_atomic(a[0]) }.value.store(a[1].0, Ordering::Release);
            Ok(Value::NIL)
        },
        "Atomic.swap" => |vm, a| {
            ownership::publish_into_shared(&vm.m, a[1])?;
            Ok(Value(unsafe { as_atomic(a[0]) }.value.swap(a[1].0, Ordering::AcqRel)))
        },
        "Atomic.compare_and_set" => atomic_cas,
        "Atomic.add" => atomic_add,
        "Atomic.update" => atomic_update,

        "List.par_map" => |vm, a| par(vm, a[0], a[1], ParKind::Map),
        "List.par_each" => |vm, a| par(vm, a[0], a[1], ParKind::Each),
        "List.par_select" => |vm, a| par(vm, a[0], a[1], ParKind::Select),
        "List.par_reduce" => par_reduce,
        _ => return None,
    })
}

// ---- threads ---------------------------------------------------------------------------------

fn spawn(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let closure = a[0];
    // Registration takes the collector's thread-list lock, which a collection holds while it
    // waits for running threads to stop: register from the native state.
    let mut child = catallaxy_gc::blocking(&vm.m.rec, || {
        let rec = catallaxy_gc::register_thread();
        Vm::new(rec, THREAD_STACK)
    });
    // The closure (and, for `spawn move`, what it captured) now belongs to the child.
    ownership::transfer_deep(closure, vm.tid(), child.tid());
    child.extra_roots().push(closure);
    let t = vm.m.alloc_thread();
    let inner = unsafe { as_thread(t) }.inner.clone();
    inner.tid.store(child.tid() as u64, Ordering::Relaxed);
    let inner2 = inner.clone();
    let handle = std::thread::Builder::new()
        .name(format!("catx-{}", child.tid()))
        .stack_size(16 << 20)
        .spawn(move || {
            let mut child = child;
            catallaxy_gc::leave_native(&child.m.rec);
            let closure = child.extra_roots()[0];
            let result = child.call_value(closure, &[]);
            // Fibers the thread started finish before the thread does.
            crate::fiber::drain(&mut child);
            let outcome = match result {
                Ok(v) => {
                    ownership::transfer_deep(v, child.tid(), TRANSIT_TID);
                    ThreadOutcome::Done(v)
                }
                Err(Trap::Panic(msg)) => {
                    let trace = child.panic_trace.take().unwrap_or_default();
                    let mut full = msg.clone();
                    for line in trace.iter().take(8) {
                        full.push_str(&format!("\n    at {line}"));
                    }
                    flush();
                    eprintln!("warning: thread {} panicked: {}", child.tid(), msg);
                    ThreadOutcome::Panicked(full)
                }
                Err(Trap::Exit(code)) => {
                    flush();
                    std::process::exit(code);
                }
                Err(Trap::Unwind { .. }) => ThreadOutcome::Panicked("internal error: stray non-local exit".into()),
            };
            {
                let mut o = inner2.outcome.lock();
                *o = outcome;
            }
            inner2.done.notify_all();
            child.extra_roots().clear();
            child.shutdown();
        })
        .map_err(|e| Trap::Panic(format!("cannot spawn thread: {e}")))?;
    *inner.handle.lock() = Some(handle);
    Ok(t)
}

fn join(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let inner = unsafe { as_thread(a[0]) }.inner.clone();
    let handle = crate::fiber::blocking_io(vm, || {
        let mut o = inner.outcome.lock();
        while matches!(*o, ThreadOutcome::Running) {
            inner.done.wait(&mut o);
        }
        drop(o);
        inner.handle.lock().take()
    });
    if let Some(h) = handle {
        blocking(vm, || {
            let _ = h.join();
        });
    }
    let outcome = std::mem::replace(&mut *inner.outcome.lock(), ThreadOutcome::Joined);
    match outcome {
        ThreadOutcome::Done(v) => {
            ownership::transfer_deep(v, TRANSIT_TID, vm.tid());
            Ok(ok(vm, v))
        }
        ThreadOutcome::Panicked(msg) => Ok(err(vm, &msg)),
        ThreadOutcome::Joined => Ok(err(vm, "thread was already joined")),
        ThreadOutcome::Running => unreachable!(),
    }
}

// ---- channels --------------------------------------------------------------------------------

fn chan_send(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let (ch, item) = (a[0], a[1]);
    let inner = &unsafe { as_channel(ch) }.inner;
    // Owned values travel without an owner until received.
    let owned = item.header().is_some_and(|h| h.state() == ObjState::Owned);
    if owned {
        if let Some(h) = item.header() {
            if h.owner() != vm.tid() {
                return panic("OwnershipError: cannot send an object owned by another thread");
            }
        }
    }
    loop {
        {
            let mut q = inner.queue.lock();
            if q.closed {
                return panic("ChannelError: send on a closed channel");
            }
            if inner.capacity == 0 || q.items.len() < inner.capacity {
                if owned {
                    ownership::transfer_deep(item, vm.tid(), TRANSIT_TID);
                }
                q.items.push_back(item);
                drop(q);
                inner.not_empty.notify_one();
                inner.wake_fibers();
                return Ok(Value::NIL);
            }
        }
        let cooperative = crate::fiber::wait_event(vm, |wake| {
            let q = inner.queue.lock();
            if q.closed || q.items.len() < inner.capacity {
                return false;
            }
            inner.waiters.lock().push(wake);
            true
        });
        if cooperative {
            continue;
        }
        blocking(vm, || {
            let mut q = inner.queue.lock();
            while !q.closed && q.items.len() >= inner.capacity {
                inner.not_full.wait(&mut q);
            }
        });
    }
}

fn chan_recv(vm: &mut Vm, ch: Value, wait: bool) -> RtResult<Value> {
    let inner = &unsafe { as_channel(ch) }.inner;
    loop {
        {
            let mut q = inner.queue.lock();
            if let Some(v) = q.items.pop_front() {
                drop(q);
                inner.not_full.notify_one();
                inner.wake_fibers();
                ownership::transfer_deep(v, TRANSIT_TID, vm.tid());
                return Ok(v);
            }
            if q.closed || !wait {
                return Ok(Value::NIL);
            }
        }
        let cooperative = crate::fiber::wait_event(vm, |wake| {
            let q = inner.queue.lock();
            if q.closed || !q.items.is_empty() {
                return false;
            }
            inner.waiters.lock().push(wake);
            true
        });
        if cooperative {
            continue;
        }
        blocking(vm, || {
            let mut q = inner.queue.lock();
            while !q.closed && q.items.is_empty() {
                inner.not_empty.wait(&mut q);
            }
        });
    }
}

// ---- mutexes ---------------------------------------------------------------------------------

fn with_mutex(vm: &mut Vm, m: Value, f: impl FnOnce(&mut Vm, &MutexObj) -> RtResult<Value>) -> RtResult<Value> {
    let mo: &MutexObj = unsafe { as_mutex(m) };
    let guard = match mo.raw.try_lock() {
        Some(g) => g,
        None => {
            let rec = vm.m.rec.clone();
            catallaxy_gc::enter_native(&rec);
            let g = mo.raw.lock();
            catallaxy_gc::leave_native(&rec);
            g
        }
    };
    // The lock is keyed by OS thread: a parked fiber holding it would let its siblings in.
    vm.no_park += 1;
    let r = f(vm, mo);
    vm.no_park -= 1;
    drop(guard);
    r
}

fn mutex_lock(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let f = a[1];
    with_mutex(vm, a[0], |vm, mo| {
        let v = Value(mo.value.load(Ordering::Acquire));
        let r = vm.call_value(f, &[v])?;
        ownership::publish_into_shared(&vm.m, r)?;
        Ok(r)
    })
}

fn mutex_update(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let f = a[1];
    with_mutex(vm, a[0], |vm, mo| {
        let v = Value(mo.value.load(Ordering::Acquire));
        let r = vm.call_value(f, &[v])?;
        ownership::share_deep(&vm.m, r, false)?;
        mo.value.store(r.0, Ordering::Release);
        Ok(r)
    })
}

// ---- atomics ---------------------------------------------------------------------------------

fn atomic_cas(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let at = &unsafe { as_atomic(a[0]) }.value;
    ownership::publish_into_shared(&vm.m, a[2])?;
    loop {
        let cur = Value(at.load(Ordering::Acquire));
        if !(cur.0 == a[1].0 || ops::values_eq(vm, cur, a[1])?) {
            return Ok(Value::FALSE);
        }
        if at.compare_exchange(cur.0, a[2].0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return Ok(Value::TRUE);
        }
    }
}

fn atomic_add(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let at = &unsafe { as_atomic(a[0]) }.value;
    let delta = a[1];
    if !ops::is_int(delta) {
        return panic("TypeError: Atomic#add needs an Int");
    }
    loop {
        let cur = Value(at.load(Ordering::Acquire));
        if !ops::is_int(cur) {
            return panic("TypeError: Atomic#add on a non-Int value");
        }
        let new = ops::int_add(&mut vm.m, cur, delta);
        if at.compare_exchange(cur.0, new.0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return Ok(new);
        }
    }
}

fn atomic_update(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let (atom, f) = (a[0], a[1]);
    loop {
        let at = &unsafe { as_atomic(atom) }.value;
        let cur = Value(at.load(Ordering::Acquire));
        // Keep `cur` alive while user code runs (another thread may replace it).
        vm.extra_roots().push(cur);
        let r = vm.call_value(f, &[cur]);
        vm.extra_roots().pop();
        let new = r?;
        ownership::publish_into_shared(&vm.m, new)?;
        let at = &unsafe { as_atomic(atom) }.value;
        if at.compare_exchange(cur.0, new.0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return Ok(new);
        }
    }
}

// ---- parallel collections --------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq)]
enum ParKind {
    Map,
    Each,
    Select,
    Reduce,
}

struct Job {
    kind: ParKind,
    closure: Value,
    items: Vec<Value>,
    /// One slot per item (map/select) or per chunk (reduce).
    results: Vec<AtomicU64>,
    identity: Value,
    remaining: AtomicUsize,
    error: Mutex<Option<String>>,
    done: (Mutex<bool>, Condvar),
}

// SAFETY: the values in a job are frozen or shared (checked statically), or transferred.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

struct Task {
    job: Arc<Job>,
    start: usize,
    end: usize,
    chunk: usize,
}

struct Pool {
    injector: Injector<Task>,
    signal: (Mutex<u64>, Condvar),
    jobs: Mutex<Vec<Arc<Job>>>,
    workers: usize,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| {
    let n = std::env::var("CATX_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .max(1);
    catallaxy_gc::add_global_roots(Box::new(JobRoots));
    let pool = Pool {
        injector: Injector::new(),
        signal: (Mutex::new(0), Condvar::new()),
        jobs: Mutex::new(Vec::new()),
        workers: n.saturating_sub(1),
    };
    for i in 0..pool.workers {
        std::thread::Builder::new()
            .name(format!("catx-pool-{i}"))
            .stack_size(16 << 20)
            .spawn(worker_main)
            .expect("spawn pool worker");
    }
    pool
});

struct JobRoots;

impl RootSource for JobRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        // The jobs mutex is never held across a safepoint.
        let jobs = unsafe { &*POOL.jobs.data_ptr() };
        for j in jobs {
            mark(t, j.closure);
            mark(t, j.identity);
            for &v in &j.items {
                mark(t, v);
            }
            for r in &j.results {
                mark(t, Value(r.load(Ordering::Relaxed)));
            }
        }
    }
}

fn mark(t: &mut Tracer, v: Value) {
    if v.is_ptr() {
        t.mark(v.as_ptr());
    }
}

fn worker_main() {
    let rec = catallaxy_gc::register_thread();
    let mut vm = Vm::new(rec, THREAD_STACK);
    // Workers idle in the native state.
    loop {
        let task = loop {
            match POOL.injector.steal() {
                Steal::Success(t) => break t,
                Steal::Retry => continue,
                Steal::Empty => {
                    let mut g = POOL.signal.0.lock();
                    if POOL.injector.is_empty() {
                        POOL.signal.1.wait_for(&mut g, std::time::Duration::from_millis(50));
                    }
                }
            }
        };
        catallaxy_gc::leave_native(&vm.m.rec);
        run_task(&mut vm, task);
        crate::fiber::drain(&mut vm);
        catallaxy_gc::enter_native(&vm.m.rec);
    }
}

fn run_task(vm: &mut Vm, task: Task) {
    let job = &task.job;
    let failed = job.error.lock().is_some();
    if !failed {
        let r = run_range(vm, job, task.start, task.end, task.chunk);
        if let Err(e) = r {
            let msg = match e {
                Trap::Panic(m) => m,
                Trap::Exit(c) => format!("exit({c}) inside a parallel block"),
                Trap::Unwind { .. } => "break or return out of a parallel block".into(),
            };
            let mut g = job.error.lock();
            if g.is_none() {
                *g = Some(msg);
            }
        }
    }
    let n = task.end - task.start;
    if job.remaining.fetch_sub(n, Ordering::AcqRel) == n {
        let (m, cv) = &job.done;
        *m.lock() = true;
        cv.notify_all();
    }
}

fn run_range(vm: &mut Vm, job: &Job, start: usize, end: usize, chunk: usize) -> RtResult<()> {
    let tid = vm.tid();
    match job.kind {
        ParKind::Reduce => {
            let mut acc = job.identity;
            vm.extra_roots().push(acc);
            let base = vm.extra_roots().len() - 1;
            for i in start..end {
                let r = vm.call_value(job.closure, &[acc, job.items[i]]);
                match r {
                    Ok(v) => {
                        acc = v;
                        vm.extra_roots()[base] = acc;
                    }
                    Err(e) => {
                        vm.extra_roots().truncate(base);
                        return Err(e);
                    }
                }
            }
            vm.extra_roots().truncate(base);
            ownership::transfer_deep(acc, tid, TRANSIT_TID);
            job.results[chunk].store(acc.0, Ordering::Release);
        }
        _ => {
            for i in start..end {
                let v = vm.call_value(job.closure, &[job.items[i]])?;
                match job.kind {
                    ParKind::Map => {
                        ownership::transfer_deep(v, tid, TRANSIT_TID);
                        job.results[i].store(v.0, Ordering::Release);
                    }
                    ParKind::Select => job.results[i].store(v.0, Ordering::Release),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn par(vm: &mut Vm, list: Value, f: Value, kind: ParKind) -> RtResult<Value> {
    let items = list_read(vm, list)?.to_vec();
    run_job(vm, kind, f, items, Value::NIL)
}

fn par_reduce(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let items = list_read(vm, a[0])?.to_vec();
    run_job(vm, ParKind::Reduce, a[2], items, a[1])
}

fn run_job(vm: &mut Vm, kind: ParKind, f: Value, items: Vec<Value>, identity: Value) -> RtResult<Value> {
    // Workers read the block, the items and the identity: they must be old.
    let tid = vm.tid();
    ownership::escape(f, tid);
    ownership::escape(identity, tid);
    for &x in &items {
        ownership::escape(x, tid);
    }
    let n = items.len();
    let pool = &*POOL;
    let threads = pool.workers + 1;
    let chunk_size = (n / (threads * 8)).max(1);
    let nchunks = n.div_ceil(chunk_size);
    let nresults = match kind {
        ParKind::Map | ParKind::Select => n,
        ParKind::Reduce => nchunks,
        ParKind::Each => 0,
    };
    let job = Arc::new(Job {
        kind,
        closure: f,
        items,
        results: (0..nresults).map(|_| AtomicU64::new(Value::NIL.0)).collect(),
        identity,
        remaining: AtomicUsize::new(n),
        error: Mutex::new(None),
        done: (Mutex::new(n == 0), Condvar::new()),
    });
    pool.jobs.lock().push(job.clone());
    for c in 0..nchunks {
        let start = c * chunk_size;
        let end = ((c + 1) * chunk_size).min(n);
        pool.injector.push(Task { job: job.clone(), start, end, chunk: c });
    }
    {
        let _g = pool.signal.0.lock();
        pool.signal.1.notify_all();
    }
    // A fiber runs on a small native stack, and helping can nest without bound: a stolen task
    // may belong to another job and start a parallel job of its own, which helps in turn. So a
    // fiber leaves the work to the pool and only waits (unless there is no pool to wait for).
    if crate::fiber::in_fiber() && pool.workers > 0 {
        let job2 = job.clone();
        blocking(vm, move || {
            let (m, cv) = &job2.done;
            let mut d = m.lock();
            while !*d {
                cv.wait(&mut d);
            }
        });
    }
    // Help until our job is finished (running any queued task keeps nested jobs progressing).
    loop {
        if job.remaining.load(Ordering::Acquire) == 0 {
            break;
        }
        match pool.injector.steal() {
            Steal::Success(t) => run_task(vm, t),
            Steal::Retry => {}
            Steal::Empty => {
                let job2 = job.clone();
                blocking(vm, move || {
                    let (m, cv) = &job2.done;
                    let mut d = m.lock();
                    if !*d {
                        cv.wait_for(&mut d, std::time::Duration::from_millis(1));
                    }
                });
            }
        }
    }
    pool.jobs.lock().retain(|j| !Arc::ptr_eq(j, &job));
    if let Some(msg) = job.error.lock().take() {
        return Err(Trap::Panic(msg));
    }
    let tid = vm.tid();
    let out = match kind {
        ParKind::Each => Value::NIL,
        ParKind::Map => {
            let vals: Vec<Value> = job.results.iter().map(|r| Value(r.load(Ordering::Acquire))).collect();
            for &v in &vals {
                ownership::transfer_deep(v, TRANSIT_TID, tid);
            }
            new_list(vm, vals)
        }
        ParKind::Select => {
            let vals: Vec<Value> = job
                .items
                .iter()
                .zip(job.results.iter())
                .filter(|(_, r)| Value(r.load(Ordering::Acquire)).is_true())
                .map(|(&v, _)| v)
                .collect();
            new_list(vm, vals)
        }
        ParKind::Reduce => {
            let parts: Vec<Value> = job.results.iter().map(|r| Value(r.load(Ordering::Acquire))).collect();
            for &v in &parts {
                ownership::transfer_deep(v, TRANSIT_TID, tid);
            }
            let base = vm.extra_roots().len();
            vm.extra_roots().extend_from_slice(&parts);
            let mut acc = identity;
            let mut res = Ok(());
            for &p in &parts {
                match vm.call_value(f, &[acc, p]) {
                    Ok(v) => acc = v,
                    Err(e) => {
                        res = Err(e);
                        break;
                    }
                }
                vm.extra_roots().push(acc);
            }
            vm.extra_roots().truncate(base);
            res?;
            acc
        }
    };
    Ok(out)
}
