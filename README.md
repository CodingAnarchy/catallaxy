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
- **Errors are values.** `Result[T, E]` and `?` for recoverable errors, `T?` for absence, panics for bugs.
  A panic unwinds only its own thread.
- **Fast enough to matter.** A register-based bytecode VM with NaN-boxed values, typed opcodes, inline
  caches and a parallel mark-sweep collector with thread-local allocation.

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
catx test [dir]                 run a conformance-style test directory
catx repl                       interactive session
catx lsp                        language server (stdio)
```

## Documentation

- [Language reference](docs/reference.md) — syntax, types and semantics.
- [Standard library](docs/stdlib.md) — every builtin type, method and function.
- [Design](docs/design.md) — why the language and runtime look the way they do.
- [Examples](examples/) — complete programs, each with its expected output.

## Repository layout

| Crate | Role |
| --- | --- |
| `catallaxy-gc` | segmented heap, thread-local allocation, safepoints, parallel mark, lazy sweep |
| `catallaxy-runtime` | NaN-boxed values, object layouts, classes, ownership states |
| `catallaxy-vm` | bytecode format, interpreter, inline caches, program loader |
| `catallaxy-compiler` | lexer, parser, type checker, bytecode emitter, formatter |
| `catallaxy-stdlib` | native standard library (strings, collections, threads, channels, JSON, files) |
| `catallaxy` | the `catx` command-line tool, REPL and language server |

All `unsafe` code lives in `catallaxy-gc` and `catallaxy-runtime`.

## Status

Catallaxy 0.1 implements the v1 design (`docs/design.md`, phases 1–4 without fibers):

- the full static type system: inference, structural interfaces with default methods, implicit
  generics, nullables, unions, exhaustive `match`, `Result` and `?`, extensions, frozen types,
  concurrency capabilities checked at compile time;
- the runtime: NaN-boxed values, fixed-layout objects, ownership states with runtime backstops,
  `--promote-on-access`, a parallel mark-sweep collector with thread-local allocation and lazy sweep;
- threads, channels, `Mutex`, `Atomic`, `with lock`, `par_map`/`par_each`/`par_reduce` on a
  work-stealing pool;
- tooling: `check --json`, formatter, REPL, language server, Rust extension API.

Validation: a conformance suite of 110 programs (run + reject), 42 example programs with checked
output, Miri (Tree Borrows) and ThreadSanitizer on the collector and runtime, Loom models of the
object-lock and mark protocols, continuous fuzzing of the compiler front end, and a GC stress mode
(`CATX_GC_STRESS=1`) that the whole corpus passes. On an Apple M-series machine Catallaxy runs the
`bench/` programs 1.5–7× faster than CPython 3.13 and type-checks 8,000 lines in about 20 ms.

Post-v1 work, as the design lays it out, is tracked in the
[`post-v1` issues](https://github.com/CodingAnarchy/catallaxy/issues?q=label%3Apost-v1): thread-local
young collection, shorter GC pauses, fibers, a seqlock fast path for shared dict reads,
superinstructions, threaded dispatch, a JIT experiment and a C extension ABI.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
