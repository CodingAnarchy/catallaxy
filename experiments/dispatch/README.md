# Dispatch experiment (issue #6)

Question: would tail-call threaded dispatch (one function per opcode, each jumping straight to the
next handler) beat the interpreter's `match`-in-a-loop?

`src/main.rs` is a miniature of `catallaxy_vm::interp` with the same instruction encoding, register
file and call frames, implemented both ways. It runs two programs compiled exactly as Catallaxy
0.2 compiles them: `loop` (the `bench/loop.cx` loop, 21M iterations) and `fib` (`fib(32)`, 7M
calls). `measure.py` reports medians of 7 runs; `.github/workflows/dispatch-experiment.yml` runs it
on a GitHub x86-64 runner.

## Results (October 2026)

Apple M-series (aarch64), hardware counters from `time -l`:

| Program | Dispatch | Instructions | Cycles |
| --- | --- | --- | --- |
| loop | match | 2,451M | 375M |
| loop | threaded | 2,787M | 416M (+11%) |
| fib | match | 769M | 148M |
| fib | threaded | 938M | 153M (+3%) |

AMD EPYC 7763 (x86-64, GitHub runner; perf counters unavailable, wall time):

| Program | match | threaded |
| --- | --- | --- |
| loop | 190 ms | 225 ms (+19%) |
| fib | 56 ms | 160 ms (+186%) |

The real interpreter compiles to a single shared indirect branch (`objdump` of `Vm::run`: one
`br`), and LLVM declines to tail-duplicate it even with `-tail-dup-indirect-size` raised to 1000,
so a threaded VM would have to be a rewrite on top of `become`, which is still unstable.

## Decision

Keep the `match` dispatch. Modern indirect-branch predictors (Apple's, AMD Zen 3) predict the
shared dispatch branch well: the `match` loop runs at 6.6 instructions per cycle on `loop`. The
threaded version executes more instructions (each handler re-derives its state from arguments)
and loses on both architectures, badly so for call-heavy code, where frame state no longer stays in
registers across handlers. Interpreter speed comes instead from doing less per instruction
(superinstructions, #5) and from fewer instructions (codegen). Revisit if `become` stabilizes and
the handlers can share a register-pinned state.
