//! The register-based interpreter.
//!
//! One [`Vm`] runs per OS thread. Frames live in a fixed-size register stack that never moves, so
//! raw pointers into it stay valid across nested calls (natives receive their arguments as a slice
//! of the caller's registers). Calls between bytecode functions do not recurse on the Rust stack;
//! only natives calling back into bytecode do.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use catallaxy_gc::{Header, ObjState, RootSource, ThreadRecord, Tracer};
use catallaxy_runtime::class::{class_of, heap_class, ids, ObjKind, BUILTINS};
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
    stack: *mut Value,
    stack_cap: usize,
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
    pub(crate) frames: Vec<Frame>,
    /// Objects locked by `with lock`, with the frame depth that locked them.
    held_locks: Vec<(usize, Value)>,
    /// Extra values rooted by the runtime (thread-pool jobs and the like).
    extra_roots: Vec<Value>,
    /// Roots of the thread's fiber VMs (main VM only).
    children: Vec<VmRoots>,
    /// Values held for the thread's fibers, by fiber id (results awaiting `join`).
    pinned: Vec<(u64, Value)>,
}

// SAFETY: a Vm is used by one thread at a time; it is created by a parent thread and handed to
// the child before the child starts running code.
unsafe impl Send for Vm {}

#[derive(Clone, Copy)]
struct VmRoots {
    stack: *const Value,
    rs: *const RootState,
    handles: *const Vec<Value>,
}

impl RootSource for VmRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        // SAFETY: called only while the owning thread is stopped; all three pointers are owned
        // allocations that the thread itself also reaches only through raw pointers.
        let rs = unsafe { &*self.rs };
        let top = rs.frames.last().map(|f| f.top).unwrap_or(0);
        for i in 0..top {
            ops_mark(t, unsafe { *self.stack.add(i) });
        }
        for f in &rs.frames {
            ops_mark(t, f.closure);
        }
        for &v in unsafe { &*self.handles } {
            ops_mark(t, v);
        }
        for &v in &rs.extra_roots {
            ops_mark(t, v);
        }
        for &(_, v) in &rs.held_locks {
            ops_mark(t, v);
        }
        for child in &rs.children {
            child.trace_roots(t);
        }
        for &(_, v) in &rs.pinned {
            ops_mark(t, v);
        }
    }
}

#[inline(always)]
fn ops_mark(t: &mut Tracer, v: Value) {
    if v.is_ptr() {
        t.mark(v.as_ptr());
    }
}

fn alloc_stack(n: usize) -> *mut Value {
    let layout = std::alloc::Layout::array::<Value>(n).unwrap();
    // Zeroed memory is lazily committed by the OS; zero bits decode as the float 0.0, which the
    // collector ignores.
    let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut Value;
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    p
}

pub const MAIN_STACK: usize = 4 << 20;
pub const THREAD_STACK: usize = 1 << 20;

impl Vm {
    /// Creates a VM for a thread registered with the collector. The thread record must be in the
    /// native state; the VM's roots are installed before this returns.
    pub fn new(rec: Arc<ThreadRecord>, stack_size: usize) -> Box<Vm> {
        let m = Mutator::new(rec);
        let rs = Box::into_raw(Box::new(RootState {
            frames: Vec::with_capacity(256),
            held_locks: Vec::new(),
            extra_roots: Vec::new(),
            children: Vec::new(),
            pinned: Vec::new(),
        }));
        let vm = Box::new(Vm {
            m,
            stack: alloc_stack(stack_size),
            stack_cap: stack_size,
            rs,
            next_serial: 1,
            panic_trace: None,
            root_rs: rs,
            no_park: 0,
        });
        let roots: Box<dyn RootSource> = Box::new(VmRoots { stack: vm.stack, rs: vm.rs, handles: vm.m.handles_ptr() });
        let roots_ptr = Box::into_raw(roots);
        let heap_ptr = vm.m.heap_ptr();
        unsafe { catallaxy_gc::install(&vm.m.rec, roots_ptr, heap_ptr) };
        // The root source box is leaked deliberately; it is tiny and lives as long as the thread.
        vm
    }

    /// Creates a VM for a fiber of this VM's thread: it shares the thread's heap and collector
    /// record, and its roots are scanned as part of the thread's. Release it with
    /// [`Vm::release_fiber`] on the same thread.
    pub fn new_fiber(&mut self, stack_size: usize) -> Box<Vm> {
        let rs = Box::into_raw(Box::new(RootState {
            frames: Vec::with_capacity(16),
            held_locks: Vec::new(),
            extra_roots: Vec::new(),
            children: Vec::new(),
            pinned: Vec::new(),
        }));
        let vm = Box::new(Vm {
            m: Mutator::sharing(&self.m),
            stack: alloc_stack(stack_size),
            stack_cap: stack_size,
            rs,
            next_serial: 1,
            panic_trace: None,
            root_rs: self.root_rs,
            no_park: 0,
        });
        let roots = VmRoots { stack: vm.stack, rs: vm.rs, handles: vm.m.handles_ptr() };
        // SAFETY: the root state belongs to this thread, which is running (not being scanned).
        unsafe { (*self.root_rs).children.push(roots) };
        vm
    }

    /// Unregisters a fiber VM from its thread's roots and frees it. It takes the box
    /// `new_fiber` returned: the VM's address was handed to the fiber's coroutine.
    #[allow(clippy::boxed_local)]
    pub fn release_fiber(mut vm: Box<Vm>) {
        unsafe {
            (*vm.root_rs).children.retain(|c| !std::ptr::eq(c.rs, vm.rs));
            let layout = std::alloc::Layout::array::<Value>(vm.stack_cap).unwrap();
            std::alloc::dealloc(vm.stack as *mut u8, layout);
            drop(Box::from_raw(vm.rs));
            vm.m.free_handles();
        }
    }

    /// Keeps `v` alive under `key` in the thread's main VM until [`Vm::unpin`].
    pub fn pin(&mut self, key: u64, v: Value) {
        unsafe { (*self.root_rs).pinned.push((key, v)) };
    }

    /// Removes and returns the value pinned under `key`.
    pub fn unpin(&mut self, key: u64) -> Option<Value> {
        let pinned = unsafe { &mut (*self.root_rs).pinned };
        let i = pinned.iter().position(|&(k, _)| k == key)?;
        Some(pinned.swap_remove(i).1)
    }

    /// Whether a fiber running on this VM may park: not while it holds `with lock` object
    /// locks or a `Mutex`, which are keyed by OS thread and so would let sibling fibers in.
    pub fn park_allowed(&self) -> bool {
        self.no_park == 0 && self.rs().held_locks.is_empty()
    }

    /// Registers the current thread and creates its VM, leaving it in the running state.
    pub fn for_current_thread(stack_size: usize) -> Box<Vm> {
        let rec = catallaxy_gc::register_thread();
        let vm = Vm::new(rec, stack_size);
        catallaxy_gc::leave_native(&vm.m.rec);
        vm
    }

    /// Unregisters the thread. The VM must not be used afterwards.
    pub fn shutdown(&mut self) {
        catallaxy_gc::enter_native(&self.m.rec);
        // Unregister first so no collection can touch the heap while it is being released.
        catallaxy_gc::unregister_thread(&self.m.rec);
        self.m.release_pages();
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
        if let Some(native) = f.native {
            return native(self, args);
        }
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

    #[inline(always)]
    fn check_stack(&self, need: usize) -> RtResult<()> {
        if need >= self.stack_cap || self.rs().frames.len() >= MAX_FRAMES {
            return Err(Trap::Panic("SystemStackError: stack level too deep".into()));
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
        // Pushes a bytecode frame for `f` whose arguments are already at absolute slot `base`.
        macro_rules! enter {
            ($f:expr, $base:expr, $ret:expr, $closure:expr, $nargs:expr) => {{
                let f: &'static Func = $f;
                let base: usize = $base;
                if base + f.nregs >= self.stack_cap || frames!().len() >= MAX_FRAMES {
                    save!();
                    return Err(Trap::Panic("SystemStackError: stack level too deep".into()));
                }
                for i in ($nargs)..f.nparams {
                    unsafe { *self.stack.add(base + i) = Value::EMPTY };
                }
                for i in f.nparams.max($nargs)..f.nregs {
                    unsafe { *self.stack.add(base + i) = Value::NIL };
                }
                save!();
                let top = (base + f.nregs).max(fr.top);
                frames!().push(Frame { func: f, pc: 0, base, ret: $ret, closure: $closure, serial: 0, top });
                if unsafe { (*poll).load(Ordering::Relaxed) } {
                    catallaxy_gc::safepoint_slow(&self.m.rec);
                }
                load_frame!();
            }};
        }
        macro_rules! call_native {
            ($nf:expr, $a:expr, $nargs:expr) => {{
                save!();
                let args = unsafe { std::slice::from_raw_parts(regs.add($a), $nargs) };
                let v = tri!(($nf)(self, args));
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
                    let eq = if x.0 == y.0 && !x.is_float() { true } else { tri!(ops::values_eq(self, x, y)) };
                    let res = if Op::from_u8(op_of(w)) == Op::Eq { eq } else { !eq };
                    set!(a, Value::bool(res));
                }
                Op::Lt | Op::Le => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let o = tri!(ops::compare_or_panic(self, x, y));
                    let res = if Op::from_u8(op_of(w)) == Op::Lt { o.is_lt() } else { o.is_le() };
                    set!(a, Value::bool(res));
                }
                Op::Cmp => {
                    let (x, y) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let o = tri!(ops::compare_or_panic(self, x, y));
                    set!(a, Value::small_int(o as i64));
                }
                Op::Not => set!(a, Value::bool(!r!(b_of(w)).is_true())),
                Op::Is => {
                    let d = data!();
                    let x = r!(b_of(w));
                    save!();
                    let ok = tri!(slow::conforms(self, x, d));
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
                    let v = tri!(slow::generic_arith(self, Op::from_u8(op_of(w)), x, y));
                    set!(a, v);
                }
                Op::Neg => {
                    let x = r!(b_of(w));
                    save!();
                    let v = tri!(slow::generic_neg(self, x));
                    set!(a, v);
                }

                // ---- strings ----
                Op::Concat => {
                    let (b, n) = (b_of(w), c_of(w));
                    let mut s = String::new();
                    for i in 0..n {
                        match str_of(r!(b + i)) {
                            Some(x) => s.push_str(x),
                            None => {
                                save!();
                                return Err(Trap::Panic("internal error: Concat of a non-string".into()));
                            }
                        }
                    }
                    let v = self.m.alloc_str(&s);
                    set!(a, v);
                }
                Op::ToS => {
                    let x = r!(b_of(w));
                    if str_of(x).is_some() {
                        set!(a, x);
                    } else {
                        save!();
                        let s = tri!(ops::to_s(self, x));
                        let v = self.m.alloc_str(&s);
                        set!(a, v);
                    }
                }

                // ---- control ----
                Op::Jmp => {
                    let off = sax_of(w);
                    pc = (pc as isize + off) as usize;
                    if off < 0 && unsafe { (*poll).load(Ordering::Relaxed) } {
                        save!();
                        catallaxy_gc::safepoint_slow(&self.m.rec);
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
                Op::Return | Op::ReturnNil => {
                    let v = if Op::from_u8(op_of(w)) == Op::Return { r!(a) } else { Value::NIL };
                    let done = frames!().pop().unwrap();
                    if !self.rs().held_locks.is_empty() {
                        self.release_locks();
                    }
                    if frames!().len() == stop {
                        return Ok(v);
                    }
                    unsafe { *self.stack.add(done.ret) = v };
                    load_frame!();
                }

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
                        let v = tri!(nf(self, args));
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
                    if !recv.is_ptr() {
                        save!();
                        return Err(Trap::Panic("internal error: virtual call on a non-object".into()));
                    }
                    let cls = unsafe { heap_class(recv.as_ptr()) };
                    let f = program::func(cls.vtable[slot]);
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
                    if !o.is_ptr() {
                        save!();
                        return Err(slow::field_on_non_object(o));
                    }
                    let h = unsafe { &*o.as_ptr() };
                    let inst = unsafe { as_instance(o) };
                    debug_assert!(i < inst.nfields(), "field index out of range");
                    let v = if h.is_local_or_frozen(tid) {
                        inst.get(i)
                    } else {
                        save!();
                        tri!(check_read(&self.m, h));
                        inst.get_acquire(i)
                    };
                    set!(a, v);
                }
                Op::SetField => {
                    let o = r!(a);
                    let i = b_of(w);
                    let v = r!(c_of(w));
                    if !o.is_ptr() {
                        save!();
                        return Err(slow::field_on_non_object(o));
                    }
                    let h = unsafe { &*o.as_ptr() };
                    let inst = unsafe { as_instance(o) };
                    debug_assert!(i < inst.nfields(), "field index out of range");
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
                    let mut items = Vec::with_capacity(n.max(4));
                    for i in 0..n {
                        items.push(r!(b + i));
                    }
                    let v = self.m.alloc_list(items);
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
                    let h = unsafe { &*l.as_ptr() };
                    if h.is_local_or_frozen(tid) && i.is_small_int() {
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
                    let v = tri!(slow::list_get(self, l, i));
                    set!(a, v);
                }
                Op::ListSet => {
                    let (l, i, v) = (r!(a), r!(b_of(w)), r!(c_of(w)));
                    save!();
                    tri!(slow::list_set(self, l, i, v));
                }
                Op::ListLen => {
                    let l = r!(b_of(w));
                    let h = unsafe { &*l.as_ptr() };
                    let n = if h.is_local_or_frozen(tid) {
                        unsafe { as_list(l) }.len()
                    } else {
                        save!();
                        tri!(ownership::read_list(&self.m, l)).len()
                    };
                    set!(a, Value::small_int(n as i64));
                }
                Op::ListPush => {
                    let (l, v) = (r!(a), r!(b_of(w)));
                    save!();
                    tri!(slow::list_push(self, l, v));
                }
                Op::DynGet => {
                    let d = data!() as usize;
                    let recv = r!(b_of(w));
                    let ic = &fr.func.ics[d];
                    if recv.is_ptr() {
                        let h = unsafe { &*recv.as_ptr() };
                        if h.is_local_or_frozen(tid) && unsafe { heap_class(h).kind } == ObjKind::Dict {
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
                    let v = tri!(slow::dyn_get(self, recv, ic));
                    set!(a, v);
                }
                Op::Index => {
                    let (x, i) = (r!(b_of(w)), r!(c_of(w)));
                    save!();
                    let v = tri!(slow::index_get(self, x, i));
                    set!(a, v);
                }
                Op::SetIndex => {
                    let (x, i, v) = (r!(a), r!(b_of(w)), r!(c_of(w)));
                    save!();
                    tri!(slow::index_set(self, x, i, v));
                }
                Op::TupleGet => {
                    let t = r!(b_of(w));
                    let i = c_of(w);
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
                    let ok = tri!(slow::conforms(self, x, d));
                    set!(a, if ok { x } else { Value::NIL });
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
                        catallaxy_gc::safepoint_slow(&self.m.rec);
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
