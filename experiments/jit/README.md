# JIT experiment (issue #7)

Question: is a baseline JIT worth building for Catallaxy, and what would it buy?

`src/jit.rs` is a small method JIT built on Cranelift 0.136. It translates real Catallaxy 0.1 bytecode,
compiled from `.cx` source by `catallaxy-compiler` and loaded by `catallaxy-vm`, into machine code.
`src/main.rs` is a harness that runs a program like `catx run` does. It JIT-compiles the entry functions
that fit the subset, plus their callees, and leaves everything else to the interpreter. Both run in one
process on the real runtime (`Vm`, heap, stdlib natives, GC). The experiment is a standalone package and
not a workspace member. It needs no changes under `crates/`.

```sh
cargo build --release                                            # catx, for the baseline
cargo build --release --manifest-path experiments/jit/Cargo.toml
experiments/jit/target/release/jit-experiment jit run bench/fib.cx
experiments/jit/target/release/jit-experiment interp call bench/fib.cx fib 35
python3 experiments/jit/measure.py                               # the tables below
```

Flags: `--poll call|load|none` picks the safepoint poll style, `--gc-stress` forces a collection from a
second thread every millisecond, and `-v` lists the functions that were not JIT-able and why.

## What the JIT supports

- **Opcodes:** `Nop Move LoadInt LoadK`(Int constant)` LoadNil LoadTrue LoadFalse`, `AddI SubI MulI DivI
  ModI AddIK NegI`, `EqI LtI LeI`, `Jmp JmpIf JmpIfNot`, the fused `JNLtI JNLeI JNEqI JEqI`,
  `Return ReturnNil Safepoint`, and `CallNative`.
- **Calls:** `Call` to a native, or to another JIT-able function with exactly `nparams` arguments
  (no defaults, no closures).
- **Register kinds:** a whole-program fixpoint assigns each register a kind. Registers whose every
  definition is an Int op hold an unboxed `i64` in a Cranelift variable. All other registers hold a
  NaN-boxed `Value`. Parameters are optimistically unboxed, and callers convert at the call. A function
  whose every `Return` returns an Int register returns an unboxed `i64`, so `fib` passes and returns
  raw integers.
- **No deopt (bail-out only):** results stay in the 48-bit small-int range. These cases leave the fast
  path:
  - an add, sub or mul that overflows that range (checked with a shift pair, plus `smulhi` for multiply);
  - `/` or `%` with a negative dividend or a non-positive divisor (the interpreter's floor semantics);
  - a boxed operand that is not a small int;
  - a native that returns a heap object.

  In any of these cases the function returns a sentinel (`i64::MIN` for unboxed returns, `u64::MAX` for
  boxed ones), and every JIT caller propagates it. The harness then reruns that entry in the interpreter.
  The rerun is correct for pure functions. It would repeat side effects that ran before the bail-out,
  which a production JIT must not do (see below). `cases/bailout.cx` exercises each bail-out, for
  example `call cases/bailout.cx grow 40` gives `12157665459056928801` (a BigInt) in both modes.
- **Natives:** JIT code calls natives through a trampoline (`jit_call_native`). The trampoline gets the
  `Vm`, the `NativeFn` pointer and the boxed arguments, which sit in a stack slot. Natives can allocate
  and run the collector, which is fine because of the next rule.
- **GC safety without stack maps:** JIT frames never hold a heap pointer. Int registers are raw integers.
  Boxed registers can only hold nil, booleans, small ints, or native results that are checked not to be
  pointers. The collector's root scan walks only the interpreter's `Value` stack and stays exact.
- **Safepoints:** JIT code polls at every backward `Jmp`, at `Safepoint`, and at function entry, matching
  the interpreter's polls at back-edges and calls. On a request it calls `catallaxy_gc::safepoint_slow`.
  - `--poll call` (the default) calls the existing public `catallaxy_gc::is_gc_requested()` through an
    imported symbol.
  - `--poll load` is an inline one-byte load from a flag address. It stands in for exposing the GC
    flag's address, which would need a small accessor in `catallaxy-gc`, so this harness polls a local
    flag instead.
  - `--poll none` removes the polls, to measure what they cost.

Coverage of `bench/`: `fib.cx` and `loop.cx` JIT entirely. In `parallel.cx` and `parmap.cx` the hot
`work` function fits, but it is reached through `spawn`/`par_map` closures, and this harness has no
interpreter-to-JIT entry. It therefore measures `work` with `call`. `nbody.cx` (floats and fields),
`trees.cx` (`New`, `GetField`), `words.cx` (dicts and strings) and `alloc_par.cx` are outside the subset.
`cases/collatz.cx` adds a branchier Int kernel with nested calls, `if`/`else`, `/` and `%`.

## Results (October 2026)

Apple M-series (arm64, macOS 26.5), median of 7 runs:

- `wall`, `instructions` and `cycles` cover the whole process (from `time -l`), including compiling the
  prelude and program.
- `exec` is measured in process. It covers the entry calls only and leaves out bytecode compilation and
  JIT compilation.
- `compile` is the JIT's total time: analysis, Cranelift codegen at `opt_level=speed`, and finalization.
- `catx` is the real binary. `interp` is the same interpreter driven by the harness, and it matches
  `catx`.

Every configuration printed identical output.

| Program | Config | wall ms | exec ms | instructions | cycles | compile | exec speedup |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fib.cx (`fib(32)`) | catx | 76.3 | | 2,088M | 312M | | |
| | interp | 76.4 | 69.1 | 2,085M | 314M | | 1.0x |
| | jit, poll=call | 18.1 | 10.5 | 352M | 66M | 0.41 ms | **6.6x** |
| | jit, poll=load | 17.6 | 10.4 | 316M | 65M | 0.38 ms | 6.6x |
| | jit, poll=none | 17.7 | 10.4 | 270M | 65M | 0.36 ms | 6.6x |
| loop.cx (20M iterations) | catx | 105.6 | | 3,796M | 438M | | |
| | interp | 102.7 | 95.4 | 3,793M | 425M | | 1.0x |
| | jit, poll=call | 31.0 | 23.6 | 816M | 121M | 0.34 ms | **4.0x** |
| | jit, poll=load | 23.2 | 15.6 | 716M | 87M | 0.35 ms | 6.1x |
| | jit, poll=none | 20.9 | 13.3 | 595M | 77M | 0.33 ms | 7.2x |
| cases/collatz.cx | catx | 475.2 | | 13,263M | 2,017M | | |
| | interp | 467.5 | 460.1 | 13,260M | 1,999M | | 1.0x |
| | jit, poll=call | 70.5 | 62.9 | 1,257M | 294M | 0.67 ms | **7.3x** |
| | jit, poll=load | 55.7 | 48.2 | 1,004M | 229M | 0.68 ms | 9.5x |
| | jit, poll=none | 53.1 | 45.6 | 710M | 222M | 0.60 ms | 10.1x |
| `fib(35)` via `call` | interp | 296.8 | 289.6 | 8,598M | 1,263M | | 1.0x |
| | jit, poll=call | 51.3 | 43.9 | 1,240M | 209M | 0.30 ms | **6.6x** |
| `work(1)` from parallel.cx (2M iterations) | interp | 28.6 | 22.1 | 761M | 114M | | 1.0x |
| | jit, poll=call | 12.9 | 6.2 | 199M | 46M | 0.34 ms | **3.6x** |

Per-function compile time is 0.1 to 0.3 ms for functions of 6 to 25 bytecode words, which produce 200 to
480 bytes of arm64 code. The first function compiled in a process pays about 0.1 ms of one-time
Cranelift setup on top of that. For example, `steps` in collatz.cx (24 words) takes 167 µs and `fib`
(13 words) takes 113 to 213 µs. Compiling is cheap next to any loop worth compiling: one `fib(32)` run
takes about 25x the compile time of both functions.

`work` is bounded by two dependent `%` (hardware divides) per iteration, so it gains least. Most of
`fib`'s remaining time is the call itself: frame setup, spilling the unboxed argument, and the
sentinel check after each return.

**Safepoints work and cost little when inline.** `--gc-stress` (a collection forced from another
thread every millisecond) shows the effect. With `poll=call` or `poll=load`, collections complete with
a median of 13 µs and a maximum of 45 µs while JIT code runs. With `poll=none` one collection waits
the whole run: 42 ms for `fib(35)` and 12 ms for `loop.cx`. The interpreter's own figures are a
median of 11 µs and a maximum of 138 µs. Calling `is_gc_requested()` out of line costs up to 1.5x in
tight loops (loop.cx 23.6 ms with the call against 15.6 ms with a load). A production JIT would need
`catallaxy-gc` to expose the flag's address so it can poll with one inline load.

## What a production JIT would additionally need

This experiment avoids every hard problem by keeping heap values out of JIT frames. The functions
that matter in practice need all of the following:

1. **Stack maps / GC root maps.** Code that touches objects (fields, lists, strings, closures) holds
   heap pointers in machine registers and spill slots across calls and safepoints. Cranelift can emit
   stack maps (`declare_var_needs_stack_map`), and the collector's root scan would have to walk JIT
   frames as well as the interpreter's `Value` stack. The heap is non-moving, which helps: maps only
   need to find roots, not update them.
2. **Deoptimization.** Bailing out at function granularity breaks once code has side effects.
   Overflow into BigInt, a failed type guard, or a stale inline cache needs mid-function transfer to
   the interpreter: materialize the interpreter frame from JIT state at a known pc. That means OSR
   metadata per guard, plus OSR entry so hot loops in long-running frames (module bodies) get compiled
   at all.
3. **Interpreter ↔ JIT transitions.** Hotness counters at `Call` and back-edges, calls from the
   interpreter into JIT code (closures, methods, `par_map` workers), calls from JIT code back into
   interpreted functions, and unwinding (`Unwind`, panics, backtraces with line numbers) through mixed
   frames.
4. **Inline caches and method dispatch.** `CallMethod`, `CallVirtual` and `DynGet` go through the VM's
   per-site inline caches. The JIT would read and guard on them, and needs a code-invalidation story
   for when classes are extended at load time or in the REPL.
5. **Ownership checks for shared objects.** Every field and container access on a `shared` object
   checks the owner and lock state (`Header::is_local_or_frozen`, `LockObj`, promotion on access). These
   checks are the language's memory-safety boundary under races. The JIT would have to reproduce them
   exactly, inline, and keep the memory orderings of the Rust implementation.
6. **Threads and `par_map`.** JIT code is shared by all mutator threads. Compilation must be safe
   against concurrent installation (publish entry points atomically) and against collection, and
   each thread's JIT frames must be scannable while that thread is stopped. Every one of these paths
   is a new source of races that ThreadSanitizer cannot see inside generated code.
7. **Float and boxing support.** nbody-style code needs unboxed `f64` registers, `IToF`, float
   compares and the fused float branches. Mixed Int/Float code needs representation changes at calls.
8. **Stack limits.** The interpreter's frame stack is bounded and reports stack overflow as a
   Catallaxy panic. JIT recursion runs on the native stack (here 256 MB) and would need explicit
   checks.
9. **Testing.** Differential fuzzing of JIT against interpreter (the repo already has a `fuzz/`
   setup), bail-out coverage tests, and x86-64 as well as arm64 CI. Miri cannot execute generated
   code, so the guarantees that currently come from running the runtime under Miri do not extend to it.

## Decision

**No-go for now.** Do not build a production JIT in the current performance phase. Keep this
experiment as the reference for when the question comes back.

The speedups are real: 4 to 10x on Int-only kernels after 0.3 to 0.7 ms of compile time, with exact
results and working safepoints. Three reasons outweigh them:

- **The subset that is cheap to JIT is not where the benchmarks spend their time.** Of the eight
  `bench/` programs, only `fib` and `loop` are Int-only. `parallel`/`parmap` have an Int kernel, but it
  is reachable only through closures and `par_map`, which needs the interpreter-to-JIT entry path.
  `nbody`, `trees`, `words` and `alloc_par` are dominated by fields, allocation, floats, dicts and
  strings. Getting real speedups there means building items 1 to 6 above. That is a second
  execution engine, larger than the interpreter, and it is where the bugs live.
- **Reliability and the unsafe boundary.** The design puts `unsafe` in a few crates so it can be
  fuzzed and run under Miri and ThreadSanitizer, and the runtime's promise is that a racy program
  never corrupts the heap. A JIT moves the most delicate invariants (root maps, ownership and lock
  checks, safepoint placement) into generated machine code. No Rust tool checks that code, and a
  miscompile there is a memory-safety bug, not a wrong answer. The README states that all `unsafe`
  lives in `catallaxy-gc` and `catallaxy-runtime`. In practice `catallaxy-vm`'s interpreter loop and
  `catallaxy-stdlib` already contain `unsafe` blocks, and a JIT would widen that further.
- **Cost.** Cranelift roughly doubles clean release build time for anything that links it. On this
  machine a clean release build of the harness takes 41 s, against 21 s for `catx`. It also adds
  about 1.1 MB of `__TEXT`. The harness binary has 3.7 MB of `__TEXT` against 2.6 MB for `catx`, even
  though the harness leaves out the REPL, LSP and formatter. That would be a heavy dependency for an
  embeddable language.

The interpreter still has cheaper headroom. This experiment shows where the time goes: the JIT's
gain on `loop.cx` comes from removing dispatch and tag checks, not from better arithmetic. Unboxed
Int and Float register kinds, more superinstructions (#5), cheaper call frames (fib is call-bound),
and an inline safepoint flag in the interpreter are each a fraction of the work. None of them
touches the safety story.

**Revisit when** profiles of real programs (not microbenchmarks) show time dominated by Int/Float
kernels in hot functions, after the interpreter work above. A sensible first production step is
the same no-heap-values design extended to floats, with a hotness counter at `Call` and an
interpreter-to-JIT entry. That keeps the GC root scan unchanged and needs only one small
`catallaxy-gc` accessor for the poll flag. Stack maps, deopt and inline caches should wait until
that tier has paid for itself.
