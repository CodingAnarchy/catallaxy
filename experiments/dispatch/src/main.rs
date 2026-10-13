//! Issue #6: does tail-call threaded dispatch beat the `match`-in-a-loop interpreter?
//!
//! A miniature of the Catallaxy VM (same 32-bit encoding, NaN-boxed-style small ints as plain
//! i64 here, a register file, calls with frames) runs two programs with two dispatch styles:
//!
//! - `switch`: one `match` in a loop, the structure of `catallaxy_vm::interp` (one shared
//!   indirect branch);
//! - `threaded`: one function per opcode; each ends by calling the next handler through a table
//!   (a tail call LLVM turns into a jump in release builds; `become` would guarantee it).
//!
//! `cargo run --release -- <switch|threaded> <loop|fib>` prints the result; time it externally.

use std::hint::black_box;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Op {
    LoadInt,
    Move,
    AddI,
    AddIK,
    ModI,
    JNLtI,
    Jmp,
    Call,
    Return,
    Halt,
}
const NOPS: usize = 10;

fn enc(op: Op, a: u8, b: u8, c: u8) -> u32 {
    op as u32 | (a as u32) << 8 | (b as u32) << 16 | (c as u32) << 24
}
fn enc_sbx(op: Op, a: u8, sbx: i16) -> u32 {
    op as u32 | (a as u32) << 8 | ((sbx as u16) as u32) << 16
}
#[inline(always)]
fn a_of(w: u32) -> usize {
    ((w >> 8) & 0xff) as usize
}
#[inline(always)]
fn b_of(w: u32) -> usize {
    ((w >> 16) & 0xff) as usize
}
#[inline(always)]
fn c_of(w: u32) -> usize {
    (w >> 24) as usize
}
#[inline(always)]
fn sbx_of(w: u32) -> isize {
    (w >> 16) as u16 as i16 as isize
}
#[inline(always)]
fn sc_of(w: u32) -> i64 {
    (w >> 24) as u8 as i8 as i64
}

/// `total = 0; i = 0; while i < n { total += i % 7; i += 1 }`, as Catallaxy 0.2 compiles it.
fn loop_prog(n: i16) -> Vec<u32> {
    vec![
        enc_sbx(Op::LoadInt, 0, 0),
        enc_sbx(Op::LoadInt, 1, 0),
        enc_sbx(Op::LoadInt, 3, n), // 2: loop head
        enc(Op::JNLtI, 1, 3, 5),    // -> 10 (pc after data word + 5)
        0,
        enc_sbx(Op::LoadInt, 4, 7),
        enc(Op::ModI, 5, 1, 4),
        enc(Op::AddI, 0, 0, 5),
        enc(Op::AddIK, 1, 1, 1),
        enc_sbx(Op::Jmp, 0, -8), // -> 2
        enc(Op::Halt, 0, 0, 0),  // 10
    ]
}

/// `fn fib(n) { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }`, as Catallaxy 0.2 compiles it.
/// Frame layout: r0 = n, r1 = result, r2/r3 call slots. Entry at 0; main at 13.
fn fib_prog(n: i16) -> Vec<u32> {
    vec![
        enc_sbx(Op::LoadInt, 2, 2),  // 0
        enc(Op::JNLtI, 0, 2, 2),     // 1 -> 5
        0,
        enc(Op::Move, 1, 0, 0),      // 3
        enc_sbx(Op::Jmp, 0, 7),      // 4 -> 12
        enc(Op::AddIK, 2, 0, 0xff),  // 5  r2 = n - 1
        enc(Op::Call, 2, 0, 0),      // 6  r2 = fib(r2)
        enc(Op::AddIK, 3, 0, 0xfe),  // 7  r3 = n - 2
        enc(Op::Call, 3, 0, 0),      // 8
        enc(Op::AddI, 1, 2, 3),      // 9
        enc(Op::Return, 1, 0, 0),    // 10 (unused slot 11 below)
        enc(Op::Halt, 0, 0, 0),      // 11
        enc(Op::Return, 1, 0, 0),    // 12
        enc_sbx(Op::LoadInt, 4, n),  // 13 main
        enc(Op::Call, 4, 0, 0),      // 14
        enc(Op::Halt, 4, 0, 0),      // 15
    ]
}

const FRAME: usize = 8;

struct Frame {
    ret_pc: usize,
    base: usize,
}

// ---- switch dispatch ------------------------------------------------------------------------

fn run_switch(code: &[u32], entry: usize, regs: &mut [i64]) -> i64 {
    let mut pc = entry;
    let mut base = 0usize;
    let mut frames: Vec<Frame> = Vec::new();
    let code = code.as_ptr();
    let r = regs.as_mut_ptr();
    loop {
        let w = unsafe { *code.add(pc) };
        pc += 1;
        let a = base + a_of(w);
        unsafe {
            match std::mem::transmute::<u8, Op>(w as u8) {
                Op::LoadInt => *r.add(a) = sbx_of(w) as i64,
                Op::Move => *r.add(a) = *r.add(base + b_of(w)),
                Op::AddI => *r.add(a) = (*r.add(base + b_of(w))).wrapping_add(*r.add(base + c_of(w))),
                Op::AddIK => *r.add(a) = (*r.add(base + b_of(w))).wrapping_add(sc_of(w)),
                Op::ModI => *r.add(a) = (*r.add(base + b_of(w))).rem_euclid(*r.add(base + c_of(w))),
                Op::JNLtI => {
                    if *r.add(a) >= *r.add(base + b_of(w)) {
                        pc += 1 + c_of(w);
                    } else {
                        pc += 1;
                    }
                }
                Op::Jmp => pc = (pc as isize + sbx_of(w)) as usize,
                Op::Call => {
                    frames.push(Frame { ret_pc: pc, base });
                    base = a; // callee's r0 is the caller's call slot
                    pc = 0;
                }
                Op::Return => {
                    let v = *r.add(a);
                    let f = frames.pop().unwrap();
                    *r.add(base) = v; // result into the call slot
                    base = f.base;
                    pc = f.ret_pc;
                }
                Op::Halt => return *r.add(a),
            }
        }
    }
}

// ---- tail-call threaded dispatch ------------------------------------------------------------

struct St {
    code: *const u32,
    r: *mut i64,
    frames: Vec<Frame>,
    result: i64,
}

type Handler = fn(&mut St, usize, usize);

static TABLE: [Handler; NOPS] = [h_loadint, h_move, h_addi, h_addik, h_modi, h_jnlti, h_jmp, h_call, h_return, h_halt];

#[inline(always)]
fn next(st: &mut St, pc: usize, base: usize) {
    let w = unsafe { *st.code.add(pc) };
    TABLE[w as u8 as usize](st, pc, base)
}
#[inline(always)]
fn word(st: &St, pc: usize) -> u32 {
    unsafe { *st.code.add(pc) }
}

fn h_loadint(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    unsafe { *st.r.add(base + a_of(w)) = sbx_of(w) as i64 };
    next(st, pc + 1, base)
}
fn h_move(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    unsafe { *st.r.add(base + a_of(w)) = *st.r.add(base + b_of(w)) };
    next(st, pc + 1, base)
}
fn h_addi(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    unsafe { *st.r.add(base + a_of(w)) = (*st.r.add(base + b_of(w))).wrapping_add(*st.r.add(base + c_of(w))) };
    next(st, pc + 1, base)
}
fn h_addik(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    unsafe { *st.r.add(base + a_of(w)) = (*st.r.add(base + b_of(w))).wrapping_add(sc_of(w)) };
    next(st, pc + 1, base)
}
fn h_modi(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    unsafe { *st.r.add(base + a_of(w)) = (*st.r.add(base + b_of(w))).rem_euclid(*st.r.add(base + c_of(w))) };
    next(st, pc + 1, base)
}
fn h_jnlti(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    let jump = unsafe { *st.r.add(base + a_of(w)) >= *st.r.add(base + b_of(w)) };
    next(st, if jump { pc + 2 + c_of(w) } else { pc + 2 }, base)
}
fn h_jmp(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    next(st, (pc as isize + 1 + sbx_of(w)) as usize, base)
}
fn h_call(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    st.frames.push(Frame { ret_pc: pc + 1, base });
    next(st, 0, base + a_of(w))
}
fn h_return(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    let v = unsafe { *st.r.add(base + a_of(w)) };
    let f = st.frames.pop().unwrap();
    unsafe { *st.r.add(base) = v };
    next(st, f.ret_pc, f.base)
}
fn h_halt(st: &mut St, pc: usize, base: usize) {
    let w = word(st, pc);
    st.result = unsafe { *st.r.add(base + a_of(w)) };
}

fn run_threaded(code: &[u32], entry: usize, regs: &mut [i64]) -> i64 {
    let mut st = St { code: code.as_ptr(), r: regs.as_mut_ptr(), frames: Vec::new(), result: 0 };
    next(&mut st, entry, 0);
    st.result
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (style, prog) = (args.get(1).map(String::as_str).unwrap_or("switch"), args.get(2).map(String::as_str).unwrap_or("loop"));
    let (code, entry, reps) = match prog {
        "loop" => (loop_prog(30000), 0, 700),
        _ => (fib_prog(32), 13, 1),
    };
    let mut regs = vec![0i64; FRAME * 64];
    let mut sum = 0i64;
    for _ in 0..reps {
        sum = sum.wrapping_add(match style {
            "switch" => run_switch(black_box(&code), entry, &mut regs),
            _ => run_threaded(black_box(&code), entry, &mut regs),
        });
    }
    println!("{sum}");
}
