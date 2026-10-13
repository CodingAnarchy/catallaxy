//! Baseline method JIT for the Int subset of Catallaxy bytecode, on Cranelift.
//!
//! Supported: `Nop Move LoadInt LoadK(Int) LoadNil LoadTrue LoadFalse AddI SubI MulI DivI ModI
//! AddIK NegI EqI LtI LeI Jmp JmpIf JmpIfNot JNLtI JNLeI JNEqI JEqI Return ReturnNil Safepoint`,
//! `Call` to a native or to another JIT-able function with exactly `nparams` arguments, and
//! `CallNative`.
//!
//! Registers become Cranelift variables. Each register gets a kind by a fixpoint over the whole
//! program: `Int` registers hold an unboxed i64 (always within the 48-bit small-int range), all
//! others hold a NaN-boxed `Value`. A function returns an unboxed Int when every `Return` returns
//! an Int register.
//!
//! There is no deoptimization. Anything the fast path does not handle (a non-small-int operand, a
//! result leaving the small-int range, a negative or zero divisor, a native returning a heap
//! object) makes the function return a sentinel, which every JIT caller propagates. The harness
//! then reruns the entry in the interpreter. Because JIT frames never hold a heap pointer, the
//! collector's root scan (which only walks the interpreter stack) stays correct without stack maps.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use catallaxy_runtime::{Trap, Value};
use catallaxy_vm::bytecode::*;
use catallaxy_vm::program::{self, Func};
use catallaxy_vm::{NativeFn, Vm};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I64, I8};
use cranelift_codegen::ir::{
    AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, UserFuncName,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};

/// Returned by an Int-returning function that bailed out (outside the 48-bit small-int range).
pub const SENT_INT: i64 = i64::MIN;
/// Returned by a Value-returning function that bailed out (a non-canonical NaN, never a Value).
pub const SENT_BOXED: i64 = -1;

const TAG_INT: i64 = 0xFFFA << 48;
const PAYLOAD_MASK: i64 = (1 << 48) - 1;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Poll {
    /// Call `catallaxy_gc::is_gc_requested()` through an imported symbol.
    Call,
    /// Inline load of a flag's address (a harness-local stand-in for exposing the GC flag).
    Load,
    /// No polls (to measure their cost; not GC-safe with other threads).
    None,
}

/// Stand-in for `catallaxy_gc`'s private request flag, for `Poll::Load`.
pub static LOCAL_GC_FLAG: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// A trap raised by a native called from JIT code.
    pub static NATIVE_TRAP: RefCell<Option<Trap>> = const { RefCell::new(None) };
}

extern "C" fn jit_gc_requested() -> i8 {
    catallaxy_gc::is_gc_requested() as i8
}

extern "C" fn jit_safepoint_slow(vm: *mut Vm) {
    // SAFETY: the harness passes the running thread's Vm, which outlives the JIT call.
    let vm = unsafe { &mut *vm };
    catallaxy_gc::safepoint_slow(&vm.m.rec);
}

extern "C" fn jit_call_native(vm: *mut Vm, nf: usize, args: *const u64, nargs: usize) -> u64 {
    // SAFETY: `nf` was produced from a `NativeFn` by `Jit::compile`; `args` points at `nargs`
    // boxed values in the caller's stack slot; `vm` is the running thread's Vm.
    let (nf, args, vm) = unsafe {
        (std::mem::transmute::<usize, NativeFn>(nf), std::slice::from_raw_parts(args as *const Value, nargs), &mut *vm)
    };
    match nf(vm, args) {
        // A heap result would sit in a JIT frame the collector cannot see: bail.
        Ok(v) if v.is_ptr() => SENT_BOXED as u64,
        Ok(v) => v.0,
        Err(t) => {
            NATIVE_TRAP.with(|c| *c.borrow_mut() = Some(t));
            SENT_BOXED as u64
        }
    }
}

#[derive(Clone, Copy)]
struct Inst {
    pc: usize,
    w: u32,
    data: u32,
}

impl Inst {
    fn op(&self) -> Op {
        Op::from_u8(op_of(self.w))
    }
    /// The next instruction's pc.
    fn next(&self) -> usize {
        self.pc + 1 + self.op().has_data() as usize
    }
    fn target(&self) -> Option<usize> {
        let w = self.w;
        match self.op() {
            Op::Jmp => Some((self.next() as isize + sax_of(w)) as usize),
            Op::JmpIf | Op::JmpIfNot => Some((self.next() as isize + sbx_of(w)) as usize),
            Op::JNLtI | Op::JNLeI | Op::JNEqI | Op::JEqI => {
                let c = c_of(w);
                let off = if c < 0x80 { c as isize } else { self.data as i32 as isize };
                Some((self.next() as isize + off) as usize)
            }
            _ => None,
        }
    }
    /// Register written, if any.
    fn def(&self) -> Option<usize> {
        match self.op() {
            Op::Move
            | Op::LoadK
            | Op::LoadInt
            | Op::LoadNil
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::AddI
            | Op::SubI
            | Op::MulI
            | Op::DivI
            | Op::ModI
            | Op::AddIK
            | Op::NegI
            | Op::EqI
            | Op::LtI
            | Op::LeI
            | Op::Call
            | Op::CallNative => Some(a_of(self.w)),
            _ => None,
        }
    }
}

enum Callee {
    Native(NativeFn),
    Jit(u32),
}

pub struct FnInfo {
    pub fid: u32,
    pub name: String,
    func: &'static Func,
    insts: Vec<Inst>,
    reg_int: Vec<bool>,
    ret_int: bool,
    callees: Vec<u32>,
}

/// Decodes `fid` and checks it against the subset. Callees are checked later.
fn scan(fid: u32) -> Result<FnInfo, String> {
    let func = program::func(fid);
    if func.native.is_some() {
        return Err("native".into());
    }
    if !func.captures.is_empty() {
        return Err("closure".into());
    }
    let code = &func.code;
    let mut insts = Vec::new();
    let mut callees = Vec::new();
    let mut pc = 0;
    while pc < code.len() {
        let w = code[pc];
        let op = Op::from_u8(op_of(w));
        let data = if op.has_data() { code[pc + 1] } else { 0 };
        let i = Inst { pc, w, data };
        match op {
            Op::Nop
            | Op::Move
            | Op::LoadInt
            | Op::LoadNil
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::AddI
            | Op::SubI
            | Op::MulI
            | Op::DivI
            | Op::ModI
            | Op::AddIK
            | Op::NegI
            | Op::EqI
            | Op::LtI
            | Op::LeI
            | Op::Jmp
            | Op::JmpIf
            | Op::JmpIfNot
            | Op::JNLtI
            | Op::JNLeI
            | Op::JNEqI
            | Op::JEqI
            | Op::Return
            | Op::ReturnNil
            | Op::Safepoint
            | Op::CallNative => {}
            Op::LoadK => {
                if !func.consts[bx_of(w)].is_small_int() {
                    return Err(format!("pc {pc}: non-Int constant"));
                }
            }
            Op::Call => {
                let callee = program::func(data);
                if callee.native.is_none() {
                    if callee.nparams != b_of(w) {
                        return Err(format!("pc {pc}: call to {} with default arguments", callee.name));
                    }
                    callees.push(data);
                }
            }
            op => return Err(format!("pc {pc}: unsupported {op:?}")),
        }
        insts.push(i);
        pc = i.next();
    }
    Ok(FnInfo { fid, name: func.name.clone(), func, insts, reg_int: vec![true; func.nregs], ret_int: true, callees })
}

/// The result of analysis: eligible functions with register kinds, and why others are not.
pub struct Analysis {
    pub fns: HashMap<u32, FnInfo>,
    pub rejected: Vec<(u32, String)>,
}

/// Analyses `roots` and everything they call.
pub fn analyse(roots: &[u32]) -> Analysis {
    let mut fns: HashMap<u32, FnInfo> = HashMap::new();
    let mut rejected: HashMap<u32, String> = HashMap::new();
    let mut work: Vec<u32> = roots.to_vec();
    while let Some(fid) = work.pop() {
        if fns.contains_key(&fid) || rejected.contains_key(&fid) {
            continue;
        }
        match scan(fid) {
            Ok(info) => {
                work.extend(info.callees.iter().copied());
                fns.insert(fid, info);
            }
            Err(e) => {
                rejected.insert(fid, e);
            }
        }
    }
    // Drop functions that call rejected ones, to a fixpoint.
    loop {
        let bad: Vec<(u32, u32)> =
            fns.values().filter_map(|f| f.callees.iter().find(|c| !fns.contains_key(c)).map(|c| (f.fid, *c))).collect();
        if bad.is_empty() {
            break;
        }
        for (fid, c) in bad {
            fns.remove(&fid);
            rejected.insert(fid, format!("calls non-JIT-able {}", program::func(c).name));
        }
    }
    // Register and return kinds, to a global fixpoint (optimistically Int, demoted to boxed).
    loop {
        let ret_int: HashMap<u32, bool> = fns.iter().map(|(k, f)| (*k, f.ret_int)).collect();
        let mut changed = false;
        for f in fns.values_mut() {
            loop {
                let mut local = false;
                for i in &f.insts {
                    let Some(a) = i.def() else { continue };
                    let int = match i.op() {
                        Op::Move => f.reg_int[b_of(i.w)],
                        Op::LoadNil | Op::LoadTrue | Op::LoadFalse | Op::EqI | Op::LtI | Op::LeI | Op::CallNative => {
                            false
                        }
                        Op::Call => ret_int.get(&i.data).copied().unwrap_or(false),
                        _ => true,
                    };
                    if !int && f.reg_int[a] {
                        f.reg_int[a] = false;
                        local = true;
                    }
                }
                if !local {
                    break;
                }
                changed = true;
            }
            let ri = f.insts.iter().all(|i| match i.op() {
                Op::Return => f.reg_int[a_of(i.w)],
                Op::ReturnNil => false,
                _ => true,
            });
            if ri != f.ret_int {
                f.ret_int = ri;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut rejected: Vec<(u32, String)> = rejected.into_iter().collect();
    rejected.sort();
    Analysis { fns, rejected }
}

pub struct Compiled {
    pub ptr: *const u8,
    pub nparams: usize,
    pub param_int: Vec<bool>,
    pub ret_int: bool,
    pub compile_time: Duration,
    pub code_size: usize,
}

pub struct Jit {
    /// Owns the code memory (kept for the life of the process: the harness runs one program).
    _module: JITModule,
    pub compiled: HashMap<u32, Compiled>,
    pub finalize_time: Duration,
}

impl Jit {
    /// Compiles every function in `an`.
    pub fn compile(an: &Analysis, poll: Poll) -> Jit {
        let mut jb = JITBuilder::with_flags(&[("opt_level", "speed")], default_libcall_names()).expect("host ISA");
        jb.symbol("jit_gc_requested", jit_gc_requested as *const u8);
        jb.symbol("jit_safepoint_slow", jit_safepoint_slow as *const u8);
        jb.symbol("jit_call_native", jit_call_native as *const u8);
        let mut module = JITModule::new(jb);
        let fe = module.target_config();
        let ptr_ty = fe.pointer_type();
        assert_eq!(ptr_ty, I64);

        let mut s_req = module.make_signature();
        s_req.returns.push(AbiParam::new(I8));
        let f_req = module.declare_function("jit_gc_requested", Linkage::Import, &s_req).unwrap();
        let mut s_slow = module.make_signature();
        s_slow.params.push(AbiParam::new(I64));
        let f_slow = module.declare_function("jit_safepoint_slow", Linkage::Import, &s_slow).unwrap();
        let mut s_nat = module.make_signature();
        for _ in 0..4 {
            s_nat.params.push(AbiParam::new(I64));
        }
        s_nat.returns.push(AbiParam::new(I64));
        let f_nat = module.declare_function("jit_call_native", Linkage::Import, &s_nat).unwrap();

        let mut ids: HashMap<u32, FuncId> = HashMap::new();
        let mut order: Vec<&FnInfo> = an.fns.values().collect();
        order.sort_by_key(|f| f.fid);
        for f in &order {
            let mut sig = module.make_signature();
            for _ in 0..=f.func.nparams {
                sig.params.push(AbiParam::new(I64));
            }
            sig.returns.push(AbiParam::new(I64));
            let id = module.declare_function(&format!("cx{}_{}", f.fid, f.name), Linkage::Local, &sig).unwrap();
            ids.insert(f.fid, id);
        }

        let mut ctx = module.make_context();
        let mut fbc = FunctionBuilderContext::new();
        let mut times = HashMap::new();
        for f in &order {
            let t0 = Instant::now();
            ctx.func.signature = module.declarations().get_function_decl(ids[&f.fid]).signature.clone();
            ctx.func.name = UserFuncName::user(0, f.fid);
            {
                let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
                let refs = Refs {
                    req: module.declare_func_in_func(f_req, b.func),
                    slow: module.declare_func_in_func(f_slow, b.func),
                    nat: module.declare_func_in_func(f_nat, b.func),
                    jit: ids.iter().map(|(fid, id)| (*fid, module.declare_func_in_func(*id, b.func))).collect(),
                };
                translate(&mut b, f, an, &refs, poll);
                b.seal_all_blocks();
                b.finalize(fe);
            }
            if std::env::var_os("JIT_DUMP_IR").is_some() {
                eprintln!("{}", ctx.func.display());
            }
            module.define_function(ids[&f.fid], &mut ctx).unwrap_or_else(|e| panic!("{}: {e:?}", f.name));
            let size = ctx.compiled_code().map(|c| c.code_buffer().len()).unwrap_or(0);
            module.clear_context(&mut ctx);
            times.insert(f.fid, (t0.elapsed(), size));
        }
        let t0 = Instant::now();
        module.finalize_definitions().unwrap();
        let finalize_time = t0.elapsed();
        let compiled = order
            .iter()
            .map(|f| {
                let (compile_time, code_size) = times[&f.fid];
                (
                    f.fid,
                    Compiled {
                        ptr: module.get_finalized_function(ids[&f.fid]),
                        nparams: f.func.nparams,
                        param_int: f.reg_int[..f.func.nparams].to_vec(),
                        ret_int: f.ret_int,
                        compile_time,
                        code_size,
                    },
                )
            })
            .collect();
        Jit { _module: module, compiled, finalize_time }
    }

    /// Calls a compiled function. `args` are i64s or boxed values per `param_int`. Returns the
    /// result as a Value, or None if the function bailed out.
    pub fn call(&self, fid: u32, vm: &mut Vm, args: &[Value]) -> Option<Value> {
        let c = &self.compiled[&fid];
        assert_eq!(args.len(), c.nparams);
        let mut raw = Vec::with_capacity(args.len());
        for (a, &int) in args.iter().zip(&c.param_int) {
            raw.push(if int {
                if !a.is_small_int() {
                    return None;
                }
                a.as_small_int()
            } else {
                a.0 as i64
            });
        }
        let vmp = vm as *mut Vm as i64;
        // SAFETY: `ptr` is a finalized function with `1 + nparams` i64 parameters and an i64
        // result in the host's default calling convention (the C ABI for these types).
        let r = unsafe {
            match raw.len() {
                0 => std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(c.ptr)(vmp),
                1 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64) -> i64>(c.ptr)(vmp, raw[0]),
                2 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64) -> i64>(c.ptr)(vmp, raw[0], raw[1]),
                3 => std::mem::transmute::<*const u8, extern "C" fn(i64, i64, i64, i64) -> i64>(c.ptr)(
                    vmp, raw[0], raw[1], raw[2],
                ),
                n => panic!("harness supports up to 3 parameters, not {n}"),
            }
        };
        if c.ret_int {
            (r != SENT_INT).then(|| Value::small_int(r))
        } else {
            (r != SENT_BOXED).then_some(Value(r as u64))
        }
    }
}

struct Refs {
    req: FuncRef,
    slow: FuncRef,
    nat: FuncRef,
    jit: HashMap<u32, FuncRef>,
}

struct Tx<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    vars: Vec<Variable>,
    int: Vec<bool>,
    bail: Block,
    vm: cranelift_codegen::ir::Value,
}

impl Tx<'_, '_> {
    fn box_int(&mut self, i: cranelift_codegen::ir::Value) -> cranelift_codegen::ir::Value {
        let p = self.b.ins().band_imm_s(i, PAYLOAD_MASK);
        self.b.ins().bor_imm_s(p, TAG_INT)
    }
    /// Continue only if `ok` is nonzero; otherwise bail.
    fn guard(&mut self, ok: cranelift_codegen::ir::Value) {
        let cont = self.b.create_block();
        self.b.ins().brif(ok, cont, &[], self.bail, &[]);
        self.b.switch_to_block(cont);
    }
    fn get_int(&mut self, r: usize) -> cranelift_codegen::ir::Value {
        let v = self.b.use_var(self.vars[r]);
        if self.int[r] {
            return v;
        }
        let tag = self.b.ins().ushr_imm_s(v, 48);
        let ok = self.b.ins().icmp_imm_s(IntCC::Equal, tag, 0xFFFA);
        self.guard(ok);
        let s = self.b.ins().ishl_imm_s(v, 16);
        self.b.ins().sshr_imm_s(s, 16)
    }
    fn get_boxed(&mut self, r: usize) -> cranelift_codegen::ir::Value {
        let v = self.b.use_var(self.vars[r]);
        if self.int[r] {
            self.box_int(v)
        } else {
            v
        }
    }
    /// Stores an unboxed Int already known to be in range.
    fn set_int(&mut self, r: usize, i: cranelift_codegen::ir::Value) {
        let v = if self.int[r] { i } else { self.box_int(i) };
        self.b.def_var(self.vars[r], v);
    }
    fn set_boxed(&mut self, r: usize, v: cranelift_codegen::ir::Value) {
        assert!(!self.int[r], "boxed def of an Int register");
        self.b.def_var(self.vars[r], v);
    }
    /// Bails unless `i` fits in 48 bits.
    fn check_small(&mut self, i: cranelift_codegen::ir::Value) {
        let s = self.b.ins().ishl_imm_s(i, 16);
        let s = self.b.ins().sshr_imm_s(s, 16);
        let ok = self.b.ins().icmp(IntCC::Equal, s, i);
        self.guard(ok);
    }
    fn poll(&mut self, poll: Poll, refs: &Refs) {
        let req = match poll {
            Poll::None => return,
            Poll::Call => {
                let call = self.b.ins().call(refs.req, &[]);
                self.b.inst_results(call)[0]
            }
            Poll::Load => {
                let addr = self.b.ins().iconst(I64, LOCAL_GC_FLAG.as_ptr() as i64);
                self.b.ins().load(I8, MemFlagsData::trusted(), addr, 0)
            }
        };
        let slow = self.b.create_block();
        let cont = self.b.create_block();
        self.b.set_cold_block(slow);
        self.b.ins().brif(req, slow, &[], cont, &[]);
        self.b.switch_to_block(slow);
        let vm = self.vm;
        self.b.ins().call(refs.slow, &[vm]);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
    }
}

fn translate(b: &mut FunctionBuilder, f: &FnInfo, an: &Analysis, refs: &Refs, poll: Poll) {
    let func = f.func;
    // Block boundaries: entry, branch targets and every instruction after a branch.
    let mut starts = BTreeSet::new();
    for i in &f.insts {
        if let Some(t) = i.target() {
            starts.insert(t);
            starts.insert(i.next());
        }
        if matches!(i.op(), Op::Return | Op::ReturnNil) {
            starts.insert(i.next());
        }
    }
    let blocks: HashMap<usize, Block> = starts.iter().map(|&pc| (pc, b.create_block())).collect();

    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let params = b.block_params(entry).to_vec();
    let bail = b.create_block();
    b.set_cold_block(bail);
    let vars: Vec<Variable> = (0..func.nregs).map(|_| b.declare_var(I64)).collect();
    let mut tx = Tx { b, vars, int: f.reg_int.clone(), bail, vm: params[0] };
    for r in 0..func.nregs {
        let v = if r < func.nparams {
            params[r + 1]
        } else if tx.int[r] {
            tx.b.ins().iconst(I64, 0)
        } else {
            tx.b.ins().iconst(I64, Value::NIL.0 as i64)
        };
        tx.b.def_var(tx.vars[r], v);
    }
    tx.poll(poll, refs);

    let mut terminated = false;
    for i in &f.insts {
        if let Some(&blk) = blocks.get(&i.pc) {
            if !terminated {
                tx.b.ins().jump(blk, &[]);
            }
            tx.b.switch_to_block(blk);
            terminated = false;
        }
        let w = i.w;
        let a = a_of(w);
        match i.op() {
            Op::Nop => {}
            Op::Move => {
                if tx.int[a] {
                    let v = tx.get_int(b_of(w));
                    tx.set_int(a, v);
                } else {
                    let v = tx.get_boxed(b_of(w));
                    tx.set_boxed(a, v);
                }
            }
            Op::LoadInt => {
                let v = tx.b.ins().iconst(I64, sbx_of(w) as i64);
                tx.set_int(a, v);
            }
            Op::LoadK => {
                let v = tx.b.ins().iconst(I64, func.consts[bx_of(w)].as_small_int());
                tx.set_int(a, v);
            }
            Op::LoadNil | Op::LoadTrue | Op::LoadFalse => {
                let k = match i.op() {
                    Op::LoadNil => Value::NIL,
                    Op::LoadTrue => Value::TRUE,
                    _ => Value::FALSE,
                };
                let v = tx.b.ins().iconst(I64, k.0 as i64);
                tx.set_boxed(a, v);
            }
            Op::AddI | Op::SubI => {
                let x = tx.get_int(b_of(w));
                let y = tx.get_int(c_of(w));
                let s = if i.op() == Op::AddI { tx.b.ins().iadd(x, y) } else { tx.b.ins().isub(x, y) };
                tx.check_small(s);
                tx.set_int(a, s);
            }
            Op::AddIK => {
                let x = tx.get_int(b_of(w));
                let s = tx.b.ins().iadd_imm_s(x, sc_of(w));
                tx.check_small(s);
                tx.set_int(a, s);
            }
            Op::NegI => {
                let x = tx.get_int(b_of(w));
                let s = tx.b.ins().ineg(x);
                tx.check_small(s);
                tx.set_int(a, s);
            }
            Op::MulI => {
                let x = tx.get_int(b_of(w));
                let y = tx.get_int(c_of(w));
                let lo = tx.b.ins().imul(x, y);
                let hi = tx.b.ins().smulhi(x, y);
                let sign = tx.b.ins().sshr_imm_s(lo, 63);
                let ok = tx.b.ins().icmp(IntCC::Equal, hi, sign);
                tx.guard(ok);
                tx.check_small(lo);
                tx.set_int(a, lo);
            }
            Op::DivI | Op::ModI => {
                // Fast path as in the interpreter's ModI: non-negative dividend, positive
                // divisor (floor and truncating semantics agree). Everything else bails.
                let x = tx.get_int(b_of(w));
                let y = tx.get_int(c_of(w));
                let xok = tx.b.ins().icmp_imm_s(IntCC::SignedGreaterThanOrEqual, x, 0);
                let yok = tx.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, y, 0);
                let ok = tx.b.ins().band(xok, yok);
                tx.guard(ok);
                let r = if i.op() == Op::DivI { tx.b.ins().udiv(x, y) } else { tx.b.ins().urem(x, y) };
                tx.set_int(a, r);
            }
            Op::EqI | Op::LtI | Op::LeI => {
                let x = tx.get_int(b_of(w));
                let y = tx.get_int(c_of(w));
                let cc = match i.op() {
                    Op::EqI => IntCC::Equal,
                    Op::LtI => IntCC::SignedLessThan,
                    _ => IntCC::SignedLessThanOrEqual,
                };
                let c = tx.b.ins().icmp(cc, x, y);
                let t = tx.b.ins().iconst(I64, Value::TRUE.0 as i64);
                let fl = tx.b.ins().iconst(I64, Value::FALSE.0 as i64);
                let v = tx.b.ins().select(c, t, fl);
                tx.set_boxed(a, v);
            }
            Op::Jmp => {
                let t = i.target().unwrap();
                if t <= i.pc {
                    tx.poll(poll, refs);
                }
                tx.b.ins().jump(blocks[&t], &[]);
                terminated = true;
            }
            Op::JmpIf | Op::JmpIfNot => {
                let v = tx.get_boxed(a);
                let c = tx.b.ins().icmp_imm_s(IntCC::Equal, v, Value::TRUE.0 as i64);
                let (t, n) = (blocks[&i.target().unwrap()], blocks[&i.next()]);
                if i.op() == Op::JmpIf {
                    tx.b.ins().brif(c, t, &[], n, &[]);
                } else {
                    tx.b.ins().brif(c, n, &[], t, &[]);
                }
                terminated = true;
            }
            Op::JNLtI | Op::JNLeI | Op::JNEqI | Op::JEqI => {
                let x = tx.get_int(a);
                let y = tx.get_int(b_of(w));
                let cc = match i.op() {
                    Op::JNLtI => IntCC::SignedGreaterThanOrEqual,
                    Op::JNLeI => IntCC::SignedGreaterThan,
                    Op::JNEqI => IntCC::NotEqual,
                    _ => IntCC::Equal,
                };
                let c = tx.b.ins().icmp(cc, x, y);
                tx.b.ins().brif(c, blocks[&i.target().unwrap()], &[], blocks[&i.next()], &[]);
                terminated = true;
            }
            Op::Return | Op::ReturnNil => {
                let v = if i.op() == Op::ReturnNil {
                    tx.b.ins().iconst(I64, Value::NIL.0 as i64)
                } else if f.ret_int {
                    tx.get_int(a)
                } else {
                    tx.get_boxed(a)
                };
                tx.b.ins().return_(&[v]);
                terminated = true;
            }
            Op::Safepoint => tx.poll(poll, refs),
            Op::Call | Op::CallNative => {
                let nargs = b_of(w);
                let native =
                    if i.op() == Op::CallNative { Some(program::native(i.data)) } else { program::func(i.data).native };
                match native.map(Callee::Native).unwrap_or(Callee::Jit(i.data)) {
                    Callee::Native(nf) => {
                        let slot = tx.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            (nargs.max(1) * 8) as u32,
                            3,
                        ));
                        for k in 0..nargs {
                            let v = tx.get_boxed(a + k);
                            tx.b.ins().stack_store(I64, v, slot, (k * 8) as i32);
                        }
                        let p = tx.b.ins().stack_addr(I64, slot, 0);
                        let nfv = tx.b.ins().iconst(I64, nf as usize as i64);
                        let n = tx.b.ins().iconst(I64, nargs as i64);
                        let vm = tx.vm;
                        let call = tx.b.ins().call(refs.nat, &[vm, nfv, p, n]);
                        let r = tx.b.inst_results(call)[0];
                        let ok = tx.b.ins().icmp_imm_s(IntCC::NotEqual, r, SENT_BOXED);
                        tx.guard(ok);
                        tx.set_boxed(a, r);
                    }
                    Callee::Jit(fid) => {
                        let callee = &an.fns[&fid];
                        let mut args = vec![tx.vm];
                        for k in 0..nargs {
                            let v = if callee.reg_int[k] { tx.get_int(a + k) } else { tx.get_boxed(a + k) };
                            args.push(v);
                        }
                        let call = tx.b.ins().call(refs.jit[&fid], &args);
                        let r = tx.b.inst_results(call)[0];
                        let sent = if callee.ret_int { SENT_INT } else { SENT_BOXED };
                        let ok = tx.b.ins().icmp_imm_s(IntCC::NotEqual, r, sent);
                        tx.guard(ok);
                        if callee.ret_int {
                            tx.set_int(a, r);
                        } else {
                            tx.set_boxed(a, r);
                        }
                    }
                }
            }
            op => unreachable!("{op:?} passed the scan"),
        }
    }
    if !terminated {
        // Falling off the end cannot happen in compiler output; treat it as a bail.
        tx.b.ins().jump(tx.bail, &[]);
    }
    tx.b.switch_to_block(tx.bail);
    let sent = if f.ret_int { SENT_INT } else { SENT_BOXED };
    let s = tx.b.ins().iconst(I64, sent);
    tx.b.ins().return_(&[s]);
}
