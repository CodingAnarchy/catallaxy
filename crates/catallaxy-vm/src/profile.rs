//! Opcode profiling, compiled in with the `op-profile` feature: counts every executed opcode and
//! every adjacent pair, to pick superinstructions and judge dispatch costs.
//! `catx run` prints the report to stderr when built with
//! `cargo build --release --features catallaxy/op-profile`.

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bytecode::{Op, NUM_OPS};

static PAIRS: [AtomicU64; NUM_OPS * NUM_OPS] = [const { AtomicU64::new(0) }; NUM_OPS * NUM_OPS];

thread_local! {
    static PREV: Cell<usize> = const { Cell::new(0) };
}

#[inline(always)]
pub fn record(op: u8) {
    let op = op as usize;
    PREV.with(|p| {
        PAIRS[p.get() * NUM_OPS + op].fetch_add(1, Ordering::Relaxed);
        p.set(op);
    });
}

/// The most frequent opcodes and opcode pairs, as text.
pub fn report(top: usize) -> String {
    let counts: Vec<u64> = PAIRS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let mut singles = vec![0u64; NUM_OPS];
    for (i, &c) in counts.iter().enumerate() {
        singles[i % NUM_OPS] += c;
    }
    let total: u64 = singles.iter().sum::<u64>().max(1);
    let name = |i: usize| format!("{:?}", Op::from_u8(i as u8));
    let mut out = String::new();
    let _ = writeln!(out, "{total} instructions executed\n\nopcodes:");
    let mut ops: Vec<(usize, u64)> = singles.into_iter().enumerate().filter(|&(_, c)| c > 0).collect();
    ops.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
    for (i, c) in ops.into_iter().take(top) {
        let _ = writeln!(out, "  {:>6.2}%  {}", 100.0 * c as f64 / total as f64, name(i));
    }
    let _ = writeln!(out, "\npairs:");
    let mut pairs: Vec<(usize, u64)> = counts.into_iter().enumerate().filter(|&(_, c)| c > 0).collect();
    pairs.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
    for (i, c) in pairs.into_iter().take(top) {
        let _ = writeln!(out, "  {:>6.2}%  {} -> {}", 100.0 * c as f64 / total as f64, name(i / NUM_OPS), name(i % NUM_OPS));
    }
    out
}
