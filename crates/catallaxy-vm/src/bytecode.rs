//! Bytecode and the program format the compiler hands to the VM.
//!
//! Instructions are 32-bit words in the style of Lua 5.x: an 8-bit opcode and up to three 8-bit
//! register operands (`A B C`), or `A` plus a 16-bit operand (`Bx`, or signed `sBx`). Operands
//! that may exceed 16 bits (function, class, native and global ids) follow the instruction as an
//! extra data word. Registers are frame-relative; a function has at most 255.

/// Opcodes. Comments give operands; `d` is a trailing data word.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Nop,
    /// R[A] = R[B]
    Move,
    /// R[A] = K[Bx]
    LoadK,
    /// R[A] = sBx (small integer)
    LoadInt,
    LoadNil,
    LoadTrue,
    LoadFalse,
    /// pc += sBx unless R[A] is the missing-argument marker
    JmpIfNotEmpty,
    /// R[A] = G[d]
    GetGlobal,
    /// G[d] = R[A]
    SetGlobal,
    /// R[A] = current closure capture B
    GetCap,
    /// R[A] = new cell holding R[B]
    NewCell,
    /// R[A] = cell(R[B]).value
    GetCell,
    /// cell(R[A]).value = R[B]
    SetCell,

    // Int arithmetic: R[A] = R[B] op R[C]
    AddI,
    SubI,
    MulI,
    DivI,
    ModI,
    PowI,
    /// R[A] = R[B] + sC (C is a signed 8-bit immediate)
    AddIK,
    /// R[A] = -R[B]
    NegI,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    /// R[A] = ~R[B]
    BNot,

    // Float arithmetic.
    AddF,
    SubF,
    MulF,
    DivF,
    ModF,
    PowF,
    NegF,
    /// R[A] = Float(R[B])
    IToF,

    // Comparisons producing Bool: R[A] = R[B] op R[C]
    EqI,
    LtI,
    LeI,
    EqF,
    LtF,
    LeF,
    /// Structural equality (any types).
    Eq,
    Ne,
    /// Generic ordering via `<=>` / builtin comparison.
    Lt,
    Le,
    /// R[A] = R[B] <=> R[C] (Int)
    Cmp,
    /// R[A] = !R[B]
    Not,
    /// R[A] = R[B] is an instance of class d (subclasses included; builtin ids for primitives)
    Is,
    /// R[A] = R[B] has every method of interface d
    IsIface,
    /// R[A] = R[B] == nil
    IsNil,

    // Generic arithmetic for Dyn, unions and erased generics.
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Neg,

    /// R[A] = concatenation of strings R[B .. B+C]
    Concat,
    /// R[A] = R[B].to_s (Display)
    ToS,

    /// pc += sAx (A and Bx together form a signed 24-bit offset)
    Jmp,
    /// if R[A] { pc += sBx }
    JmpIf,
    JmpIfNot,
    JmpIfNil,
    JmpIfNotNil,
    /// return R[A]
    Return,
    ReturnNil,

    /// R[A] = func d(R[A .. A+B])
    Call,
    /// R[A] = native d(R[A .. A+B])
    CallNative,
    /// R[A] = closure R[A](R[A+1 .. A+1+B])
    CallClosure,
    /// R[A] = R[A].method(R[A+1 ..]) with B args including self; d = inline cache index
    CallMethod,
    /// Like CallMethod through vtable slot d.
    CallVirtual,

    /// R[A] = new instance of class d
    New,
    /// R[A] = R[B].field[C]
    GetField,
    /// R[A].field[B] = R[C]
    SetField,

    /// R[A] = [R[B] .. R[B+C]]
    NewList,
    /// R[A] = [R[B]: R[B+1], ...] with C pairs
    NewDict,
    /// R[A] = (R[B], ..., R[B+C-1])
    NewTuple,
    /// R[A] = R[B]..R[C]
    NewRange,
    /// R[A] = R[B]...R[C]
    NewRangeEx,
    /// R[A] = R[B][R[C]] (list, Int index)
    ListGet,
    /// R[A][R[B]] = R[C]
    ListSet,
    /// R[A] = R[B].length (list)
    ListLen,
    /// R[A].push(R[B])
    ListPush,
    /// R[A] = R[B][R[C]] (generic: list, dict, string, tuple, Dyn)
    Index,
    /// R[A][R[B]] = R[C] (generic)
    SetIndex,
    /// R[A] = tuple R[B] element C
    TupleGet,
    /// R[A] = closure over func d, captures per the function's capture list
    Closure,
    /// R[A] = variant tag of enum value R[B]
    EnumTag,
    /// R[A] = R[B], panicking if nil
    Unwrap,
    /// R[A] = R[B] if it conforms to runtime type d, else nil
    AsCast,
    /// R[A] = an id for the current frame (target of non-local exits)
    FrameId,
    /// Non-local exit: A = kind (0 break, 1 return), frame id in R[B], value in R[C]
    Unwind,
    /// panic with message R[A]
    Panic,
    /// lock shared object R[A] (reentrant)
    LockObj,
    UnlockObj,
    /// R[A] = R[B] with the static type `frozen T`: deep-freezes the object graph
    Freeze,
    /// Safepoint poll (emitted at loop heads that have no backward jump).
    Safepoint,
    /// R[A] = (R[B] is a heap object of kind C) — fast path for builtin containers
    IsKind,
    /// R[A] = the missing-argument marker (callee applies its default)
    LoadEmpty,
    /// R[A] = R[B].name on a `Dyn` value: record key, field or zero-argument method; d = inline
    /// cache index (caches the record's shape and slot)
    DynGet,

    // Superinstructions: a typed comparison fused with the conditional jump that consumes it.
    // The forward offset, counted from the instruction after the data word, is in C when it is
    // below 0x80; otherwise C is 0xff and the offset is the data word.
    /// if !(R[A] < R[B]) (Int) { pc += d }
    JNLtI,
    /// if !(R[A] <= R[B]) (Int) { pc += d }
    JNLeI,
    /// if !(R[A] < R[B]) (Float) { pc += d }
    JNLtF,
    /// if !(R[A] <= R[B]) (Float) { pc += d }
    JNLeF,
    /// if R[A] != R[B] (Int) { pc += d }
    JNEqI,
    /// if R[A] == R[B] (Int) { pc += d }
    JEqI,
    // The same against a small constant: B is a signed 8-bit immediate K.
    /// if !(R[A] < K) { pc += d }
    JNLtIK,
    /// if !(R[A] <= K) { pc += d }
    JNLeIK,
    /// if !(R[A] > K) { pc += d }
    JNGtIK,
    /// if !(R[A] >= K) { pc += d }
    JNGeIK,
    /// if R[A] != K { pc += d }
    JNEqIK,
    /// if R[A] == K { pc += d }
    JEqIK,
    /// R[A] = R[B].field[d & 0xffff]; R[A+1] = R[C].field[d >> 16] (two `GetField`s, fused)
    GetField2,
    /// R[A] = R[B] formatted per the spec packed in two data words (`fmtspec::FormatSpec::encode`)
    Format,
}

/// `IsKind` operand for tuples (`catallaxy_runtime::ObjKind::Tuple as u8`, checked at load).
pub const KIND_TUPLE: u8 = 7;

pub const NUM_OPS: usize = Op::Format as usize + 1;

impl Op {
    pub fn from_u8(b: u8) -> Op {
        assert!((b as usize) < NUM_OPS, "bad opcode {b}");
        // SAFETY: Op is repr(u8) with contiguous discriminants checked above.
        unsafe { std::mem::transmute::<u8, Op>(b) }
    }

    /// How many data words follow the instruction.
    pub fn data_len(self) -> usize {
        if self == Op::Format {
            2
        } else {
            self.has_data() as usize
        }
    }

    /// Whether the instruction is followed by one data word.
    pub fn has_data(self) -> bool {
        matches!(
            self,
            Op::GetGlobal
                | Op::SetGlobal
                | Op::Call
                | Op::CallNative
                | Op::CallMethod
                | Op::DynGet
                | Op::JNLtI
                | Op::JNLeI
                | Op::JNLtF
                | Op::JNLeF
                | Op::JNEqI
                | Op::JEqI
                | Op::JNLtIK
                | Op::JNLeIK
                | Op::JNGtIK
                | Op::JNGeIK
                | Op::JNEqIK
                | Op::JEqIK
                | Op::GetField2
                | Op::CallVirtual
                | Op::New
                | Op::Closure
                | Op::Is
                | Op::IsIface
                | Op::AsCast
        )
    }
}

#[inline(always)]
pub fn encode(op: Op, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}
#[inline(always)]
pub fn encode_bx(op: Op, a: u8, bx: u16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((bx as u32) << 16)
}
#[inline(always)]
pub fn encode_sbx(op: Op, a: u8, sbx: i16) -> u32 {
    encode_bx(op, a, sbx as u16)
}
#[inline(always)]
pub fn encode_sax(op: Op, sax: i32) -> u32 {
    debug_assert!((-(1 << 23)..(1 << 23)).contains(&sax));
    (op as u32) | (((sax as u32) & 0xff_ffff) << 8)
}

#[inline(always)]
pub fn op_of(w: u32) -> u8 {
    w as u8
}
#[inline(always)]
pub fn a_of(w: u32) -> usize {
    ((w >> 8) & 0xff) as usize
}
#[inline(always)]
pub fn b_of(w: u32) -> usize {
    ((w >> 16) & 0xff) as usize
}
#[inline(always)]
pub fn c_of(w: u32) -> usize {
    (w >> 24) as usize
}
#[inline(always)]
pub fn bx_of(w: u32) -> usize {
    (w >> 16) as usize
}
#[inline(always)]
pub fn sbx_of(w: u32) -> isize {
    (w >> 16) as u16 as i16 as isize
}
#[inline(always)]
pub fn sc_of(w: u32) -> i64 {
    (w >> 24) as u8 as i8 as i64
}
#[inline(always)]
pub fn sax_of(w: u32) -> isize {
    ((w as i32) >> 8) as isize
}

/// A compile-time constant.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i64),
    BigInt(String),
    Float(f64),
    Str(String),
}

/// Where a closure capture comes from when the closure is created.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CapSrc {
    /// A register of the creating frame.
    Reg(u8),
    /// A capture of the creating closure.
    Cap(u8),
}

#[derive(Clone, Debug)]
pub struct IcProto {
    pub name: String,
    /// Default interface method to use when the receiver's class lacks `name`.
    pub fallback: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct FuncProto {
    pub name: String,
    pub file: u32,
    pub nparams: u8,
    pub nregs: u8,
    pub code: Vec<u32>,
    /// (pc, line) pairs, sorted by pc.
    pub lines: Vec<(u32, u32)>,
    pub consts: Vec<Const>,
    pub ics: Vec<IcProto>,
    pub captures: Vec<CapSrc>,
    /// Native implementation name, for `native fn` declarations (code is empty).
    pub native: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClassCap {
    Owned,
    Frozen,
    Shared,
}

#[derive(Clone, Debug)]
pub struct ClassProto {
    pub id: u32,
    pub name: String,
    pub fields: Vec<String>,
    pub superclass: Option<u32>,
    /// All methods visible on instances (inherited, own and extension), by name.
    pub methods: Vec<(String, u32)>,
    pub vtable: Vec<u32>,
    pub capability: ClassCap,
    /// For enum variants: (enum id, variant tag).
    pub variant: Option<(u32, u32)>,
}

/// Runtime type descriptions for `as?` checks.
#[derive(Clone, Debug, PartialEq)]
pub enum RtType {
    Any,
    Int,
    Float,
    Bool,
    Str,
    Nil,
    /// Instance of class (or subclass), or builtin class id.
    Class(u32),
    /// Any variant of the enum with this id.
    Enum(u32),
    List(Box<RtType>),
    Dict(Box<RtType>, Box<RtType>),
    Set(Box<RtType>),
    Tuple(Vec<RtType>),
    Optional(Box<RtType>),
    Union(Vec<RtType>),
    Fn,
    /// Interface: the value's class is one of those listed in `Program::ifaces`.
    Iface(u32),
}

#[derive(Clone, Debug)]
pub struct GlobalProto {
    pub name: String,
    /// Module constants are frozen on assignment.
    pub is_const: bool,
}

/// A compiled program. Ids are global and stable across incremental (REPL) compilations: a later
/// program extends an earlier one, and the VM loads only what it has not seen.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub funcs: Vec<FuncProto>,
    pub classes: Vec<ClassProto>,
    /// Methods callable by name on builtin classes: (class id, name, func id).
    pub builtin_methods: Vec<(u32, String, u32)>,
    pub natives: Vec<String>,
    pub globals: Vec<GlobalProto>,
    pub rt_types: Vec<RtType>,
    /// Interfaces for runtime checks: for each, the sorted ids of the classes whose methods
    /// satisfy it (the checker decides, with the rules of a static conversion).
    pub ifaces: Vec<Vec<u32>>,
    pub files: Vec<String>,
    /// Functions to run, in order, the last being the entry module's main body.
    pub entry: Vec<u32>,
}

/// Human-readable disassembly (for `catx disasm` and debugging).
pub fn disassemble(p: &Program, f: &FuncProto) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "fn {} (params {}, regs {})", f.name, f.nparams, f.nregs);
    let mut pc = 0;
    while pc < f.code.len() {
        let w = f.code[pc];
        let op = Op::from_u8(op_of(w));
        let _ = write!(out, "  {pc:4}  {op:?}");
        match op {
            Op::LoadK => {
                let _ = write!(out, " r{} {:?}", a_of(w), f.consts.get(bx_of(w)));
            }
            Op::LoadInt => {
                let _ = write!(out, " r{} {}", a_of(w), sbx_of(w));
            }
            Op::Jmp => {
                let _ = write!(out, " -> {}", pc as isize + 1 + sax_of(w));
            }
            Op::JmpIf | Op::JmpIfNot | Op::JmpIfNil | Op::JmpIfNotNil | Op::JmpIfNotEmpty => {
                let _ = write!(out, " r{} -> {}", a_of(w), pc as isize + 1 + sbx_of(w));
            }
            Op::AddIK => {
                let _ = write!(out, " r{} r{} {}", a_of(w), b_of(w), sc_of(w));
            }
            _ => {
                let _ = write!(out, " {} {} {}", a_of(w), b_of(w), c_of(w));
            }
        }
        if op == Op::Format {
            let spec = catallaxy_runtime::fmtspec::FormatSpec::decode([f.code[pc + 1], f.code[pc + 2]]);
            let _ = write!(out, "  ; {spec:?}");
            pc += 2;
        } else if op.has_data() {
            pc += 1;
            let d = f.code[pc];
            match op {
                Op::Call | Op::Closure => {
                    let _ = write!(out, "  ; {}", p.funcs.get(d as usize).map(|f| f.name.as_str()).unwrap_or("?"));
                }
                Op::CallNative => {
                    let _ = write!(out, "  ; native {}", p.natives.get(d as usize).map(|s| s.as_str()).unwrap_or("?"));
                }
                Op::JNLtI
                | Op::JNLeI
                | Op::JNLtF
                | Op::JNLeF
                | Op::JNEqI
                | Op::JEqI
                | Op::JNLtIK
                | Op::JNLeIK
                | Op::JNGtIK
                | Op::JNGeIK
                | Op::JNEqIK
                | Op::JEqIK => {
                    let c = c_of(f.code[pc - 1]);
                    let off = if c < 0x80 { c as isize } else { d as i32 as isize };
                    let _ = write!(out, "  ; -> {}", pc as isize + 1 + off);
                }
                Op::GetField2 => {
                    let _ = write!(out, "  ; .{} .{}", d & 0xffff, d >> 16);
                }
                Op::CallMethod | Op::DynGet => {
                    let _ = write!(out, "  ; .{}", f.ics.get(d as usize).map(|i| i.name.as_str()).unwrap_or("?"));
                }
                _ => {
                    let _ = write!(out, "  ; {d}");
                }
            }
        }
        out.push('\n');
        pc += 1;
    }
    out
}
