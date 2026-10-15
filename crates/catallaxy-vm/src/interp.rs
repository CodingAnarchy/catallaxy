//! The register-based interpreter.
//!
//! One [`Vm`] runs per OS thread (and one per fiber). Frames live in a register stack that starts
//! small and grows on demand up to the VM's limit; see [`Vm::grow_stack`] for how growing keeps
//! the raw pointers into it sound (natives receive their arguments as a slice of the caller's
//! registers). Calls between bytecode functions do not recurse on the Rust stack; only natives
//! calling back into bytecode do.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use catallaxy_gc::{Header, ObjState, RootSource, ThreadRecord, Tracer};
use catallaxy_runtime::class::{class_of, heap_class, ids, ObjKind, BUILTINS};
use catallaxy_runtime::fmtspec::{self, FormatSpec};
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops::{self, Interop};
use catallaxy_runtime::ownership::{self, check_read, check_write, Access};
use catallaxy_runtime::{panic, Mutator, RtResult, Sym, Trap, Value};

use crate::bytecode::*;
use crate::program::{self, Func, NO_FUNC};
use crate::slow;

pub const UNWIND_BREAK: u8 = 0;
pub const UNWIND_RETURN: u8 = 1;

const MAX_FRAMES: usize = 200_000;
/// How deeply `with lock` may nest on one object. The object's lock counts up to
/// [`catallaxy_gc::Header::MAX_LOCK_NESTING`] acquisitions; the rest is left for the runtime's
/// own short-lived locking of the same object inside the block.
pub const MAX_WITH_LOCK_NESTING: u32 = 1000;
const _: () = assert!(MAX_WITH_LOCK_NESTING + 16 <= catallaxy_gc::Header::MAX_LOCK_NESTING);

#[derive(Clone, Copy)]
pub struct Frame {
    pub func: &'static Func,
    pub pc: usize,
    pub base: usize,
    /// Absolute stack slot receiving the result.
    pub ret: usize,
    pub closure: Value,
    /// Identity for non-local exits (0 until requested).
    pub serial: u64,
    /// One past the highest live register of this frame and every frame below it.
    pub top: usize,
}

pub struct Vm {
    pub m: Mutator,
    /// The register stack: `stack_cap` values, growing up to `stack_max` (see [`Vm::grow_stack`]).
    stack: *mut Value,
    stack_cap: usize,
    stack_max: usize,
    /// Register stacks replaced by a larger one, kept until no native can still be reading its
    /// arguments from them (see [`Vm::grow_stack`]).
    retired: Vec<(*mut Value, usize)>,
    /// `CATX_GC_STRESS` is on (see [`Vm::stress_move`]).
    stress: bool,
    /// Frames, locks and extra roots: read by the collector while this thread is stopped, so
    /// they live in a separate allocation reached only through this raw pointer.
    rs: *mut RootState,
    next_serial: u64,
    /// Stack trace captured where the current panic started.
    pub panic_trace: Option<Vec<String>>,
    /// The root state of the thread's main VM, which also lists the thread's fiber VMs for the
    /// collector. Equal to `rs` for a main VM.
    root_rs: *mut RootState,
    /// Nonzero while native code holds a lock keyed by the OS thread (`Mutex` closures): a fiber
    /// must not park then, or a sibling fiber could enter the same lock.
    pub no_park: u32,
}

/// VM state the collector scans.
pub(crate) struct RootState {
    /// The register stack (a copy of [`Vm::stack`], which moves when it grows).
    stack: *mut Value,
    pub(crate) frames: Vec<Frame>,
    /// Objects locked by `with lock`, with the frame depth that locked them.
    held_locks: Vec<(usize, Value)>,
    /// Extra values rooted by the runtime (thread-pool jobs and the like).
    extra_roots: Vec<Value>,
    /// Roots of the thread's fiber VMs (main VM only). A fiber's root state records its index
    /// here, so it is removed in O(1).
    children: Vec<VmRoots>,
    /// This fiber VM's index in its thread's `children`.
    child_index: usize,
    /// Values held for the thread's fibers, by fiber id (results awaiting `join`).
    pinned: std::collections::HashMap<u64, Value>,
}

impl RootState {
    fn new(stack: *mut Value, frames: usize) -> Box<RootState> {
        Box::new(RootState {
            stack,
            frames: Vec::with_capacity(frames),
            held_locks: Vec::new(),
            extra_roots: Vec::new(),
            children: Vec::new(),
            child_index: usize::MAX,
            pinned: std::collections::HashMap::new(),
        })
    }
}

// SAFETY: a Vm is used by one thread at a time; it is created by a parent thread and handed to
// the child before the child starts running code.
unsafe impl Send for Vm {}

#[derive(Clone, Copy)]
struct VmRoots {
    rs: *mut RootState,
    handles: *const Vec<Value>,
    /// For a fiber VM: whether minor collections must scan it (`SCAN_*`).
    scan: u8,
}

/// A fiber VM that is parked, and whose roots referenced no young object when a minor
/// collection last scanned them. Only the fiber writes its roots, so they still reference only
/// old objects, whose young referents the remembered set finds: minor collections skip it.
const SCAN_CLEAN: u8 = 0;
/// A fiber VM that ran (or was created) since a minor collection last found it clean.
const SCAN_DIRTY: u8 = 1;
/// The running fiber VM: its roots change under the collection's feet, so it is never clean.
const SCAN_RUNNING: u8 = 2;

impl VmRoots {
    /// Marks this VM's own roots (not its children's). With `check`, returns whether any of them
    /// references a young object.
    fn trace_own(&self, t: &mut Tracer, check: bool) -> bool {
        let mut young = false;
        let mut mark = |t: &mut Tracer, v: Value| {
            if v.is_ptr() {
                let p = v.as_ptr();
                // SAFETY: roots reference live objects.
                young |= check && unsafe { (*p).is_young() };
                t.mark(p);
            }
        };
        // SAFETY: called only while the owning thread is stopped (or is the one collecting);
        // both pointers are owned allocations that the thread itself also reaches only through
        // raw pointers.
        let rs = unsafe { &*self.rs };
        let top = rs.frames.last().map(|f| f.top).unwrap_or(0);
        for i in 0..top {
            mark(t, unsafe { *rs.stack.add(i) });
        }
        for f in &rs.frames {
            mark(t, f.closure);
        }
        for &v in unsafe { &*self.handles } {
            mark(t, v);
        }
        for &v in &rs.extra_roots {
            mark(t, v);
        }
        for &(_, v) in &rs.held_locks {
            mark(t, v);
        }
        for &v in rs.pinned.values() {
            mark(t, v);
        }
        young
    }
}

impl RootSource for VmRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        self.trace_own(t, false);
        let minor = t.is_minor();
        // SAFETY: as in `trace_own`. A minor collection runs on the owning thread itself, which
        // is the only one to read or write the children's scan states.
        let children = unsafe { &mut (*self.rs).children };
        for child in children.iter_mut() {
            if !minor {
                child.trace_own(t, false);
                continue;
            }
            match child.scan {
                SCAN_CLEAN => {}
                SCAN_DIRTY => {
                    // The young objects marked here are tenured by this collection, except
                    // large ones only roots reference, which stay young (see
                    // `LocalHeap::sweep_young`): then the fiber stays dirty and is scanned again.
                    if !child.trace_own(t, true) {
                        child.scan = SCAN_CLEAN;
                    }
                }
                _ => {
                    child.trace_own(t, false);
                }
            }
        }
    }
}

/// A register stack of `n` values, or null if the allocator refuses.
fn alloc_stack(n: usize) -> *mut Value {
    let layout = std::alloc::Layout::array::<Value>(n).unwrap();
    // Zeroed memory is lazily committed by the OS; zero bits decode as the float 0.0, which the
    // collector ignores.
    unsafe { std::alloc::alloc_zeroed(layout) as *mut Value }
}

/// The `MemoryError` for a register stack of `n` values the allocator refused.
#[cold]
fn stack_refused<T>(n: usize) -> RtResult<T> {
    catallaxy_runtime::refused(format!(
        "cannot allocate a {} KB register stack",
        n * std::mem::size_of::<Value>() / 1024
    ))
}

/// Register stack limits, in values. A VM's stack starts at [`INITIAL_STACK`] values (or its
/// limit, if smaller) and grows on demand up to its limit.
pub const MAIN_STACK: usize = 4 << 20;
pub const THREAD_STACK: usize = 1 << 20;
pub const INITIAL_STACK: usize = 1024;
/// The initial register stack under `CATX_GC_STRESS`, which also moves small stacks on every
/// call back into the interpreter from a native (see [`Vm::grow_stack`]).
const STRESS_INITIAL_STACK: usize = 16;
/// Under stress, stacks up to this many values move on every call back from a native.
const STRESS_MOVE_LIMIT: usize = 64 * 1024;
/// Under stress, stacks stop moving while the ones kept for natives hold this many values.
const STRESS_RETIRED_LIMIT: usize = 2 << 20;

/// Whether `CATX_GC_STRESS` is on: register stacks then start tiny and move often, so that a
/// stale pointer into one shows up in the conformance suite.
fn stack_stress() -> bool {
    static STRESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRESS.get_or_init(|| std::env::var("CATX_GC_STRESS").is_ok_and(|v| v != "0"))
}

/// The register stack a VM with limit `max` starts with.
fn initial_stack(max: usize, initial: usize) -> usize {
    let initial = if stack_stress() { STRESS_INITIAL_STACK } else { initial };
    initial.clamp(1, max.max(1))
}

/// Frees a register stack of `n` values made by [`alloc_stack`].
///
/// # Safety
/// `p` came from `alloc_stack(n)` and nothing uses it any more.
unsafe fn free_stack(p: *mut Value, n: usize) {
    std::alloc::dealloc(p as *mut u8, std::alloc::Layout::array::<Value>(n).unwrap());
}

impl Vm {
    /// Creates a VM for a thread registered with the collector. The thread record must be in the
    /// native state; the VM's roots are installed before this returns.
    /// Aborts if the register stack cannot be allocated; see [`Vm::try_new`].
    pub fn new(rec: Arc<ThreadRecord>, stack_size: usize) -> Box<Vm> {
        Vm::try_new(rec, stack_size)
            .unwrap_or_else(|_| std::alloc::handle_alloc_error(std::alloc::Layout::array::<Value>(stack_size).unwrap()))
    }

    /// As [`Vm::new`], but a register stack the allocator refuses is a `MemoryError` panic.
    /// The thread record is left registered either way.
    pub fn try_new(rec: Arc<ThreadRecord>, stack_size: usize) -> RtResult<Box<Vm>> {
        let cap = initial_stack(stack_size, INITIAL_STACK);
        let stack = alloc_stack(cap);
        if stack.is_null() {
            return stack_refused(cap);
        }
        let m = Mutator::new(rec);
        let rs = Box::into_raw(RootState::new(stack, 256));
        let vm = Box::new(Vm {
            m,
            stack,
            stack_cap: cap,
            stack_max: stack_size,
            retired: Vec::new(),
            stress: stack_stress(),
            rs,
            next_serial: 1,
            panic_trace: None,
            root_rs: rs,
            no_park: 0,
        });
        let roots: Box<dyn RootSource> =
            Box::new(VmRoots { rs: vm.rs, handles: vm.m.handles_ptr(), scan: SCAN_RUNNING });
        let roots_ptr = Box::into_raw(roots);
        let heap_ptr = vm.m.heap_ptr();
        unsafe { catallaxy_gc::install(&vm.m.rec, roots_ptr, heap_ptr) };
        // The root source box is leaked deliberately; it is tiny and lives as long as the thread.
        Ok(vm)
    }

    /// Creates a VM for a fiber of this VM's thread: it shares the thread's heap and collector
    /// record, and its roots are scanned as part of the thread's. Its register stack starts at
    /// `initial` values and grows up to `max`. Release it with [`Vm::release_fiber`] on the same
    /// thread. A register stack the allocator refuses is a `MemoryError` panic.
    pub fn new_fiber(&mut self, initial: usize, max: usize) -> RtResult<Box<Vm>> {
        let cap = initial_stack(max, initial);
        let stack = alloc_stack(cap);
        if stack.is_null() {
            return stack_refused(cap);
        }
        let rs = Box::into_raw(RootState::new(stack, 16));
        let vm = Box::new(Vm {
            m: Mutator::sharing(&self.m),
            stack,
            stack_cap: cap,
            stack_max: max,
            retired: Vec::new(),
            stress: stack_stress(),
            rs,
            next_serial: 1,
            panic_trace: None,
            root_rs: self.root_rs,
            no_park: 0,
        });
        let roots = VmRoots { rs: vm.rs, handles: vm.m.handles_ptr(), scan: SCAN_DIRTY };
        // SAFETY: the root states belong to this thread, which is running (not being scanned).
        unsafe {
            let children = &mut (*self.root_rs).children;
            (*vm.rs).child_index = children.len();
            children.push(roots);
        }
        Ok(vm)
    }

    /// Unregisters a fiber VM from its thread's roots and frees it, in O(1). It takes the box
    /// `new_fiber` returned: the VM's address was handed to the fiber's coroutine.
    #[allow(clippy::boxed_local)]
    pub fn release_fiber(mut vm: Box<Vm>) {
        // SAFETY: the root states belong to this thread, which is running (not being scanned).
        unsafe {
            let children = &mut (*vm.root_rs).children;
            let i = (*vm.rs).child_index;
            debug_assert!(std::ptr::eq(children[i].rs, vm.rs));
            children.swap_remove(i);
            if let Some(moved) = children.get(i) {
                (*moved.rs).child_index = i;
            }
            vm.free_stacks();
            drop(Box::from_raw(vm.rs));
            vm.m.free_handles();
        }
    }

    /// Frees the register stack and the retired ones. The VM must not run code afterwards.
    fn free_stacks(&mut self) {
        self.free_retired();
        if !self.stack.is_null() {
            // SAFETY: allocated by `alloc_stack` with this capacity; no collection scans it now.
            unsafe { free_stack(self.stack, self.stack_cap) };
            self.stack = std::ptr::null_mut();
        }
    }

    fn free_retired(&mut self) {
        for (p, n) in self.retired.drain(..) {
            // SAFETY: a retired stack is no longer the VM's, and the caller made sure no native
            // still reads its arguments from it.
            unsafe { free_stack(p, n) };
        }
    }

    /// Tells minor collections that this fiber VM starts (`true`) or stops running. One that has
    /// stopped is scanned by the next minor collection, and skipped by later ones until it runs
    /// again if its roots then referenced no young object (issue #174). Fiber VMs only.
    pub fn set_fiber_running(&mut self, running: bool) {
        // SAFETY: the root states belong to this thread, which is running (not being scanned).
        unsafe {
            let i = (*self.rs).child_index;
            let child = &mut (&mut (*self.root_rs).children)[i];
            debug_assert!(std::ptr::eq(child.rs, self.rs));
            child.scan = if running { SCAN_RUNNING } else { SCAN_DIRTY };
        }
    }

    /// Keeps `v` alive under `key` in the thread's main VM until [`Vm::unpin`].
    pub fn pin(&mut self, key: u64, v: Value) {
        unsafe { (*self.root_rs).pinned.insert(key, v) };
    }

    /// Removes and returns the value pinned under `key`.
    pub fn unpin(&mut self, key: u64) -> Option<Value> {
        unsafe { (*self.root_rs).pinned.remove(&key) }
    }

    /// Whether a fiber running on this VM may park: not while it holds `with lock` object
    /// locks or a `Mutex`, which are keyed by OS thread and so would let sibling fibers in.
    pub fn park_allowed(&self) -> bool {
        self.no_park == 0 && self.rs().held_locks.is_empty()
    }

    /// Registers the current thread and creates its VM, leaving it in the running state.
    pub fn for_current_thread(stack_size: usize) -> Box<Vm> {
        catallaxy_runtime::stack::init_thread();
        let rec = catallaxy_gc::register_thread();
        let vm = Vm::new(rec, stack_size);
        catallaxy_gc::leave_native(&vm.m.rec);
        vm
    }

    /// Unregisters the thread and frees its register stack. The VM must not be used afterwards.
    pub fn shutdown(&mut self) {
        catallaxy_gc::enter_native(&self.m.rec);
        // Unregistering hands the heap's pages back, with no collection running (#218's
        // crash: a collection must never run while live objects sit in pages it does not
        // know); releasing them again afterwards finds nothing left.
        catallaxy_gc::unregister_thread(&self.m.rec);
        self.m.release_pages();
        self.free_stacks();
    }

    #[inline(always)]
    pub fn tid(&self) -> u16 {
        self.m.tid
    }

    /// First stack slot above every live register of every frame.
    fn stack_top(&self) -> usize {
        self.rs().frames.last().map(|f| f.top).unwrap_or(0)
    }

    #[inline(always)]
    fn set_reg(&mut self, i: usize, v: Value) {
        unsafe { *self.stack.add(i) = v }
    }

    /// Sets stack slots `lo..hi` to nil, so values left by a popped frame are not roots.
    fn clear_regs(&mut self, lo: usize, hi: usize) {
        for i in lo..hi.min(self.stack_cap) {
            self.set_reg(i, Value::NIL);
        }
    }

    /// A handle scope for native code: values rooted in it survive collections triggered by
    /// calls back into the VM, and are released when the scope is dropped.
    pub fn handle_scope(&mut self) -> catallaxy_runtime::HandleScope<'static> {
        // SAFETY: the mutator lives as long as the VM and belongs to this thread; scopes are
        // created and dropped within one native call.
        unsafe { catallaxy_runtime::HandleScope::detached(&mut self.m) }
    }

    pub fn extra_roots(&mut self) -> &mut Vec<Value> {
        &mut self.rs().extra_roots
    }

    /// The collector-visible state. Never hold the reference across a safepoint.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn rs(&self) -> &mut RootState {
        // SAFETY: only this thread mutates its root state, and never while stopped.
        unsafe { &mut *self.rs }
    }

    // ---- calling ---------------------------------------------------------------------------

    /// Calls function `fid` with `args` (padded with missing-argument markers for defaults).
    pub fn call_func(&mut self, fid: u32, args: &[Value]) -> RtResult<Value> {
        let f = program::func(fid);
        self.invoke(f, Value::NIL, args)
    }

    /// Calls a closure value.
    pub fn call_value(&mut self, closure: Value, args: &[Value]) -> RtResult<Value> {
        if !closure.is_ptr() || unsafe { heap_class(closure.as_ptr()) }.kind != ObjKind::Closure {
            return panic(format!("cannot call a value of type {}", class_of(closure).name));
        }
        let fid = unsafe { as_closure(closure) }.func;
        self.invoke(program::func(fid), closure, args)
    }

    /// Calls a method by name on `recv` (dynamic dispatch). Errors if it does not exist.
    pub fn call_method_named(&mut self, recv: Value, name: Sym, args: &[Value]) -> RtResult<Value> {
        let cls = class_of(recv);
        let Some(fid) = cls.lookup(name) else {
            return panic(format!("NoMethodError: undefined method `{}` for {}", name, cls.name));
        };
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(recv);
        all.extend_from_slice(args);
        self.call_func(fid, &all)
    }

    fn invoke(&mut self, f: &'static Func, closure: Value, args: &[Value]) -> RtResult<Value> {
        // Natives re-enter the interpreter through here, so this is where a native recursion
        // (through `Mutex#lock`, `par_map`, a user `==`) would otherwise overflow the native stack.
        catallaxy_runtime::stack::check()?;
        if let Some(native) = f.native {
            return native(self, args);
        }
        self.stress_move()?;
        let mut base = self.stack_top();
        if !closure.is_nil() {
            // Keep the closure rooted below the callee's frame.
            self.check_stack(base + 1)?;
            self.set_reg(base, closure);
            base += 1;
        }
        self.check_stack(base + f.nregs)?;
        if args.len() > f.nparams {
            return panic(format!("{} expects {} arguments, got {}", f.name, f.nparams, args.len()));
        }
        for (i, &a) in args.iter().enumerate() {
            self.set_reg(base + i, a);
        }
        for i in args.len()..f.nparams {
            self.set_reg(base + i, Value::EMPTY);
        }
        for i in f.nparams..f.nregs {
            self.set_reg(base + i, Value::NIL);
        }
        let stop = self.rs().frames.len();
        let top = (base + f.nregs).max(self.stack_top());
        self.rs().frames.push(Frame { func: f, pc: 0, base, ret: base, closure, serial: 0, top });
        catallaxy_gc::safepoint(&self.m.rec);
        self.execute(stop)
    }

    /// Makes room for `need` registers, for a call from native code.
    #[inline(always)]
    fn check_stack(&mut self, need: usize) -> RtResult<()> {
        if need >= self.stack_cap || self.rs().frames.len() >= MAX_FRAMES {
            // With no frames, no interpreter loop runs on this VM, so no native called from
            // bytecode does either.
            let idle = self.rs().frames.is_empty();
            return self.grow_stack(need, idle);
        }
        Ok(())
    }

    /// Makes the register stack hold more than `need` values, or fails with a stack overflow
    /// past the VM's limit (or `MAX_FRAMES` frames). The stack at least doubles, so a deep
    /// recursion copies it O(log n) times.
    ///
    /// Growing moves the stack, and code may hold raw pointers into the old one: each running
    /// interpreter loop's register window, and the argument slices of natives that called back
    /// into the interpreter. Interpreter loops reload their window after anything that may call
    /// back (see `vm_call!` in `run`). Natives only read their argument slices, so the old
    /// stack is kept, values and all, unless `safe` says no native called from bytecode can be
    /// running on this VM (the outermost interpreter loop is asking, or none runs). Kept stacks
    /// are freed by the next safe growth, or with the VM; growth at least doubles the stack, so
    /// together they are smaller than it.
    ///
    /// Under `CATX_GC_STRESS` a small stack also moves (to a copy of the same size) on calls
    /// back into the interpreter, so that a missed reload shows up in the test suites.
    #[cold]
    #[inline(never)]
    fn grow_stack(&mut self, need: usize, safe: bool) -> RtResult<()> {
        if self.rs().frames.len() >= MAX_FRAMES || need >= self.stack_max {
            return Err(Trap::Panic(catallaxy_runtime::stack::OVERFLOW.into()));
        }
        let cap = if need < self.stack_cap {
            self.stack_cap
        } else {
            (need + 1).max(self.stack_cap.saturating_mul(2)).min(self.stack_max)
        };
        let stack = alloc_stack(cap);
        if stack.is_null() {
            return stack_refused(cap);
        }
        // SAFETY: both are valid for `stack_cap` values, and the new one is at least as large.
        unsafe { std::ptr::copy_nonoverlapping(self.stack, stack, self.stack_cap) };
        let old = std::mem::replace(&mut self.stack, stack);
        let old_cap = std::mem::replace(&mut self.stack_cap, cap);
        // No safepoint since the copy: the collector sees one stack or the other, both complete.
        self.rs().stack = stack;
        if safe {
            self.free_retired();
            // SAFETY: no native runs on this VM, so nothing reads the old stack.
            unsafe { free_stack(old, old_cap) };
        } else {
            self.retired.push((old, old_cap));
        }
        Ok(())
    }

    /// Under `CATX_GC_STRESS`, moves a small register stack on a call back from a native (until
    /// the stacks kept for natives add up to [`STRESS_RETIRED_LIMIT`] values).
    #[inline(always)]
    fn stress_move(&mut self) -> RtResult<()> {
        if self.stress {
            self.stress_move_slow()?;
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn stress_move_slow(&mut self) -> RtResult<()> {
        if self.rs().frames.is_empty() {
            self.free_retired();
        } else if self.stack_cap <= STRESS_MOVE_LIMIT
            && self.retired.iter().map(|&(_, n)| n).sum::<usize>() < STRESS_RETIRED_LIMIT
        {
            self.grow_stack(0, false)?;
        }
        Ok(())
    }

    /// Runs frames until frame index `stop` returns, handling non-local exits and errors.
    fn execute(&mut self, stop: usize) -> RtResult<Value> {
        loop {
            match self.run(stop) {
                Ok(v) => return Ok(v),
                Err(Trap::Unwind { frame, kind, value }) => {
                    let target = self.rs().frames[stop..].iter().rposition(|f| f.serial == frame).map(|i| i + stop);
                    let Some(idx) = target else {
                        self.unwind_to(stop);
                        return Err(Trap::Unwind { frame, kind, value });
                    };
                    self.unwind_to(idx + 1);
                    if kind == UNWIND_RETURN {
                        let f = self.rs().frames.pop().unwrap();
                        self.clear_regs(f.base, f.base + f.func.nregs);
                        self.release_locks();
                        if idx == stop {
                            return Ok(value);
                        }
                        self.set_reg(f.ret, value);
                    } else {
                        // Break: the frame was suspended in a call (instruction + data word);
                        // the call's result register gets nil and execution continues after it.
                        let f = *self.rs().frames.last().unwrap();
                        let w = f.func.code[f.pc - 2];
                        self.set_reg(f.base + a_of(w), value);
                    }
                }
                Err(e) => {
                    if let Trap::Panic(_) = &e {
                        if self.panic_trace.is_none() {
                            self.panic_trace = Some(self.backtrace());
                        }
                    }
                    self.unwind_to(stop);
                    return Err(e);
                }
            }
        }
    }

    /// Pops frames down to `depth`, releasing locks they held.
    fn unwind_to(&mut self, depth: usize) {
        if let Some(f) = self.rs().frames.get(depth) {
            // The popped frames' registers would otherwise stay reachable from the scan of a
            // lower frame's register window (see `return_value!`).
            let (lo, hi) = (f.base, self.stack_top());
            self.clear_regs(lo, hi);
        }
        self.rs().frames.truncate(depth);
        self.release_locks();
    }

    fn release_locks(&mut self) {
        let depth = self.rs().frames.len();
        while let Some(&(d, v)) = self.rs().held_locks.last() {
            if d < depth {
                break;
            }
            self.rs().held_locks.pop();
            let h = unsafe { &*v.as_ptr() };
            if h.is_locked_by(self.m.tid) {
                h.unlock();
            }
        }
    }

    /// Restores this VM after a Rust panic unwound through it (an internal error): pops every
    /// frame, releasing the object locks they held, and returns the Catallaxy backtrace at the
    /// point of the panic. The thread is left in the running state. Native guards (`Mutex`
    /// closures, the runtime's own object locks) were released by the unwind itself.
    pub fn recover_from_internal_panic(&mut self) -> Vec<String> {
        let trace = self.backtrace();
        self.unwind_to(0);
        self.panic_trace = None;
        self.no_park = 0;
        // Nothing runs on this VM now, so no native reads a retired stack.
        self.free_retired();
        if self.m.rec.state() != catallaxy_gc::ThreadState::Running {
            catallaxy_gc::leave_native(&self.m.rec);
        }
        trace
    }

    /// Number of active frames on this VM's stack.
    pub fn frame_depth(&self) -> usize {
        self.rs().frames.len()
    }

    pub fn backtrace(&self) -> Vec<String> {
        let mut out = Vec::new();
        for f in self.rs().frames.iter().rev() {
            let pc = f.pc.saturating_sub(1);
            let file = program::file_name(f.func.file);
            out.push(format!("{} ({}:{})", f.func.name, file, f.func.line_at(pc)));
        }
        out
    }

    /// Current source location (for warnings).
    pub fn location(&self) -> String {
        match self.rs().frames.last() {
            Some(f) => format!("{}:{}", program::file_name(f.func.file), f.func.line_at(f.pc.saturating_sub(1))),
            None => "<native>".into(),
        }
    }

    // ---- the interpreter loop --------------------------------------------------------------

    #[inline(never)]
    fn run(&mut self, stop: usize) -> RtResult<Value> {
        let tid = self.m.tid;
        // This thread's safepoint flag (raised for a major collection or a full nursery).
        let poll: *const std::sync::atomic::AtomicBool = self.m.rec.poll_flag();
        // The root state's address never changes; keep it in a register.
        let rs: *mut RootState = self.rs;
        macro_rules! frames {
            () => {
                unsafe { &mut (*rs).frames }
            };
        }
        let mut fr = *frames!().last().unwrap();
        let mut code: *const u32 = fr.func.code.as_ptr();
        let mut pc = fr.pc;
        let mut regs: *mut Value = unsafe { self.stack.add(fr.base) };

        macro_rules! r {
            ($i:expr) => {
                unsafe { *regs.add($i) }
            };
        }
        macro_rules! set {
            ($i:expr, $v:expr) => {{
                let v = $v;
                unsafe { *regs.add($i) = v }
            }};
        }
        macro_rules! save {
            () => {
                frames!().last_mut().unwrap().pc = pc;
            };
        }
        macro_rules! tri {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => {
                        save!();
                        return Err(e);
                    }
                }
            };
        }
        // Takes a fused compare-and-jump: short offsets live in C (0..=127), long ones in the
        // data word, so the common case has no load on the path to the next instruction.
        macro_rules! fused_jump {
            ($w:expr) => {{
                let c = c_of($w);
                let off = if c < 0x80 { c as isize } else { (unsafe { *code.add(pc) }) as i32 as isize };
                pc = (pc as isize + 1 + off) as usize;
            }};
        }
        macro_rules! data {
            () => {{
                let d = unsafe { *code.add(pc) };
                pc += 1;
                d
            }};
        }
        macro_rules! load_frame {
            () => {{
                fr = *frames!().last().unwrap();
                code = fr.func.code.as_ptr();
                pc = fr.pc;
                regs = unsafe { self.stack.add(fr.base) };
            }};
        }
        // A safepoint's slow path (the frame's pc is saved): takes part in a pending collection,
        // then panics with a MemoryError if the heap is over `CATX_MAX_HEAP` or the system
        // refused to grow it (see `catallaxy_gc::limits::take_pending_error`).
        macro_rules! poll_slow {
            () => {{
                catallaxy_gc::safepoint_slow(&self.m.rec);
                if let Some(msg) = catallaxy_gc::limits::take_pending_error() {
                    return Err(Trap::Panic(msg));
                }
            }};
        }
        // Pushes a bytecode frame for `f` whose arguments are already at absolute slot `base`.
        macro_rules! enter {
            ($f:expr, $base:expr, $ret:expr, $closure:expr, $nargs:expr) => {{
                let f: &'static Func = $f;
                let base: usize = $base;
                if base + f.nregs >= self.stack_cap || frames!().len() >= MAX_FRAMES {
                    save!();
                    // May move the stack; `regs` is reset below.
                    tri!(self.grow_stack(base + f.nregs, stop == 0));
                }
                for i in ($nargs)..f.nparams {
                    unsafe { *self.stack.add(base + i) = Value::EMPTY };
                }
                for i in f.nparams.max($nargs)..f.nregs {
                    unsafe { *self.stack.add(base + i) = Value::NIL };
                }
                save!();
                let top = (base + f.nregs).max(fr.top);
                // The new frame is built here and kept in locals, not read back from the stack.
                fr = Frame { func: f, pc: 0, base, ret: $ret, closure: $closure, serial: 0, top };
                frames!().push(fr);
                code = f.code.as_ptr();
                pc = 0;
                regs = unsafe { self.stack.add(base) };
                if unsafe { (*poll).load(Ordering::Relaxed) } {
                    poll_slow!();
                }
            }};
        }
        // Pops the current frame (its return slot is already in `fr`) and resumes the caller.
        //
        // The caller's registers above the return slot are cleared: the callee and its callees
        // left values there, and the collector scans the caller's whole register window, so
        // they would otherwise keep dead objects alive. The compiler allocates registers like a
        // stack and a call's register is the first free one, so none of them is live in the
        // caller (a bytecode callee overwrites them on entry anyway). Slots above the caller's
        // window are not scanned and are reset by the next frame that covers them.
        macro_rules! return_value {
            ($v:expr) => {{
                let v: Value = $v;
                let ret = fr.ret;
                let frames = frames!();
                frames.pop();
                if unsafe { !(*rs).held_locks.is_empty() } {
                    self.release_locks();
                }
                if frames!().len() == stop {
                    return Ok(v);
                }
                load_frame!();
                let end = fr.base + fr.func.nregs;
                let mut i = ret + 1;
                while i < end {
                    unsafe { *self.stack.add(i) = Value::NIL };
                    i += 1;
                }
                unsafe { *self.stack.add(ret) = v };
            }};
        }
        // Calls something that may run bytecode on this VM (a native, a user `==` or `to_s`, a
        // `Dyn` dispatch...), which may grow the register stack and so move it: reloads the
        // register window afterwards. Every call that takes `self` goes through here.
        macro_rules! vm_call {
            ($e:expr) => {{
                let v = tri!($e);
                regs = unsafe { self.stack.add(fr.base) };
                v
            }};
        }
        macro_rules! call_native {
            ($nf:expr, $a:expr, $nargs:expr) => {{
                save!();
                let args = unsafe { std::slice::from_raw_parts(regs.add($a), $nargs) };
                let v = vm_call!(($nf)(self, args));
                set!($a, v);
            }};
        }

        loop {
            let w = unsafe { *code.add(pc) };
            pc += 1;
            let a = a_of(w);
            #[cfg(feature = "op-profile")]
            crate::profile::record(op_of(w));
            match Op::from_u8(op_of(w)) {
                Op::Nop => {}
                Op::Move => set!(a, r!(b_of(w))),
                Op::LoadK => set!(a, fr.func.consts[bx_of(w)]),
                Op::LoadInt => set!(a, Value::small_int(sbx_of(w) as i64)),
                Op::LoadNil => set!(a, Value::NIL),
                Op::LoadTrue => set!(a, Value::TRUE),
                Op::LoadFalse => set!(a, Value::FALSE),
                Op::LoadEmpty => set!(a, Value::EMPTY),
                Op::JmpIfNotEmpty => {
                    if !r!(a).is_empty_marker() {
                        pc = (pc as isize + sbx_of(w)) as usize;
                    }
                }
                Op::GetGlobal => {
                    let d = data!() as usize;
                    let v = Value(program::global(d).load(Ordering::Acquire));
                    if v.is_empty_marker() {
                        save!();
                        return Err(Trap::Panic(format!(
                            "NameError: `{}` used before it was initialized",
                            program::global_name(d)
                        )));
                    }
                    set!(a, v);
                }
                Op::SetGlobal => {
                    let d = data!() as usize;
                    let v = r!(a);
                    tri!(ownership::publish_into_shared(&self.m, v));
                    program::global(d).store(v.0, Ordering::Release);
                }
                // Closures, cells and their registers are only made and read by compiled code
                // (never by the program's values), so their kinds are invariants of the compiler:
                // the unchecked views below are safe.
                Op::GetCap => {
                    let c = unsafe { as_closure(fr.closure) };
                    set!(a, c.caps()[b_of(w)]);
                }
                Op::NewCell => {
                    let v = r!(b_of(w));
                    let c = self.m.alloc_cell(v);
                    set!(a, c);
                }
                Op::GetCell => {
                    let c = r!(b_of(w));
                    let h = unsafe { &*c.as_ptr() };
                    let acc = tri!(check_read(&self.m, h));
                    let cell = unsafe { as_cell(c) };
                    let v = if acc == Access::Local {
                        cell.value.load(Ordering::Relaxed)
                    } else {
                        cell.value.load(Ordering::Acquire)
                    };
                    set!(a, Value(v));
                }
                Op::SetCell => {
                    let c = r!(a);
                    let v = r!(b_of(w));
                    let h = unsafe { &*c.as_ptr() };
                    let acc = tri!(check_write(&self.m, h));
                    let cell = unsafe { as_cell(c) };
                    if acc == Access::Local {
                        cell.value.store(v.0, Ordering::Relaxed);
                    } else {
                        tri!(ownership::publish_into_shared(&self.m, v));
                        cell.value.store(v.0, Ordering::Release);
                    }
                }

                // ---- Int ----
                Op::AddI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    if Value::both_small_ints(x, y) {
                        let s = x.as_small_int() + y.as_small_int();
                        if catallaxy_runtime::value::fits_small(s) {
                            set!(a, Value::small_int(s));
                            continue;
                        }
                    }
                    tri!(slow::expect_ints(x, y, "+"));
                    let v = ops::int_add(&mut self.m, x, y);
                    set!(a, v);
                }
                Op::SubI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    if Value::both_small_ints(x, y) {
                        let s = x.as_small_int() - y.as_small_int();
                        if catallaxy_runtime::value::fits_small(s) {
                            set!(a, Value::small_int(s));
                            continue;
                        }
                    }
                    tri!(slow::expect_ints(x, y, "-"));
                    let v = ops::int_sub(&mut self.m, x, y);
                    set!(a, v);
                }
                Op::MulI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    if Value::both_small_ints(x, y) {
                        if let Some(p) = x.as_small_int().checked_mul(y.as_small_int()) {
                            if catallaxy_runtime::value::fits_small(p) {
                                set!(a, Value::small_int(p));
                                continue;
                            }
                        }
                    }
                    tri!(slow::expect_ints(x, y, "*"));
                    let v = ops::int_mul(&mut self.m, x, y);
                    set!(a, v);
                }
                Op::DivI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    tri!(slow::expect_ints(x, y, "/"));
                    let v = tri!(ops::int_div(&mut self.m, x, y));
                    set!(a, v);
                }
                Op::ModI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    if Value::both_small_ints(x, y) {
                        let (p, q) = (x.as_small_int(), y.as_small_int());
                        if q > 0 && p >= 0 {
                            set!(a, Value::small_int(p % q));
                            continue;
                        }
                    }
                    tri!(slow::expect_ints(x, y, "%"));
                    let v = tri!(ops::int_mod(&mut self.m, x, y));
                    set!(a, v);
                }
                Op::PowI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    tri!(slow::expect_ints(x, y, "**"));
                    let v = tri!(ops::int_pow(&mut self.m, x, y));
                    set!(a, v);
                }
                Op::AddIK => {
                    let x = r!(b_of(w));
                    let k = sc_of(w);
                    if x.is_small_int() {
                        let s = x.as_small_int() + k;
                        if catallaxy_runtime::value::fits_small(s) {
                            set!(a, Value::small_int(s));
                            continue;
                        }
                    }
                    let kv = Value::small_int(k);
                    tri!(slow::expect_ints(x, kv, "+"));
                    let v = ops::int_add(&mut self.m, x, kv);
                    set!(a, v);
                }
                Op::NegI => {
                    let x = r!(b_of(w));
                    tri!(slow::expect_ints(x, x, "-"));
                    let v = ops::int_neg(&mut self.m, x);
                    set!(a, v);
                }
                Op::BAnd | Op::BOr | Op::BXor | Op::Shl | Op::Shr => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    tri!(slow::expect_ints(x, y, "bitwise operator"));
                    let opc = match Op::from_u8(op_of(w)) {
                        Op::BAnd => b'&',
                        Op::BOr => b'|',
                        Op::BXor => b'^',
                        Op::Shl => b'<',
                        _ => b'>',
                    };
                    let v = tri!(ops::int_bitop(&mut self.m, opc, x, y));
                    set!(a, v);
                }
                Op::BNot => {
                    let x = r!(b_of(w));
                    tri!(slow::expect_ints(x, x, "~"));
                    let m1 = Value::small_int(-1);
                    let v = tri!(ops::int_bitop(&mut self.m, b'^', x, m1));
                    set!(a, v);
                }

                // ---- Float ----
                Op::AddF => set!(a, Value::from_f64(tri!(slow::f(r!(b_of(w)))) + tri!(slow::f(r!(c_of(w)))))),
                Op::SubF => set!(a, Value::from_f64(tri!(slow::f(r!(b_of(w)))) - tri!(slow::f(r!(c_of(w)))))),
                Op::MulF => set!(a, Value::from_f64(tri!(slow::f(r!(b_of(w)))) * tri!(slow::f(r!(c_of(w)))))),
                Op::DivF => set!(a, Value::from_f64(tri!(slow::f(r!(b_of(w)))) / tri!(slow::f(r!(c_of(w)))))),
                Op::ModF => {
                    let (x, y) = (tri!(slow::f(r!(b_of(w)))), tri!(slow::f(r!(c_of(w)))));
                    let m = x % y;
                    let m = if m != 0.0 && (m < 0.0) != (y < 0.0) { m + y } else { m };
                    set!(a, Value::from_f64(m));
                }
                Op::PowF => set!(a, Value::from_f64(tri!(slow::f(r!(b_of(w)))).powf(tri!(slow::f(r!(c_of(w))))))),
                Op::NegF => set!(a, Value::from_f64(-tri!(slow::f(r!(b_of(w)))))),
                Op::IToF => {
                    let x = r!(b_of(w));
                    let f = if x.is_small_int() {
                        x.as_small_int() as f64
                    } else if x.is_float() {
                        x.as_f64()
                    } else {
                        tri!(slow::expect_ints(x, x, "to_f"));
                        ops::int_to_f64(x)
                    };
                    set!(a, Value::from_f64(f));
                }

                // ---- comparisons ----
                Op::EqI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    let eq = if Value::both_small_ints(x, y) {
                        x.0 == y.0
                    } else {
                        ops::is_int(x) && ops::is_int(y) && ops::int_cmp(x, y).is_eq()
                    };
                    set!(a, Value::bool(eq));
                }
                Op::LtI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    let lt = if Value::both_small_ints(x, y) {
                        x.as_small_int() < y.as_small_int()
                    } else {
                        tri!(slow::expect_ints(x, y, "<"));
                        ops::int_cmp(x, y).is_lt()
                    };
                    set!(a, Value::bool(lt));
                }
                Op::LeI => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    let le = if Value::both_small_ints(x, y) {
                        x.as_small_int() <= y.as_small_int()
                    } else {
                        tri!(slow::expect_ints(x, y, "<="));
                        ops::int_cmp(x, y).is_le()
                    };
                    set!(a, Value::bool(le));
                }
                Op::EqF => set!(a, Value::bool(tri!(slow::f(r!(b_of(w)))) == tri!(slow::f(r!(c_of(w)))))),
                Op::LtF => set!(a, Value::bool(tri!(slow::f(r!(b_of(w)))) < tri!(slow::f(r!(c_of(w)))))),
                Op::LeF => set!(a, Value::bool(tri!(slow::f(r!(b_of(w)))) <= tri!(slow::f(r!(c_of(w)))))),
                Op::Eq | Op::Ne => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let eq = if x.0 == y.0 && !x.is_float() { true } else { vm_call!(ops::values_eq(self, x, y)) };
                    let res = if Op::from_u8(op_of(w)) == Op::Eq { eq } else { !eq };
                    set!(a, Value::bool(res));
                }
                Op::Lt | Op::Le => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let o = vm_call!(ops::compare_or_panic(self, x, y));
                    let res = if Op::from_u8(op_of(w)) == Op::Lt { o.is_lt() } else { o.is_le() };
                    set!(a, Value::bool(res));
                }
                Op::Cmp => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let o = vm_call!(ops::compare_or_panic(self, x, y));
                    set!(a, Value::small_int(o as i64));
                }
                Op::Not => set!(a, Value::bool(!r!(b_of(w)).is_true())),
                Op::Is => {
                    let d = data!();
                    let x = r!(b_of(w));
                    save!();
                    let ok = vm_call!(slow::conforms(self, x, d));
                    set!(a, Value::bool(ok));
                }
                Op::IsIface => {
                    let d = data!();
                    let x = r!(b_of(w));
                    set!(a, Value::bool(slow::has_iface(x, d)));
                }
                Op::IsNil => set!(a, Value::bool(r!(b_of(w)).is_nil())),
                Op::IsKind => {
                    let x = r!(b_of(w));
                    set!(a, Value::bool(x.is_ptr() && unsafe { heap_class(x.as_ptr()) }.kind as usize == c_of(w)));
                }

                // ---- generic arithmetic ----
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let v = vm_call!(slow::generic_arith(self, Op::from_u8(op_of(w)), x, y));
                    set!(a, v);
                }
                Op::Neg => {
                    let x = r!(b_of(w));
                    save!();
                    let v = vm_call!(slow::generic_neg(self, x));
                    set!(a, v);
                }

                // ---- strings ----
                Op::Concat => {
                    let (b, n) = (b_of(w), c_of(w));
                    for i in 0..n {
                        if str_of(r!(b + i)).is_none() {
                            // A value of the wrong type (through `Dyn` or a hole in the checker).
                            save!();
                            // (A static message: building one here, even in a cold call, costs
                            // the whole interpreter loop's register allocation.)
                            return Err(Trap::Panic("TypeError: expected String operands to concatenate".into()));
                        }
                    }
                    // The parts stay in their registers, so they stay valid while they are
                    // copied, even if the allocator refuses and a collection runs.
                    let v = tri!(self.m.try_alloc_concat(n, |i| {
                        let x = r!(b + i);
                        // SAFETY: every part was checked to be a string above.
                        unsafe { as_str_obj(x) }
                    }));
                    set!(a, v);
                }
                Op::ToS => {
                    let x = r!(b_of(w));
                    if str_of(x).is_some() {
                        set!(a, x);
                    } else {
                        save!();
                        let s = vm_call!(ops::to_s(self, x));
                        let v = self.m.alloc_str(&s);
                        set!(a, v);
                    }
                }
                Op::Format => {
                    let spec = FormatSpec::decode([data!(), data!()]);
                    let x = r!(b_of(w));
                    save!();
                    let s = vm_call!(fmtspec::format_value(self, x, &spec));
                    let v = self.m.alloc_str(&s);
                    set!(a, v);
                }

                // ---- control ----
                Op::Jmp => {
                    let off = sax_of(w);
                    pc = (pc as isize + off) as usize;
                    if off < 0 && unsafe { (*poll).load(Ordering::Relaxed) } {
                        save!();
                        poll_slow!();
                    }
                }
                Op::JNLtI => {
                    let (x, y) = (r!(a), r!(b_of(w)));
                    let jump = if Value::both_small_ints(x, y) {
                        x.as_small_int() >= y.as_small_int()
                    } else {
                        save!();
                        tri!(slow::fused_jump(Op::JNLtI, x, y))
                    };
                    if jump {
                        fused_jump!(w);
                    } else {
                        pc += 1;
                    }
                }
                Op::JNLeI => {
                    let (x, y) = (r!(a), r!(b_of(w)));
                    let jump = if Value::both_small_ints(x, y) {
                        x.as_small_int() > y.as_small_int()
                    } else {
                        save!();
                        tri!(slow::fused_jump(Op::JNLeI, x, y))
                    };
                    if jump {
                        fused_jump!(w);
                    } else {
                        pc += 1;
                    }
                }
                Op::JNLtIK | Op::JNLeIK | Op::JNGtIK | Op::JNGeIK | Op::JNEqIK | Op::JEqIK => {
                    let x = r!(a);
                    let k = b_of(w) as u8 as i8 as i64;
                    let ord = if x.is_small_int() {
                        x.as_small_int().cmp(&k)
                    } else {
                        save!();
                        tri!(slow::cmp_int_imm(Op::from_u8(op_of(w)), x, k))
                    };
                    let jump = match Op::from_u8(op_of(w)) {
                        Op::JNLtIK => !ord.is_lt(),
                        Op::JNLeIK => !ord.is_le(),
                        Op::JNGtIK => !ord.is_gt(),
                        Op::JNGeIK => !ord.is_ge(),
                        Op::JNEqIK => !ord.is_eq(),
                        _ => ord.is_eq(),
                    };
                    if jump {
                        fused_jump!(w);
                    } else {
                        pc += 1;
                    }
                }
                Op::JNEqI | Op::JEqI => {
                    let (x, y) = (r!(a), r!(b_of(w)));
                    let eq = if Value::both_small_ints(x, y) {
                        x.0 == y.0
                    } else {
                        ops::is_int(x) && ops::is_int(y) && ops::int_cmp(x, y).is_eq()
                    };
                    if eq == (op_of(w) == Op::JEqI as u8) {
                        fused_jump!(w);
                    } else {
                        pc += 1;
                    }
                }
                Op::JNLtF | Op::JNLeF => {
                    let (x, y) = (r!(a), r!(b_of(w)));
                    // Negated on purpose: a NaN comparison is false, so it jumps.
                    #[allow(clippy::neg_cmp_op_on_partial_ord)]
                    let jump = if x.is_float() && y.is_float() {
                        let (x, y) = (x.as_f64(), y.as_f64());
                        if op_of(w) == Op::JNLtF as u8 {
                            !(x < y)
                        } else {
                            !(x <= y)
                        }
                    } else {
                        save!();
                        tri!(slow::fused_jump(Op::from_u8(op_of(w)), x, y))
                    };
                    if jump {
                        fused_jump!(w);
                    } else {
                        pc += 1;
                    }
                }
                Op::JmpIf => {
                    if r!(a).is_true() {
                        pc = (pc as isize + sbx_of(w)) as usize;
                    }
                }
                Op::JmpIfNot => {
                    if !r!(a).is_true() {
                        pc = (pc as isize + sbx_of(w)) as usize;
                    }
                }
                Op::JmpIfNil => {
                    if r!(a).is_nil() {
                        pc = (pc as isize + sbx_of(w)) as usize;
                    }
                }
                Op::JmpIfNotNil => {
                    if !r!(a).is_nil() {
                        pc = (pc as isize + sbx_of(w)) as usize;
                    }
                }
                Op::Return => {
                    let v = r!(a);
                    return_value!(v);
                }
                Op::ReturnNil => return_value!(Value::NIL),

                // ---- calls ----
                Op::Call => {
                    let fid = data!();
                    let nargs = b_of(w);
                    let f = program::func(fid);
                    if let Some(nf) = f.native {
                        call_native!(nf, a, nargs);
                    } else {
                        let base = fr.base + a;
                        enter!(f, base, base, Value::NIL, nargs);
                    }
                }
                Op::CallNative => {
                    let nid = data!();
                    let nargs = b_of(w);
                    let nf = program::native(nid);
                    call_native!(nf, a, nargs);
                }
                Op::CallClosure => {
                    let c = r!(a);
                    let nargs = b_of(w);
                    if !c.is_ptr() || unsafe { heap_class(c.as_ptr()) }.kind != ObjKind::Closure {
                        save!();
                        return Err(Trap::Panic(format!("cannot call a value of type {}", class_of(c).name)));
                    }
                    let f = program::func(unsafe { as_closure(c) }.func);
                    if let Some(nf) = f.native {
                        save!();
                        let args = unsafe { std::slice::from_raw_parts(regs.add(a + 1), nargs) };
                        let v = vm_call!(nf(self, args));
                        set!(a, v);
                    } else {
                        let base = fr.base + a + 1;
                        enter!(f, base, fr.base + a, c, nargs);
                    }
                }
                Op::CallMethod => {
                    let d = data!() as usize;
                    let nargs = b_of(w);
                    let recv = r!(a);
                    let ic = &fr.func.ics[d];
                    let cid = catallaxy_runtime::class::class_id_of(recv);
                    let c = ic.cache.load(Ordering::Relaxed);
                    let fid = if (c >> 32) as u32 == cid.wrapping_add(1) {
                        c as u32
                    } else {
                        let fid = match class_of(recv).lookup(ic.name) {
                            Some(f) => f,
                            None if ic.fallback != NO_FUNC => ic.fallback,
                            None => {
                                save!();
                                return Err(Trap::Panic(format!(
                                    "NoMethodError: undefined method `{}` for {}",
                                    ic.name,
                                    class_of(recv).name
                                )));
                            }
                        };
                        ic.cache.store(((cid.wrapping_add(1) as u64) << 32) | fid as u64, Ordering::Relaxed);
                        fid
                    };
                    let f = program::func(fid);
                    if let Some(nf) = f.native {
                        call_native!(nf, a, nargs);
                    } else {
                        let base = fr.base + a;
                        enter!(f, base, base, Value::NIL, nargs);
                    }
                }
                Op::CallVirtual => {
                    let slot = data!() as usize;
                    let nargs = b_of(w);
                    let recv = r!(a);
                    let fid = if recv.is_ptr() { unsafe { heap_class(recv.as_ptr()) }.vtable.get(slot) } else { None };
                    let Some(&fid) = fid else {
                        // A value of the wrong type (through `Dyn` or a hole in the checker).
                        save!();
                        return Err(slow::virtual_on_wrong_type(recv));
                    };
                    let f = program::func(fid);
                    if let Some(nf) = f.native {
                        call_native!(nf, a, nargs);
                    } else {
                        let base = fr.base + a;
                        enter!(f, base, base, Value::NIL, nargs);
                    }
                }

                // ---- objects ----
                Op::New => {
                    let cid = data!();
                    let cls = catallaxy_runtime::class::class_by_id(cid);
                    let v = self.m.alloc_instance(cls);
                    if cls.capability == catallaxy_runtime::class::Capability::Shared {
                        unsafe { (*v.as_ptr()).set_state(ObjState::Shared) };
                    }
                    set!(a, v);
                }
                Op::GetField => {
                    let o = r!(b_of(w));
                    let i = c_of(w);
                    let Some(inst) = instance_with_field(o, i) else {
                        save!();
                        return Err(slow::field_on_non_object(o));
                    };
                    let h = &inst.header;
                    let v = if h.is_local_or_frozen(tid) {
                        inst.get(i)
                    } else {
                        save!();
                        tri!(check_read(&self.m, h));
                        inst.get_acquire(i)
                    };
                    set!(a, v);
                }
                Op::GetField2 => {
                    let d = data!();
                    let (i, j) = ((d & 0xffff) as usize, (d >> 16) as usize);
                    let (o1, o2) = (r!(b_of(w)), r!(c_of(w)));
                    if let (Some(x1), Some(x2)) = (instance_with_field(o1, i), instance_with_field(o2, j)) {
                        if x1.header.is_local_or_frozen(tid) && x2.header.is_local_or_frozen(tid) {
                            set!(a, x1.get(i));
                            set!(a + 1, x2.get(j));
                            continue;
                        }
                    }
                    save!();
                    let v1 = vm_call!(slow::get_field(self, o1, i));
                    set!(a, v1);
                    let v2 = vm_call!(slow::get_field(self, o2, j));
                    set!(a + 1, v2);
                }
                Op::SetField => {
                    let o = r!(a);
                    let i = b_of(w);
                    let v = r!(c_of(w));
                    let Some(inst) = instance_with_field(o, i) else {
                        save!();
                        return Err(slow::field_on_non_object(o));
                    };
                    let h = &inst.header;
                    let meta = h.meta();
                    if Header::state_of(meta) == ObjState::Owned && Header::owner_of(meta) == tid {
                        if Header::needs_remember(meta) {
                            ownership::remember(&self.m, h);
                        }
                        inst.set(i, v);
                    } else {
                        save!();
                        let acc = tri!(check_write(&self.m, h));
                        if acc == Access::Shared {
                            tri!(ownership::publish_into_shared(&self.m, v));
                            inst.set_release(i, v);
                        } else {
                            inst.set(i, v);
                        }
                    }
                }

                // ---- collections ----
                Op::NewList => {
                    let (b, n) = (b_of(w), c_of(w));
                    let items = unsafe { std::slice::from_raw_parts(regs.add(b), n) };
                    // Small literals (the common case) keep their items inline, with spare room.
                    let v = self.m.alloc_list_from_slice(items, n);
                    set!(a, v);
                }
                Op::NewDict => {
                    let (b, n) = (b_of(w), c_of(w));
                    save!();
                    let d = self.m.alloc_dict(catallaxy_runtime::dict::DictTable::with_capacity(n));
                    // Keep the new dict rooted while user `hash`/`==` may run.
                    self.extra_roots().push(d);
                    let mut err = None;
                    for i in 0..n {
                        let (k, v) = (r!(b + 2 * i), r!(b + 2 * i + 1));
                        if let Err(e) = slow::dict_set(self, d, k, v) {
                            err = Some(e);
                            break;
                        }
                        // A user `hash` or `==` may have moved the register stack.
                        regs = unsafe { self.stack.add(fr.base) };
                    }
                    self.extra_roots().pop();
                    if let Some(e) = err {
                        return Err(e);
                    }
                    set!(a, d);
                }
                Op::NewTuple => {
                    let (b, n) = (b_of(w), c_of(w));
                    let items: Vec<Value> = (0..n).map(|i| r!(b + i)).collect();
                    let v = self.m.alloc_tuple(&items);
                    set!(a, v);
                }
                Op::NewRange | Op::NewRangeEx => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    let v = self.m.alloc_range(x, y, Op::from_u8(op_of(w)) == Op::NewRangeEx);
                    set!(a, v);
                }
                Op::ListGet => {
                    let l = r!(b_of(w));
                    let i = r!(c_of(w));
                    // A value of another kind takes the slow path, which raises the TypeError.
                    if kind_of(l) == ObjKind::List
                        && i.is_small_int()
                        && unsafe { &*l.as_ptr() }.is_local_or_frozen(tid)
                    {
                        // SAFETY: checked to be a list just above.
                        let list = unsafe { as_list(l) };
                        let mut idx = i.as_small_int();
                        if idx < 0 {
                            idx += list.len() as i64;
                        }
                        if let Some(v) = (idx >= 0).then(|| list.get_local(idx as usize)).flatten() {
                            set!(a, v);
                            continue;
                        }
                    }
                    save!();
                    let v = vm_call!(slow::list_get(self, l, i));
                    set!(a, v);
                }
                Op::ListSet => {
                    let (l, i, v) = (r!(a), r!(b_of(w)), r!(c_of(w)));
                    save!();
                    vm_call!(slow::list_set(self, l, i, v));
                }
                Op::ListLen => {
                    let l = r!(b_of(w));
                    // A value of another kind takes the slow path, which raises the TypeError.
                    let n = if kind_of(l) == ObjKind::List && unsafe { &*l.as_ptr() }.is_local_or_frozen(tid) {
                        // SAFETY: checked to be a list just above.
                        unsafe { as_list(l) }.len()
                    } else {
                        save!();
                        vm_call!(slow::list_len(self, l))
                    };
                    set!(a, Value::small_int(n as i64));
                }
                Op::ListPush => {
                    let (l, v) = (r!(a), r!(b_of(w)));
                    save!();
                    vm_call!(slow::list_push(self, l, v));
                }
                Op::DynGet => {
                    let d = data!() as usize;
                    let recv = r!(b_of(w));
                    let ic = &fr.func.ics[d];
                    if recv.is_ptr() {
                        let h = unsafe { &*recv.as_ptr() };
                        if h.is_local_or_frozen(tid) && unsafe { heap_class(h).kind } == ObjKind::Dict {
                            // SAFETY: checked to be a dict just above.
                            let t = unsafe { as_dict(recv).table() };
                            if let Some(shape) = t.shape() {
                                let c = ic.cache.load(Ordering::Relaxed);
                                if (c >> 32) as u32 == shape.id.wrapping_add(1) {
                                    if let Some(v) = t.slot_value(c as u32 as usize) {
                                        set!(a, v);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    save!();
                    let v = vm_call!(slow::dyn_get(self, recv, ic));
                    set!(a, v);
                }
                Op::Index => {
                    let (x, i) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let v = vm_call!(slow::index_get(self, x, i));
                    set!(a, v);
                }
                Op::SetIndex => {
                    let (x, i, v) = (r!(a), r!(b_of(w)), r!(c_of(w)));
                    save!();
                    vm_call!(slow::index_set(self, x, i, v));
                }
                Op::TupleGet => {
                    let t = r!(b_of(w));
                    let i = c_of(w);
                    if kind_of(t) != ObjKind::Tuple {
                        save!();
                        return Err(slow::wrong_type("Tuple", t));
                    }
                    // SAFETY: checked to be a tuple just above.
                    let elems = unsafe { as_tuple(t) }.elems();
                    if i >= elems.len() {
                        save!();
                        return Err(Trap::Panic("internal error: tuple index out of range".into()));
                    }
                    set!(a, elems[i]);
                }
                Op::Closure => {
                    let fid = data!();
                    let f = program::func(fid);
                    let mut caps = Vec::with_capacity(f.captures.len());
                    for c in f.captures.iter() {
                        caps.push(match *c {
                            CapSrc::Reg(r) => r!(r as usize),
                            CapSrc::Cap(i) => unsafe { as_closure(fr.closure) }.caps()[i as usize],
                        });
                    }
                    let v = self.m.alloc_closure(fid, &caps);
                    set!(a, v);
                }
                Op::EnumTag => {
                    let x = r!(b_of(w));
                    let tag = if x.is_ptr() { unsafe { heap_class(x.as_ptr()) }.variant_tag } else { None };
                    match tag {
                        Some(t) => set!(a, Value::small_int(t as i64)),
                        None => set!(a, Value::small_int(-1)),
                    }
                }
                Op::Unwrap => {
                    let x = r!(b_of(w));
                    if x.is_nil() {
                        save!();
                        return Err(Trap::Panic("unwrapped nil (`!` on a nil value)".into()));
                    }
                    set!(a, x);
                }
                Op::AsCast => {
                    let d = data!();
                    let x = r!(b_of(w));
                    save!();
                    let ok = vm_call!(slow::conforms(self, x, d));
                    set!(a, if ok { x } else { Value::NIL });
                }
                Op::TagElems => {
                    let d = data!();
                    slow::tag_elems(r!(a), d);
                }
                Op::CheckArgs => {
                    let d = data!();
                    let c = r!(a);
                    if slow::is_tagged(c) {
                        let (b, n) = (b_of(w), c_of(w));
                        // SAFETY: the registers are live for the frame; checking allocates no
                        // heap objects and runs no code, so they stay valid and unchanged.
                        let args: &[Value] = unsafe { std::slice::from_raw_parts(regs.add(b), n) };
                        save!();
                        vm_call!(slow::check_args(self, c, args, d));
                    }
                }
                Op::CheckSet => {
                    let (k, v) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    vm_call!(slow::check_set(self, r!(a), k, v));
                }
                Op::FrameId => {
                    let next = self.next_serial;
                    let f = frames!().last_mut().unwrap();
                    let assigned = f.serial == 0;
                    if assigned {
                        f.serial = next;
                    }
                    let s = f.serial as i64;
                    if assigned {
                        self.next_serial += 1;
                    }
                    set!(a, Value::small_int(s));
                }
                Op::Unwind => {
                    let serial = r!(b_of(w)).as_small_int() as u64;
                    let value = r!(c_of(w));
                    save!();
                    return Err(Trap::Unwind { frame: serial, kind: a as u8, value });
                }
                Op::Panic => {
                    let msg = r!(a);
                    save!();
                    let s = str_of(msg).map(|s| s.to_string()).unwrap_or_else(|| "panic".into());
                    return Err(Trap::Panic(s));
                }
                Op::LockObj => {
                    let o = r!(a);
                    if let Some(h) = o.header() {
                        if h.state() == ObjState::Shared {
                            save!();
                            if h.is_locked_by(tid) && h.lock_depth() >= MAX_WITH_LOCK_NESTING {
                                return Err(Trap::Panic(format!(
                                    "LockError: `with lock` nested more than {MAX_WITH_LOCK_NESTING} deep on one object"
                                )));
                            }
                            if !h.try_lock(tid) {
                                catallaxy_gc::enter_native(&self.m.rec);
                                h.lock(tid);
                                catallaxy_gc::leave_native(&self.m.rec);
                            }
                            let depth = frames!().len() - 1;
                            self.rs().held_locks.push((depth, o));
                        }
                    }
                }
                Op::UnlockObj => {
                    let o = r!(a);
                    if let Some(pos) = self.rs().held_locks.iter().rposition(|&(_, v)| v == o) {
                        self.rs().held_locks.remove(pos);
                        let h = unsafe { &*o.as_ptr() };
                        if h.is_locked_by(tid) {
                            h.unlock();
                        }
                    }
                }
                Op::Freeze => {
                    let x = r!(b_of(w));
                    save!();
                    tri!(ownership::freeze_deep(&self.m, x));
                    set!(a, x);
                }
                Op::Safepoint => {
                    if unsafe { (*poll).load(Ordering::Relaxed) } {
                        save!();
                        poll_slow!();
                    }
                }
            }
        }
    }
}

impl Interop for Vm {
    fn mutator(&mut self) -> &mut Mutator {
        &mut self.m
    }

    fn call_method(&mut self, recv: Value, name: Sym, args: &[Value]) -> RtResult<Option<Value>> {
        let cls = class_of(recv);
        let Some(fid) = cls.lookup(name) else { return Ok(None) };
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(recv);
        all.extend_from_slice(args);
        self.call_func(fid, &all).map(Some)
    }
}

/// Kept for symmetry with builtin class ids used by natives.
pub fn builtin_string_class() -> &'static catallaxy_runtime::Class {
    BUILTINS.string
}

#[allow(dead_code)]
fn is_builtin(id: u32) -> bool {
    id < ids::FIRST_USER
}
