# difffuzz

Differential fuzzing of whole Catallaxy programs across builds and GC modes.

`difffuzz` generates random programs that are well typed, terminate, and print deterministic
output, then runs each one under several configurations of `catx` and compares the results. Any
difference in stdout or exit code, any crash, any timeout, and any `internal error` or Rust panic
message is a failure. The front-end fuzzer (`fuzz/`, target `check`) only covers parsing and type
checking; this one exercises the interpreter, the fused opcodes, the young-generation and
whole-heap collectors, threads, channels, fibers and the parallel collections.

It is a standalone crate (not a member of the main workspace) with no dependencies.

## Usage

From the repository root, build both builds of `catx` and the fuzzer:

```sh
cargo build --release && cargo build
cargo build --release --manifest-path tools/difffuzz/Cargo.toml
alias difffuzz=./tools/difffuzz/target/release/catallaxy-difffuzz
```

Then:

```sh
difffuzz --time 300                         # fuzz for 5 minutes
difffuzz --time 180 --corpus tools/difffuzz/corpus --corpus tests/conformance/run
difffuzz gen 12345 > p.cx                   # the program for seed 12345
difffuzz run p.cx                           # run the oracle on existing programs
difffuzz minimize failing.cx                # reduce a failing program (writes failing.min.cx)
```

Options (`difffuzz --help`):

| Option | Default | Meaning |
| --- | --- | --- |
| `--time SECS` | 60 | time budget for generated programs (the corpus always runs in full) |
| `--count N` | unlimited | stop after N generated programs |
| `--seed N` | from the clock | program `i` uses seed `N + i`; printed at start |
| `--jobs N` | half the cores | programs checked in parallel |
| `--catx PATH` / `--catx-debug PATH` | `target/release/catx`, `target/debug/catx` | the two builds |
| `--modes a,b,...` | the five below | modes to run; the first is the baseline |
| `--corpus DIR` | none | also run every `.cx` file in DIR (repeatable) |
| `--out DIR` | `target/difffuzz/failures` | where failures are saved |
| `--timeout SECS` | 60 | per-run timeout |
| `--minimize-secs N` | 120 | time budget per minimization; 0 disables |
| `--save-rejects DIR` | off | keep generated programs that `catx check` rejects (with diagnostics) |

The exit status is 1 if anything failed, 0 otherwise.

### Modes

| Mode | Build | Environment |
| --- | --- | --- |
| `release` (baseline) | release | |
| `debug` | debug | |
| `gc-stress` | release | `CATX_GC_STRESS=1` |
| `gc-nongen` | release | `CATX_GC_GENERATIONAL=0` |
| `gc-threads0` | release | `CATX_GC_THREADS=0` |
| `gc-stress-nongen` (opt-in) | release | both of the first two |
| `debug-gc-stress` (opt-in) | debug | `CATX_GC_STRESS=1` (slow) |

### Failures

Each failure is saved as `<kind>-<name>.cx` (the program) and `.log` (every mode's exit status,
stdout and stderr). The first two failures of each kind are also minimized into `.min.cx` by
deleting statements and whole blocks while the program still passes `catx check` and still fails
the same way. Kinds are:

- `mismatch:<mode>`: stdout or exit status differs from the baseline;
- `crash:<mode>`: killed by a signal (segfault, abort, stack overflow);
- `internal:<mode>`: output contains `internal error`, `panicked at` or a Rust backtrace;
- `timeout:<mode>`: the run exceeded `--timeout` although the baseline took less than a
  twentieth of it (otherwise the program is counted as "too slow to judge", not as a failure);
- `panic...`: a generated program panicked in the baseline. The generator only builds programs
  that cannot panic, so this is an implementation bug unless the generator is wrong. Programs
  without the `# difffuzz program` header (such as the conformance suite) are only compared,
  never required to exit 0.

Setting `DIFFFUZZ_ON_TIMEOUT` runs a command against a hung process before it is killed, with
`{pid}` replaced by its id, and adds its output to the log: for example
`DIFFFUZZ_ON_TIMEOUT='sample {pid} 1'` on macOS or
`DIFFFUZZ_ON_TIMEOUT='gdb -p {pid} -batch -ex "thread apply all bt"'` on Linux.

To turn a real bug into a regression test, minimize it, fix it, and add the program and its
expected output to `tests/conformance/run/`.

## How the generator works

`src/gen.rs` is type-directed. It keeps every variable in scope with its type and builds
expressions of a requested type from productions that are known to type-check: integers
(including big integers), booleans, strings, `List[Int]`, `Dict[String, Int]`, `Set[Int]`,
`Int?`, `frozen List[Int]`, closures, generated classes (fields, methods, single inheritance with
`super`), generated enums (matched exhaustively or with `_`), `Result[Int, String]` with `?`, and
optionals with `?`, `??`, `if let` and `match`. Statements add declarations, bounded
reassignments, collection mutations, `if`, `for`, `while`, `loop`/`break`, `each` blocks and
`match`. Templates add concurrency: `spawn` and `spawn move`, channels (bounded and unbounded,
closed and counted), `Atomic`, `Mutex`, `shared class` with `with lock`, fibers with
`Fiber.yield()`, fiber channels, fibers inside threads, panicking threads and fibers,
`freeze`, `par_map`/`par_select`/`par_reduce`/`par_each`, allocation churn and `gc()`. At the
end the program prints every top-level variable, so corruption anywhere shows up in the output.

Termination and bounded cost come from construction: loops have literal bounds and nest at most
two deep; recursion goes through fixed helpers with small literal depths; `*` always has a
literal operand and integer variables are reassigned only modulo a constant or by adding a
literal; reassigned lists and strings are truncated; functions never mutate their parameters.

Determinism: only the main thread and fibers print. Values that cross threads are joined in a
fixed order, values received from several threads are sorted, and `par_reduce` uses only
associative, commutative operators. Fibers on one thread interleave deterministically, so their
logs are printed in order.

Generated programs that `catx check` rejects are discarded and counted; the rejection rate is
printed at the end (about 0.5%). Use `--save-rejects` to see them. They mark places where the
generator's model of the type system is too optimistic, or where the checker is imprecise.

## Corpus

`corpus/` holds a seed corpus of generated programs covering every template. CI runs it, plus
`tests/conformance/run`, before generating new programs. To add a program, copy one from
`difffuzz gen SEED` or a minimized failure; generated programs start with
`# difffuzz program` and must exit 0.

## CI

The `Differential fuzz` job in `.github/workflows/ci.yml` runs the corpus and three minutes of
generated programs on every push and pull request, and uploads the failures as an artifact.
`.github/workflows/difffuzz-nightly.yml` runs a longer campaign with the opt-in modes every night.
