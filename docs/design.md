# Catallaxy Language Design

*Matthew Tanous · October 12, 2026.* The source of this document is the [Catallaxy Language Design](https://claude.ai/artifact/RbuYbCUoz3Fps7tipXLTZJ) doc; this is a markdown copy.

## Goals and non-goals

Catallaxy is a statically typed language that reads like Ruby, checks every program before it runs, and executes its threads on all cores at once, with no global lock and no way for a data race to crash the VM.

**Goals**

- True parallelism: N threads executing bytecode on N cores, no interpreter-wide lock.
- Strict static types with heavy inference: annotate signatures, infer the rest, and check structurally so duck typing still works.
- Machine-checkable before running: a fast `check` command with structured diagnostics, so people and LLMs can validate generated code without executing it.
- Memory safety under races: a racy program may compute wrong answers, but it can never segfault, corrupt the heap, or observe a half-built object. Most unsafe sharing is rejected at compile time.
- Familiar ergonomics: classes, closures, brace blocks, lists and dicts, string interpolation, REPL.
- Single-thread speed within roughly 10–20% of a design that ignored threads entirely.

**Non-goals (for v1)**

- Source compatibility with Ruby or Python, or their C extension APIs.
- Runtime metaprogramming: `method_missing`, `eval`, or adding methods to classes at runtime. Statically resolved extension methods cover the common case.
- A JIT. Static types make one easier later, but v1 is an interpreter.
- A concurrent or incremental garbage collector. v1 pauses the world; pause-time work comes later.

## Type system

Every program is fully type-checked before any bytecode runs. Public signatures are annotated, everything inside a function body is inferred, and interfaces are structural, so code that would duck-type in Ruby still compiles as long as the methods actually exist.

```
interface Named { fn name(self) -> String }

class User {
  full_name: String
  email: String?
  fn name(self) -> String { self.full_name }
}

pub fn greet(x: Named) -> String { "Hello, #{x.name()}" }

fn shout(x) { x.name().upcase() }        # private: x's interface is inferred

users = load_users()                     # List[User], inferred
emails = users.map { |u| u.email ?? "none" }
```

**Inference.** Bidirectional and local, not whole-program Hindley–Milner. Locals, block parameters and generic arguments at call sites are inferred from context. Annotations are required on `pub` functions, class fields and interface members. That keeps errors local to where the mistake is, lets modules be checked independently, and gives LLMs an explicit contract to write against.

**Structural interfaces (checked duck typing).** A type satisfies an interface by having the right methods; there is no `implements` clause. `List`, `Set` and user classes with an `each` method are all `Iterable`. Interfaces can also be written inline: `fn render(x: { fn to_s(self) -> String })`.

**Interfaces as annotations.** Any parameter, field or return type can be an interface, so public signatures say what a value must *do*, not what class it is:

```
pub fn total(xs: Enumerable[Int]) -> Int { xs.reduce(0) { |a, b| a + b } }
pub fn top[T: Comparable](xs: Enumerable[T], n: Int) -> List[T] { … }
pub fn log_all(items: Enumerable[Display & Hashable]) { … }
```

An interface in parameter position is sugar for a generic bound, like Rust's `impl Trait`; since generics are erased, both compile to the same code. Interfaces combine with `&`.

**Standard interfaces, Ruby-style.** The core library defines `Enumerable[T]` (requires `each`), `Comparable` (requires `<=>`), `Hashable`, `Display` (requires `to_s`), `Indexable[T]` and `Callable`. Interfaces can carry default methods: any type with a matching `each` gets `map`, `select`, `reduce`, `sort_by`, `each_slice` and the rest, exactly as including `Enumerable` works in Ruby, but with no `include` line and with every call type-checked.

**Implicit generics, for maximum duck typing.** A private function may leave parameters unannotated, as `shout` does above. The checker infers the structural interface each parameter needs from the body, and checks every call site against it. This is Ruby-style duck typing with compile-time guarantees. Requiring annotations on `pub` functions keeps inferred interfaces from leaking into APIs, where they make error messages hard to read.

**Classes and extensions.** Classes are nominal, with fields declared up front, single inheritance and mixin interfaces with default methods. Classes cannot be reopened at runtime. Instead, `extend String { fn shout(self) -> String { … } }` adds methods that resolve statically and are visible only where their module is imported.

**Nil and unions.** `T` never holds nil; `T?` might. Unwrapping uses `?.`, `??`, `if let`, or flow narrowing after `if x != nil`. Union types (`Int | String`) narrow with `is` checks and `match`.

**Enums and match.** Enums carry payloads, as in Rust or Swift, and `match` must be exhaustive. `Result[T, E]` with a `?` operator is the only way to return recoverable errors. There are no exceptions. A panic is reserved for bugs: failed asserts, out-of-bounds indexing, force-unwrapping nil. It unwinds only the thread that panicked, and joining that thread returns an error value, so one crashed worker never takes down the process.

**Generics.** `List[T]`, `Dict[K, V]`, and user generics with interface bounds (`fn max[T: Comparable](xs: List[T]) -> T?`). Generics are erased: every value is still one 64-bit `Value` at runtime, so the VM needs no monomorphization.

**Concurrency capabilities in the type system.** Every type is classified as one of:

| Capability | Examples | Can cross threads? |
| --- | --- | --- |
| Frozen | Int, String, `frozen class Point`, frozen lists | Yes, freely |
| Shareable | Mutex\[T\], Atomic\[T\], Channel\[T\], `shared class` with synchronized fields | Yes, by reference |
| Owned | ordinary classes, List, Dict | Only by move |

A `spawn` or `par_map` block that captures an owned value by reference is a compile error, and the message suggests `move`, `freeze`, or a `Mutex`. The runtime ownership checks from the concurrency model stay as a backstop for `Dyn` values and `--promote-on-access`.

**`Dyn` escape hatch.** Parsed JSON, plugin data and FFI results can be typed `Dyn`. Operations on `Dyn` are checked at runtime, and turning a `Dyn` into a typed value takes `as?` or a decoder. Using `Dyn` is always visible in the source.

**Tooling for people and agents.** `check --json` emits diagnostics with exact spans, error codes and machine-applicable fix suggestions. An LSP server, a deterministic formatter, and `check` fast enough to run on every save (target: under 200 ms for 10k lines) are part of the language, not afterthoughts.

## Concurrency model

Recommendation: one shared heap, ordinary threads, and three kinds of object (frozen, owned, shared). The type checker enforces who may touch what at compile time, and the VM re-checks at runtime only where static types cannot reach. This gives "just spawn a thread" ergonomics while making most objects lock-free in practice.

**One heap, not per-thread heaps.** Erlang-style isolated heaps make GC easy but force copying on every message, and that cost shows up everywhere. A shared heap with a stop-the-world collector is simpler to get right and lets threads pass large structures for free.

**Three object states**, stored in two bits of each object header:

| State | Who can read | Who can write | Cost of access |
| --- | --- | --- | --- |
| Frozen | any thread | nobody | none: plain loads |
| Owned | owning thread | owning thread | one header check, no atomics |
| Shared | any thread | any thread | per-object lock or atomic op |

- New objects are **owned** by the thread that allocated them. Nearly all objects live and die this way, so the common path never touches an atomic.
- `freeze(x)` makes an object (deeply) immutable and changes its static type to the frozen form. Literals, strings, code objects, classes and module constants are frozen by default.
- Handing a mutable object to another thread (spawn captures, a channel send, storing it in a shared object) either **moves** it, after which the compiler rejects any further use by the sender, or requires a Shareable wrapper such as `Mutex[T]`. See the capability table in the type system.
- Runtime backstop: touching another thread's owned object through a path the checker cannot see (mainly `Dyn` values and FFI) raises `OwnershipError`, a clean runtime error rather than undefined behavior. Under the `--promote-on-access` flag, the access instead promotes the object to shared and logs a warning.

**Race semantics.** Within shared objects, each single operation (`list.push`, `dict[k] = v`, attribute set) is atomic and linearizable. Compound operations (`d[k] += 1`) are not, just as in Java or free-threaded Python. Explicit tools cover the rest: `Mutex`, `Atomic`, channels, and a `with lock(obj)` block that locks one shared object.

**Why not "everything shared, lock everything"?** That is the PEP 703 approach and it works, but every dict and list access pays for a lock word check and biased refcounts. The ownership bit buys the same safety with a cheaper common path and catches accidental sharing as an error instead of a heisenbug.

## Object model and value representation

Every value is one 64-bit word that can be loaded and stored atomically, so no thread ever sees a torn value. Because classes are static, every object's field layout is fixed at compile time and a field access is a single indexed load.

**Values: NaN-boxing.** A `Value` is a `u64`. Real doubles are stored as-is. The quiet-NaN space encodes everything else:

- 48-bit heap pointer (object, string, list, closure)
- 48-bit small integer (overflow promotes to a heap bigint)
- `nil`, `true`, `false`, interned symbols

This keeps floats unboxed and makes every slot a single machine word. With static types, typed opcodes such as `add_int` and `add_float` skip tag checks entirely; tags matter only for erased generics, unions and `Dyn`. A uniform one-word representation is what lets generics stay erased instead of monomorphized.

**Object header (16 bytes):**

| Word | Bits | Field |
| --- | --- | --- |
| 0 | 64 | Class pointer: vtable, interface tables, field count, capability. Written once at allocation |
| 1 | 2 | State: owned / frozen / shared |
| 1 | 16 | Owner thread id (meaningful only when owned) |
| 1 | 2 | GC mark bits |
| 1 | 8 | Lock byte (parking\_lot-style: spin, then park) |
| 1 | 32 | Identity hash, lazily assigned |

**Fixed layouts instead of shapes.** Field offsets are known statically, so a typed field read compiles to `get_field r1, r2, #3`, with no lookup and no cache. Because an object's class never changes after allocation, header word 0 is never written again, which removes a whole category of races. Hidden classes are kept only for `Dyn` records such as parsed JSON objects.

**Fields.** Field values live inline after the header. When the checker knows a value's capability, it picks the cheapest access: plain loads and stores for owned and frozen objects, `Acquire` loads and `Release` stores for shared ones. Only `Dyn` and erased generic code needs the runtime state check.

**Containers.**

- **List:** header, length, pointer to a backing array. Growing a shared list allocates a new array and swaps the pointer under the object's lock. Readers holding the old array stay safe because the GC keeps it alive. Lists created with room for at most 7 elements (literals, small vectors, `[]`) keep them in an array inside the list object, so they cost one heap slot and no `malloc`. The pointer then points into the object. A list that outgrows that array moves out of line the same way and never moves back. A reader still holding the inline array reads old values from memory that lives as long as the list.
- **Dict:** insertion-ordered open-addressing table (the CPython 3.6+ layout). Shared dicts take the lock for writes; v1 also locks reads, and a seqlock fast path for reads is a later optimization.
- **String:** immutable UTF-8 with a cached hash. Mutable buffers are a separate `StringBuilder` type, so strings are always frozen and freely shared.

## Memory management

v1 uses a non-moving, parallel, stop-the-world mark-sweep collector with thread-local allocation. Non-moving matters in Rust: raw pointers held across a safepoint stay valid, which removes a whole class of `unsafe` bugs.

**Allocation.** The heap is a set of 256 KB segments, each split into pages of one size class (16, 32, 48 … 2048 bytes; larger objects get their own segment). Each thread owns its current page per size class and bump-allocates from it with no atomics. Only grabbing a fresh page touches a global (sharded) free-page list.

**Safepoints.** Every thread polls a global `gc_requested` flag at backward jumps, calls, and allocation slow paths: one load and a predictable branch. A thread in native code or blocking I/O sets its state to `in_native` first. The collector treats such threads as already stopped, the same trick the JVM and Go use, so a thread stuck in `read()` never delays a collection.

**Roots.** Interpreter frames and registers live in a VM-managed stack of `Value`s, so scanning them is precise and trivial. Rust code that holds `Value`s across a possible GC must root them in a `HandleScope<'a>`. Handles borrow the scope, so the borrow checker stops them from escaping. This is the main safety boundary for native code.

**Collection cycle.**

1. The requesting thread sets `gc_requested` and waits until every thread is parked or `in_native`.
2. All parked threads become mark workers, sharing a work-stealing deque (crossbeam). Roots are split per thread.
3. Marking flips the mark bit with an atomic `fetch_or`; whoever sets it pushes the object.
4. Threads resume immediately. Sweeping happens lazily: a page is swept the next time a thread wants to allocate from it.
5. Any page still unswept is swept at the start of the next collection. At that point every wholly empty segment is known, and the collector returns them to the OS (`munmap`, or `VirtualFree` on Windows). It keeps enough empty pages for the allocation budget until the following collection, so a program in a steady state does not unmap segments only to map them again.

**Running out of memory.** Allocation never fails or collects, so when the OS refuses a new segment the heap takes one from a small reserve of mapped segments (4 MB), hands a 2 MB headroom block back to the OS so the runtime's own allocations can go on, and requests a collection. That collection sweeps at once and refills the reserve from the segments it emptied, or from the OS. If it cannot, the next thread to reach a safepoint panics with a `MemoryError`; only an allocation that finds the reserve used up aborts. `catx`'s global allocator does the same for allocations inside Rust's collections: a refused request releases the headroom block and is retried once, and the collection it schedules decides whether to raise the error. Allocations the runtime makes where it can raise an error (strings, `StringBuilder`, list and dict buffers, register and fiber stacks) check for refusal themselves, and for strings, builders, list pushes and dict inserts collect once and retry first.

**Key insight for later: thread-local young collection.** The ownership rules guarantee that frozen and shared objects never point to owned objects (freezing is deep, and promotion walks the graph). So a thread's owned young objects are reachable only from that thread. That thread can collect its own nursery without stopping anyone, much like OCaml 5's per-domain minor heaps. This should be the first major GC upgrade after v1, and it makes most garbage free to collect.

**Deliberately absent.** No reference counting (atomic refcount traffic is exactly the cost this design avoids), and no general finalizers. Resources close via explicit `close`/`with` blocks, with weak references for caches.

## Execution

The VM is a register-based bytecode interpreter, one instance per OS thread, all sharing frozen code objects and the shared heap. Static types let the compiler resolve most calls and field accesses ahead of time, so the VM does far less dynamic lookup than Ruby or Python.

**Pipeline.** Source → lexer → parser (hand-written, Pratt for expressions) → AST → name resolution → **type checker** (inference, interface checks, capability checks, exhaustiveness) → typed IR → bytecode. Nothing reaches the VM until it type-checks. Compiled functions are frozen `Code` objects, so every thread can run them with no synchronization.

**Bytecode.** 32-bit fixed-width instructions with 8-bit opcode and register operands, in the style of Lua 5.x. Opcodes are typed where the checker knows the types (`add_int`, `lt_float`, `get_field`, `call_static`) and generic only for unions, erased generics and `Dyn`. Dispatch is a `match` in a tight loop, which LLVM turns into a jump table; tail-call threading can be tried later if profiling demands it.

**Inline caches, only where needed.** Interface calls and `Dyn` operations get a per-site cache of `(class_id, method_index)` packed into one `AtomicU64`. A reader loads the word once and either hits or takes the slow path. Since the whole entry is one atomic word, a thread can never see a class from one write and a method from another.

**Method dispatch.** Calls on a known class compile to `call_static` (final methods) or a vtable slot (overridable methods). Calls through a structural interface look the method up by name in the receiver's class table on the first miss, then hit the inline cache. Extension methods are resolved at compile time and are just static calls. With no runtime class reopening, cached dispatch never needs invalidating.

**Threads.** v1 maps language threads 1:1 onto OS threads.

- `spawn { … }` returns a handle; `join` returns a Result holding the block's value, or the panic that ended the thread.
- Typed channels (bounded and unbounded) are the preferred way to move owned data.
- `par_map`, `par_each` and `par_reduce` on collections run on a shared work-stealing pool, and the compiler checks that the block captures only frozen or shared values.
- Lightweight fibers for I/O-bound work (M:N scheduling) are a phase-4 feature, not v1.

**Globals and modules.** Module objects are shared. Top-level constants are frozen on assignment. Module-level mutable variables are allowed but are shared objects, so they pay locking costs, which nudges people toward constants and passing state explicitly.

## Crate architecture and phases

Six crates in one Cargo workspace, layered so that all `unsafe` code sits in `runtime` and `gc`, where it can be fuzzed and run under Miri and ThreadSanitizer.

| Crate | Role | Depends on |
| --- | --- | --- |
| `catallaxy-gc` | segmented heap, thread-local allocation, safepoints, parallel mark, lazy sweep | — |
| `catallaxy-runtime` | NaN-boxed values, object layouts, classes, ownership states | `gc` |
| `catallaxy-vm` | bytecode format, interpreter, inline caches, program loader | `runtime`, `gc` |
| `catallaxy-compiler` | lexer, parser, type checker, bytecode emitter, formatter | `vm` |
| `catallaxy-stdlib` | native standard library | `vm`, `runtime`, `gc` |
| `catallaxy` | the `catx` CLI, REPL, language server, embedding API | all of the above |

The compiler never touches the heap directly; it hands frozen `Code` objects to the VM, which keeps the parser and type checker testable in isolation. The type checker is the largest single module in the project and gets its own test suite of programs that must and must not compile.

**Phases.** Each ends with something runnable and a growing conformance test suite.

1. **Typed core.** Lexer, parser, type checker for primitives, functions, classes and simple generics, bytecode compiler, VM, simple single-threaded mark-sweep, `check --json`. Exit: typed fib, closures and classes run, and ill-typed programs are rejected with precise diagnostics.
2. **Full type system and object model.** Structural interfaces, implicit generics, nullables, unions, enums and exhaustive `match`, `Result` and `?`, extension methods, lists, dicts, strings, modules, first LSP. Exit: few-hundred-line programs check and run within 2x of CPython.
3. **Parallelism.** Capability checking, ownership states, freeze, move, per-object locks, OS threads, channels, safepoints, parallel marking. Exit: an embarrassingly parallel benchmark scales near-linearly to 8 cores, with Loom and TSan tests on the runtime.
4. **Usability.** Stdlib, `par_*` collections, REPL, `Dyn` and JSON, error messages with fix suggestions, formatter, fibers for I/O.
5. **Performance.** Thread-local young collection, seqlock dict reads, superinstructions, then a JIT experiment if warranted.

## Open decisions

These are the choices that shape everything else; the first two should be settled before any parser code is written.

- [x] **Syntax family.** Decided: indentation is never significant. Blocks use `{ }` braces. A newline ends a statement unless the line ends with a binary operator, a comma, or an unclosed `(`, `[` or `{`; a line starting with `.` continues the previous one, for method chains. Everything is an expression, with implicit return of the last value. Dict literals use square brackets, `[host: "localhost", port: 8080]`, with `[:]` for an empty dict, so `{` always opens a block and the parser never has to guess.
- [x] **Ownership violations.** Decided: cross-thread access to an owned object raises `OwnershipError` by default. A `--promote-on-access` runtime flag instead promotes the object to shared on first foreign access and logs a warning naming the object's class and the access site, so prototypes run and the log shows what to fix.
- [x] **Typing.** Decided: strict static typing with as much inference and structural (duck) typing as possible. See the type system section.
- [x] **Error handling.** Decided: `Result[T, E]` with the `?` operator for recoverable errors, `panic` for bugs, no exceptions. A panic unwinds only its own thread, and `join` reports it as an error value.
- [x] **Implicit generics.** Decided: unannotated parameters are allowed only on private functions. `pub` signatures must be annotated, but an annotation can be any interface (`Enumerable[Int]`, `Comparable`, `Display & Hashable`), so public APIs still describe behavior rather than concrete classes.
- [x] **Integers.** Decided: arbitrary precision. Values that fit in 48 bits are stored inline in the `Value` word; overflow promotes transparently to a heap bigint.
- [x] **Native extensions.** Decided: v1 supports Rust extensions only, as ordinary crates against the VM's `HandleScope` API, so the borrow checker enforces rooting and ownership rules. A C ABI may come later as a thin wrapper, once the object model is stable.
- [x] **Build approach.** Decided: the conformance suite comes first (programs that must check, programs that must be rejected with specific error codes, expected outputs) and serves as the definition of done. An agent fleet builds the parser, type checker, compiler, stdlib and tooling phase by phase against it. Code in `runtime` and `gc` must also pass Miri, Loom, ThreadSanitizer and fuzzing gates, plus human review.
- [x] **Name.** Decided: **Catallaxy**, Hayek's and Mises's term for the order that emerges from many independent actors exchanging without a central planner, just as threads here coordinate through ownership and explicit exchange with no global lock. The crate name `catallaxy` was unclaimed on crates.io as of October 12, 2026. Proposed short forms: `catx` for the CLI and `.cx` for source files.
- [ ] **Next:** reserve the `catallaxy` crate name, GitHub org and a domain.
