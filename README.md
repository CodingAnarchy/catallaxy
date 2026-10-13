# Catallaxy

Catallaxy is a statically typed language that reads like Ruby, checks every program before it runs,
and runs its threads on all cores at once, with no global lock and no way for a data race to crash the VM.

```ruby
interface Named { fn name(self) -> String }

class User {
  full_name: String
  email: String?
  fn name(self) -> String { self.full_name }
}

pub fn greet(x: Named) -> String { "Hello, #{x.name()}" }

fn shout(x) { x.name().upcase() }          # untyped: requirements inferred from the body

users = [User.new("Ada Lovelace", nil), User.new("Grace Hopper", "grace@navy.mil")]
puts(users.map { |u| greet(u) })
puts(users.map { |u| u.email ?? "none" })
puts(shout(users[0]))

squares = (1..1_000_000).to_list().par_map { |n| n * n }   # runs on every core
puts(squares.sum())
```

The name is Hayek's and Mises's word for the order that emerges from many independent actors exchanging
without a central planner, just as Catallaxy threads coordinate through ownership and explicit exchange
instead of a global lock.

## Highlights

- **Checked before it runs.** `catx check --json` reports every error with a code, an exact span and
  machine-applicable fixes, so people and tools (including LLMs) can validate code without executing it.
- **Inference with structural interfaces.** Annotate public signatures; everything else is inferred.
  Interfaces are structural, so duck typing still works, and default methods (`map`, `select`, `sort_by`,
  ...) come free to anything with an `each`.
- **Real parallelism.** Threads map 1:1 to OS threads and run bytecode in parallel. Every object is
  *owned* (by one thread), *frozen* (immutable, shared freely) or *shared* (synchronized). The checker
  rejects unsafe sharing; the runtime re-checks where static types cannot reach, so a racy program may
  compute a wrong answer but never corrupts memory.
- **Thousands of connections per thread.** Fibers park on sockets, channels, files and processes
  instead of blocking, so one OS thread serves thousands of connections (10,000 open at once in
  about a second in `bench/net.cx`). Sockets, an HTTP(S) client, regexes, processes and file-system
  calls are in the standard library.
- **Errors are values.** `Result[T, E]` and `?` for recoverable errors, `T?` for absence, panics for bugs.
  A panic unwinds only its own thread.
- **Fast enough to matter.** A register-based bytecode VM with NaN-boxed values, typed opcodes, inline
  caches, and a garbage collector in which each thread collects its own young objects without
  stopping the others, backed by a parallel mark-sweep of the whole heap.

## Install

```sh
cargo install catallaxy        # installs the `catx` binary
```

or build from source:

```sh
git clone https://github.com/CodingAnarchy/catallaxy
cd catallaxy
cargo build --release
./target/release/catx run examples/hello.cx
```

## Usage

```
catx run file.cx [args...]      run a program
catx check [--json] file.cx     type-check without running
catx fmt [--check] file.cx      format source in place
catx test [dir]                 run a conformance-style test directory (see the reference, §10)
catx repl                       interactive session
catx lsp                        language server (stdio)
```

## Documentation

- [Language reference](docs/reference.md) — syntax, types and semantics.
- [Standard library](docs/stdlib.md) — every builtin type, method and function.
- [Design](docs/design.md) — why the language and runtime look the way they do.
- [Examples](examples/) — complete programs, each with its expected output.
- Evaluations of ideas not adopted (yet), with measurements: [escape analysis](docs/escape-analysis.md),
  [concurrent marking](docs/gc-concurrent-marking.md), [minor-collection cost](docs/gc-minor-cost.md),
  and the dispatch and JIT experiments under `experiments/`.

## Repository layout

| Crate | Role |
| --- | --- |
| `catallaxy-gc` | segmented heap, thread-local allocation, safepoints, parallel mark, lazy sweep |
| `catallaxy-runtime` | NaN-boxed values, object layouts, classes, ownership states |
| `catallaxy-vm` | bytecode format, interpreter, inline caches, program loader |
| `catallaxy-compiler` | lexer, parser, type checker, bytecode emitter, formatter |
| `catallaxy-stdlib` | native standard library (strings, collections, threads, channels, fibers, JSON, files, directories, regexes, processes, sockets, HTTP) |
| `catallaxy` | the `catx` command-line tool, REPL and language server |
| `catallaxy-capi` | C ABI for embedding and native extensions (`include/catallaxy.h`) |

The memory-management and object-model `unsafe` code lives in `catallaxy-gc` and `catallaxy-runtime`,
which run under Miri and ThreadSanitizer. `catallaxy-vm` (the interpreter's register file and object
views), `catallaxy-stdlib` (native object access) and `catallaxy-capi` (the FFI boundary) use `unsafe`
through those crates' APIs; the compiler and the `catallaxy` crate use none.

`tools/difffuzz` is the differential fuzzer: it generates programs and runs each one in several
modes (release, debug, GC stress, non-generational, no marker threads), which must all agree.

## Status

Catallaxy implements the v1 design (`docs/design.md`, phases 1–4) and the post-v1 work the design
lays out. It is tested on Linux, macOS and Windows.

- the full static type system: inference, structural interfaces with default methods, implicit
  generics, nullables, unions, exhaustive `match`, `Result` and `?`, extensions, frozen types,
  concurrency capabilities checked at compile time (including through local aliases: `b = a`
  then `freeze(a)` makes `b` frozen too, and `move a` makes `b` unusable);
- the runtime: NaN-boxed values, fixed-layout objects, ownership states with runtime backstops,
  `--promote-on-access`, a parallel mark-sweep collector with thread-local allocation and lazy sweep,
  plus thread-local young collection;
- threads, channels, `Mutex`, `Atomic`, `with lock`, `par_map`/`par_each`/`par_reduce` on a
  work-stealing pool, and fibers (thousands of cooperative threads per OS thread for I/O-bound
  work);
- a standard library for scripts and servers: files, directories, paths, file metadata, regexes,
  processes (run to completion or stream through pipes), TCP sockets and listeners on a
  non-blocking reactor, and an HTTP(S) client;
- tooling: `check --json`, formatter, REPL, a language server (diagnostics, hover, definition,
  completion, references, rename, signature help, inlay hints, symbols, formatting), Rust
  extension API and C ABI.

Environment variables tune the runtime:

| Variable | Effect |
| --- | --- |
| `CATX_THREADS` | worker threads for `par_*` (default: one per core) |
| `CATX_IO_THREADS` | threads that run blocking calls for fibers (default 16) |
| `CATX_GC_THREADS` | dedicated marker threads for major collections (default: one per core, minus one) |
| `CATX_GC_GENERATIONAL=0` | turn off thread-local young collection |
| `CATX_GC_NURSERY` | young-generation size per thread in bytes (default 4 MiB; see `docs/gc-minor-cost.md`) |
| `CATX_GC_STATS=1` | print collection count, heap size and pause times at exit |
| `CATX_GC_TRACE=1` | print the phases of every major and minor collection |
| `CATX_GC_STRESS=1` | collect very often (every 64 KB allocated), to flush out rooting bugs |
| `CATX_TEST_TIMEOUT` | seconds `catx test` gives each program before failing it (default 120) |

Validation: a conformance suite of 133 programs (run + reject), 42 example programs with checked
output, Miri (Tree Borrows) and ThreadSanitizer on the collector and runtime, the whole corpus
(threads, channels, `par_*`, fibers and sockets) under ThreadSanitizer, Loom models of the
object-lock and mark protocols, continuous fuzzing of the compiler front end, differential fuzzing
of generated programs across runtime modes (in CI and nightly), and a GC stress mode that the
whole corpus passes. On an Apple M-series machine Catallaxy runs the `bench/` programs 2–13× faster
than CPython 3.13 and type-checks 8,000 lines in about 20 ms. The design budgets 10–20% of
single-thread speed for thread safety. Against a build with ownership checks and list memory
orderings stubbed out (keeping the GC polls any collector needs), the single-threaded `bench/`
programs pay 0–11%, the most on field-heavy `nbody`.

New in 0.3 ([milestone](https://github.com/CodingAnarchy/catallaxy/milestone/1)): networking
(sockets, listeners and an HTTP client, integrated with fibers), the scripting standard library
(`Regex`, `Process`, `Dir`, `Path`, file metadata), language-server navigation and refactoring,
cheaper calls and more superinstructions (up to 17% fewer cycles on call-heavy code), fiber results
stored in their handles (an unjoined result is freed with it), alias-aware `freeze` and `move`
checks, Windows support, a differential fuzzer (it found five bugs, all fixed), and the whole corpus
under ThreadSanitizer. Concurrent marking, escape analysis and an adaptive nursery were measured
and declined for now; the write-ups are linked above.

Since 0.1 (tracked as [`post-v1` issues](https://github.com/CodingAnarchy/catallaxy/issues?q=label%3Apost-v1)):
thread-local young collection and dedicated marker threads (a 1.6 GB heap's longest pause fell from
143 ms to 25 ms), fibers, lock-free reads of shared dicts, superinstructions and tighter codegen
(up to 42% fewer cycles), and a C ABI (`catallaxy-capi`). Two experiments ended in a "not now":
threaded dispatch was slower than the current `match` loop on both aarch64 and x86-64
(`experiments/dispatch`), and a Cranelift JIT, though 4–10× faster on integer kernels, covered too
little of real programs to justify its cost (`experiments/jit`).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
