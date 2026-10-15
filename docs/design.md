# Catallaxy Language Design

*Matthew Tanous · October 12, 2026. Status notes updated during 0.5.0.*

This document was written before the first line of code, and it still records why the language and
runtime look the way they do. Most of it is now built. Where the implementation differs from the
plan, or a part does not exist, a note in bold says so:

- **Built** notes say what shipped, where it differs in detail from the text before them;
- **Not built yet** notes mark parts of the plan that do not exist;
- **Declined** notes mark ideas that were tried or measured and not adopted, with a pointer to the
  write-up.

A paragraph with no note describes the code as it is. The [language reference](reference.md) is the
authority on syntax and semantics; this document explains the reasons.

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

Both still hold. **Declined:** a Cranelift method JIT was 4–10× faster on integer kernels but
covered too little of real programs to justify its cost (`experiments/jit`), and concurrent marking
was measured and left out for now (`docs/gc-concurrent-marking.md`). Pauses were shortened another
way: each thread collects its own young objects without stopping the others, and major collections
have dedicated marker threads (see Memory management).

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

**Structural interfaces (checked duck typing).** A type satisfies an interface by having the right methods; there is no `implements` clause. `List`, `Set` and user classes with an `each` method are all `Enumerable`. Interfaces can also be written inline: `fn render(x: { fn to_s(self) -> String })`.

**Interfaces as annotations.** Any parameter, field or return type can be an interface, so public signatures say what a value must *do*, not what class it is:

```
pub fn total(xs: Enumerable[Int]) -> Int { xs.reduce(0) { |a, b| a + b } }
pub fn top[T: Comparable](xs: Enumerable[T], n: Int) -> List[T] { … }
pub fn log_all(items: Enumerable[Display & Hashable]) { … }
```

An interface in parameter position is sugar for a generic bound, like Rust's `impl Trait`; since generics are erased, both compile to the same code. Interfaces combine with `&`.

**Standard interfaces, Ruby-style.** The core library defines `Enumerable[T]` (requires `each`), `Comparable` (requires `<=>`), `Hashable`, `Display` (requires `to_s`), `Indexable[T]` and `Callable`. Interfaces can carry default methods: any type with a matching `each` gets `map`, `select`, `reduce`, `sort_by`, `each_slice` and the rest, exactly as including `Enumerable` works in Ruby, but with no `include` line and with every call type-checked. **Built** as described, in the prelude (`crates/catallaxy-compiler/src/prelude/core.cx`). An interface whose methods take `Self`, such as `Comparable`, can be used only as a generic bound, not as a value type (E0375).

**Implicit generics, for maximum duck typing.** A private function may leave parameters unannotated, as `shout` does above. The checker infers the structural interface each parameter needs from the body, and checks every call site against it. This is Ruby-style duck typing with compile-time guarantees. Requiring annotations on `pub` functions keeps inferred interfaces from leaking into APIs, where they make error messages hard to read.

**Classes and extensions.** Classes are nominal, with fields declared up front, single inheritance and mixin interfaces with default methods. Classes cannot be reopened at runtime. Instead, `extend String { fn shout(self) -> String { … } }` adds methods that resolve statically and are visible only where their module is imported. **Built:** there is no mixin syntax; a class that structurally satisfies an interface gets its default methods. Extensions are exported with `pub extend`.

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

**Writes through `Dyn` (issue #160).** Generics are erased, so a typed collection converted to `Dyn` could be filled with values of the wrong type: `xs: List[String] = []; d: Dyn = xs; d.push(5)`. Typed code then failed far from the cause, with a `TypeError` where it read the `Int` back. A type test had the same hole: `d as? List[Int]` returned an alias that a later `(d as? List[Dyn])!.push("s")` broke, and an empty list passed a test for any element type. We considered three fixes:

1. **Run-time element tags.** A collection records its element types, and a write through `Dyn` is checked against them.
2. **Read-only `Dyn` views.** A typed collection can't be written through `Dyn` at all.
3. **Copy on conversion.** Converting to `Dyn` copies the collection, so no alias exists.

We chose tags. Read-only views would also need per-object state, to tell a `List[Dyn]` (which `Dyn` code must be able to write) from a `List[Int]`. They would reject correct `Dyn` code, and they don't fix type tests. Copying breaks identity: mutations through `Dyn` would be lost. It costs O(n) at every conversion, and it would need deep copies for nested collections.

How the tags work:

- **Storage.** A list keeps its tag in the high half of its embedded buffer's capacity word. That word is written once per list and never moves, so list objects keep their size classes, and capacity reads mask off the tag. A dict or set keeps two tags (key and value) in one extra word. A tag is a run-time type id plus one; 0 means unknown.
- **When a tag is set.** Only where a value is converted to `Dyn` and its static collection type is fully known (`d: Dyn = xs`, an argument or element of type `Dyn`). The checker records those conversions, and the code generator emits a `TagElems` instruction there. Tuple components and optional values are tagged too. Typed code never sets, reads or checks a tag: creating, pushing to or indexing a `List[Int]` runs the same instructions as before.
- **Which writes are checked.** Calls that may write: `push`, `insert`, `[]=`, `concat`, `add`, and every other mutating `List`, `Dict` or `Set` method with an element-typed parameter, plus `d[k] = v` and `d.key = v`. They are checked (`CheckArgs`, `CheckSet`) only when the receiver's static type is `Dyn`, or a collection type with `Dyn` in its element types, which is what a type test like `as? List[Dyn]` returns. A write that doesn't fit is a `TypeError` naming the value's type and the collection's.
- **Type tests.** A tagged collection is tested by its tags, without walking the elements. An empty `List[String]` is no `List[Int]`, and a `List[Int]` is no `List[Int?]` (generic types are invariant). A collection tagged `Dyn` (a `List[Dyn]` converted to `Dyn`) is tested element by element. If it passes, its tags narrow to the tested types, so the typed view the test returned stays valid.

**Cost.** Typed code paths gain no checks; their only extra work is the mask on capacity reads, and dicts and sets are one word larger. Measured on `bench/` and a typed list, dict and set micro-benchmark: within noise (numbers in the PR that closed #160). `Dyn` paths pay a tag store at conversion and a tag load (plus a type check when tagged) per checked write.

**Not covered yet (#219).**
- Collections that reach `Dyn` inside generic code, where the element type is a type parameter. They stay untagged and their `Dyn` writes stay unchecked, as before.
- Collections nested inside a tagged collection.
- Collections that reach `Dyn` through an interface value.
- Untagged collections from natives such as `Json.parse`, which a type test can't narrow.
- Instances of user generic classes (`Box[Int]` through `Dyn` and back as `Box`). These need type-argument tags of their own.

**Tooling for people and agents.** `check --json` emits diagnostics with exact spans, error codes and machine-applicable fix suggestions. An LSP server, a deterministic formatter, and `check` fast enough to run on every save (target: under 200 ms for 10k lines) are part of the language, not afterthoughts. **Built:** `catx check --json`, `catx fix` (applies the fixes), `catx fmt`, and `catx lsp`, which also offers the fixes as quick fixes. `catx check` type-checks 8,000 lines in about 20 ms on an Apple M-series machine.

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

**Built** as described: the states live in the object header (`crates/catallaxy-gc/src/header.rs`), along with `freeze`, `move` (`spawn move`, `ch.send(move x)`), `OwnershipError`, `--promote-on-access`, `Mutex`, `Atomic`, bounded and unbounded channels with `Channel.select`, and `with lock`, which is reentrant up to 1,000 levels.

**Why not "everything shared, lock everything"?** That is the PEP 703 approach and it works, but every dict and list access pays for a lock word check and biased refcounts. The ownership bit buys the same safety with a cheaper common path and catches accidental sharing as an error instead of a heisenbug.

## Object model and value representation

Every value is one 64-bit word that can be loaded and stored atomically, so no thread ever sees a torn value. Because classes are static, every object's field layout is fixed at compile time and a field access is a single indexed load.

**Values: NaN-boxing.** A `Value` is a `u64`. Real doubles are stored as-is. The quiet-NaN space encodes everything else:

- 48-bit heap pointer (object, string, list, closure)
- 48-bit small integer (overflow promotes to a heap bigint)
- `nil`, `true`, `false`, interned symbols

**Built:** pointers, integers and the constants are as described (`crates/catallaxy-runtime/src/value.rs`). There is no symbol value: method and field names are interned inside the compiler and VM, and the language has no symbol type.

This keeps floats unboxed and makes every slot a single machine word. With static types, typed opcodes such as `add_int` and `add_float` skip tag checks entirely; tags matter only for erased generics, unions and `Dyn`. A uniform one-word representation is what lets generics stay erased instead of monomorphized.

**Object header (16 bytes).** As built (`crates/catallaxy-gc/src/header.rs`), word 0 points to the class's `GcVTable`, the first field of the runtime class descriptor, and is written once at allocation. Word 1 is one atomic word:

| Bits | Field |
| --- | --- |
| 0..2 | State: owned / frozen / shared |
| 2..4 | GC mark color |
| 4 | Lock held (parking\_lot-style: spin, then park) |
| 5 | Lock has parked waiters |
| 6..16 | Lock recursion depth |
| 16..32 | Owner thread id when owned; lock holder when shared and locked |
| 32..61 | Identity hash (29 bits), lazily assigned |
| 61 | Remembered: an old object in its owner's remembered set |
| 62 | Young: allocated since its owner's last minor collection |
| 63 | Minor-collection mark |

The plan had a separate lock byte and a 32-bit hash; the lock state and the three generational bits took their room.

**Fixed layouts instead of shapes.** Field offsets are known statically, so a typed field read compiles to `get_field r1, r2, #3`, with no lookup and no cache. Because an object's class never changes after allocation, header word 0 is never written again, which removes a whole category of races. Hidden classes are kept only for `Dyn` records such as parsed JSON objects. **Built:** a parsed JSON object is a `Dict[String, Dyn]` that starts in a "shape mode": its keys live in shared, immutable shapes with a transition tree, and `DynGet` caches `(shape, slot)` per site. It falls back to the ordinary table on a delete, a non-string key or more than 32 keys (`crates/catallaxy-runtime/src/dict.rs`).

**Fields.** Field values live inline after the header. When the checker knows a value's capability, it picks the cheapest access: plain loads and stores for owned and frozen objects, `Acquire` loads and `Release` stores for shared ones. Only `Dyn` and erased generic code needs the runtime state check. **Built differently:** there is one `GetField` and one `SetField` instruction, and each checks the header state at run time: one load and a predictable branch. Owned and frozen objects then get relaxed loads and stores; other objects get the ownership check and `Acquire`/`Release`. **Not built yet:** choosing the access statically where the checker knows the capability.

**Containers.**

- **List:** header, length, pointer to a backing array. Growing a shared list allocates a new array and swaps the pointer under the object's lock. Readers holding the old array stay safe because the GC keeps it alive. Reads never lock, even on shared lists: the length and the array pointer are atomic words, and a replaced array is freed only at the next stop-the-world collection. Lists created with room for at most 7 elements (literals, small vectors, `[]`) keep them in an array inside the list object, so they cost one heap slot and no `malloc`. The pointer then points into the object. A list that outgrows that array moves out of line the same way and never moves back. A reader still holding the inline array reads old values from memory that lives as long as the list.
- **Dict:** insertion-ordered open-addressing table (the CPython 3.6+ layout). Shared dicts take the lock for writes; v1 also locks reads, and a seqlock fast path for reads is a later optimization. **Built:** the seqlock fast path shipped in 0.2.0. A read of a shared dict tries a lock-free read, validated by a sequence counter, and takes the lock only if a writer interfered.
- **String:** immutable UTF-8 with a cached hash. Mutable buffers are a separate `StringBuilder` type, so strings are always frozen and freely shared.

## Memory management

v1 uses a non-moving, parallel, stop-the-world mark-sweep collector with thread-local allocation. Non-moving matters in Rust: raw pointers held across a safepoint stay valid, which removes a whole class of `unsafe` bugs.

**Allocation.** The heap is a set of 256 KB segments, each split into pages of one size class (16, 32, 48 … 2048 bytes; larger objects get their own segment). Each thread owns its current page per size class and bump-allocates from it with no atomics. Only grabbing a fresh page touches a global (sharded) free-page list. **Built:** segments hold eight 32 KB pages, and the 40 size classes step by 16 bytes up to 512, then grow to 2048. Larger objects get their own allocation from the system allocator. The global free-page lists are kept per size class rather than sharded.

**Safepoints.** Every thread polls a global `gc_requested` flag at backward jumps, calls, and allocation slow paths: one load and a predictable branch. **Built:** the poll reads a per-thread flag, at backward jumps, calls and loop heads. Allocation does not poll, so allocating never collects (see Running out of memory). A thread in native code or blocking I/O sets its state to `in_native` first. The collector treats such threads as already stopped, the same trick the JVM and Go use, so a thread stuck in `read()` never delays a collection.

**Roots.** Interpreter frames and registers live in a VM-managed stack of `Value`s, so scanning them is precise and trivial. Rust code that holds `Value`s across a possible GC must root them in a `HandleScope<'a>`. Handles borrow the scope, so the borrow checker stops them from escaping. This is the main safety boundary for native code. Each fiber has its own register stack, scanned with its thread's roots. A minor collection skips a parked fiber whose roots referenced no young object when one last scanned them: only the fiber writes its roots, so until it runs again they reference only old objects, and the remembered set finds the young objects those reference. A fiber that ran is scanned again; so is one whose roots still reached a young object, which can happen because a large object that only roots reference stays young. With 40,000 parked fibers, this takes a thread's minor collections from 323 ms to 9 ms in total.

**Collection cycle.**

1. The requesting thread sets `gc_requested` and waits until every thread is parked or `in_native`.
2. All parked threads, and the collector's helper threads, become mark workers. Each one works from a private stack and every 32 objects moves up to 16 entries to its small crossbeam deque, if that deque is empty, for idle workers to steal. The collecting thread starts with what the roots reach.
3. Marking sets the mark color with an atomic compare-and-swap, and whoever sets it pushes the object. Objects that hold no references (strings, big integers, builders, native handles: `GcVTable::leaf`) are only marked, never pushed. Marking runs when memory is short, so it must not abort for want of memory. A mark stack grows only by a fallible reservation, and if that fails the object is left unmarked and the worker records an overflow. Once the workers are done, the collector traces the roots and every marked object again, alone, until a pass ends without overflow; each pass marks at least what its stack held. A minor collection does the same with its roots, its remembered set (traced in full, since the written-range scan forgets what it visited) and its minor-marked young objects. `CATX_GC_STRESS` caps mark stacks at 64 entries so that the stress runs exercise this path.
4. Threads resume immediately. Sweeping happens lazily: a page is swept the next time a thread wants to allocate from it.
5. Any page still unswept is swept at the start of the next collection. At that point every wholly empty segment is known, and the collector returns them to the OS (`munmap`, or `VirtualFree` on Windows). It keeps enough empty pages for the allocation budget until the following collection, so a program in a steady state does not unmap segments only to map them again.

**Internal errors in a collection.** A Rust panic that unwinds out of a collection (marking, sweeping, or a minor collection's remembered-set scan), on the collecting thread or on a thread helping it mark, ends the process. By then the heap's state is unknown, and in a major collection every other thread is parked waiting for the world to restart. The collector catches the unwind and calls the fatal handler that the standard library installs (`catallaxy_gc::set_fatal_handler`). The handler flushes the program's output, prints the message and exits with status 101; without a handler, the collector prints the message and aborts. Internal errors anywhere else end only their thread, fiber or `par_*` call, and a `par_*` job leaves the pool's roots however its caller exits.

**The heap limit.** The heap is capped at `CATX_MAX_HEAP`, or by default at three quarters of physical memory (`0` turns the cap off). A program that grows without bound should raise a `MemoryError`, not swap the machine: on macOS and on Linux with overcommit, allocations practically never fail. The cap counts the live bytes the last major collection measured, including the buffers of lists, dicts and builders, which their objects' `size` reports. The allocation budget between collections is capped too, so a collection runs before the heap can pass the limit. The buffers themselves (list, dict and set buffers, `StringBuilder` growth, the vectors natives build) count toward that budget, and each buffer growth first checks that the live bytes plus the new buffer stay under the cap. If not, the growth is refused like an allocator refusal: the caller collects and retries where it can, then raises a `MemoryError` that names the cap. `Json.parse` keeps everything it builds rooted and reaches a safepoint every 4,096 values, where the usual collections and over-limit check apply. `Json.stringify` checks its output buffer before it doubles. Garbage allocated since the last collection is not counted against the cap, so it never causes a refusal.

**Running out of memory.** Allocation never fails or collects, so when the OS refuses a new segment the heap takes one from a small reserve of mapped segments (4 MB), hands a 2 MB headroom block back to the OS so the runtime's own allocations can go on, and requests a collection. That collection sweeps at once and refills the reserve from the segments it emptied, or from the OS. If it cannot, the next thread to reach a safepoint panics with a `MemoryError`; only an allocation that finds the reserve used up aborts. The global allocator of `catx` and of the C API libraries (`catallaxy_gc::Allocator`; a Rust embedder opts in with one line) does the same for allocations inside Rust's collections: a refused request releases the headroom block and is retried once, and the collection it schedules decides whether to raise the error. Allocations the runtime makes where it can raise an error (strings, `StringBuilder`, list and dict buffers, register and fiber stacks, and the vectors natives build their results in) check for refusal themselves, and for strings, builders, list pushes and dict inserts collect once and retry first. A native that has built values nothing roots yet (the items of a list it is about to return) cannot collect, so it raises the error at once; natives that allocate much check before they start whether the OS has refused memory since the last collection, and collect first if so. The fallible heap allocations (`LocalHeap::try_alloc_uninit`) do not draw on the reserve: once a segment has been refused they return null for objects that need a new one until the next collection, which leaves the reserve to the allocations that cannot report failure.

**Thread-local young collection.** The ownership rules guarantee that frozen and shared objects never point to owned objects (freezing is deep, and promotion walks the graph). So a thread's owned young objects are reachable only from that thread. That thread can collect its own nursery without stopping anyone, much like OCaml 5's per-domain minor heaps. This was planned as the first major GC upgrade after v1, since it makes most garbage free to collect. **Built** in 0.2.0: each thread has a nursery (4 MiB by default, `CATX_GC_NURSERY`) and collects it on its own. The collection does not move objects: young, remembered and minor-mark bits in the header track generations, and a remembered set records old objects that were written to. Any path that lets another thread or a global reach an object tenures it first. `CATX_GC_GENERATIONAL=0` turns it off. Major collections also gained dedicated marker threads (`CATX_GC_THREADS`), which cut a 1.6 GB heap's longest pause from 143 ms to 25 ms. `docs/gc-minor-cost.md` measures what a minor collection costs.

**Deliberately absent.** No reference counting (atomic refcount traffic is exactly the cost this design avoids), and no general finalizers. Resources close via explicit `close`/`with` blocks, with weak references for caches. **Built:** there are no user-visible finalizers. Sockets, listeners and channels have `close()`; internally, a socket or native handle that dies still releases its OS resource, so a forgotten `close` does not leak a descriptor. **Not built yet:** weak references, and a `with` block for resources (`with` exists only as `with lock(obj)`).

## Execution

The VM is a register-based bytecode interpreter, one instance per OS thread, all sharing frozen code objects and the shared heap. Static types let the compiler resolve most calls and field accesses ahead of time, so the VM does far less dynamic lookup than Ruby or Python.

**Pipeline.** Source → lexer → parser (hand-written, Pratt for expressions) → AST → name resolution → **type checker** (inference, interface checks, capability checks, exhaustiveness) → typed IR → bytecode. Nothing reaches the VM until it type-checks. Compiled functions are frozen `Code` objects, so every thread can run them with no synchronization.

**Bytecode.** 32-bit fixed-width instructions with 8-bit opcode and register operands, in the style of Lua 5.x. Opcodes are typed where the checker knows the types (`add_int`, `lt_float`, `get_field`, `call_static`) and generic only for unions, erased generics and `Dyn`. Dispatch is a `match` in a tight loop, which LLVM turns into a jump table; tail-call threading can be tried later if profiling demands it. **Built:** typed opcodes follow this plan under other names (`AddI`, `AddF`, `LtF`, `GetField`), with A/B/C, Bx and sBx operand forms; a wide operand (a function, class or global id) takes one extra data word. Superinstructions fuse common sequences, such as compare-and-jump and two field reads. **Declined:** tail-call threaded dispatch was 3–19% slower than the `match` loop on aarch64 and worse on x86-64 (`experiments/dispatch`).

**Inline caches, only where needed.** Interface calls and `Dyn` operations get a per-site cache of `(class_id, method_index)` packed into one `AtomicU64`. A reader loads the word once and either hits or takes the slow path. Since the whole entry is one atomic word, a thread can never see a class from one write and a method from another.

**Method dispatch.** Calls on a known class compile to `call_static` (final methods) or a vtable slot (overridable methods). (The instructions are named `Call` and `CallVirtual`; an interface call is `CallMethod`.) Calls through a structural interface look the method up by name in the receiver's class table on the first miss, then hit the inline cache. Extension methods are resolved at compile time and are just static calls. With no runtime class reopening, cached dispatch never needs invalidating.

**Threads.** v1 maps language threads 1:1 onto OS threads.

- `spawn { … }` returns a handle; `join` returns a Result holding the block's value, or the panic that ended the thread.
- Typed channels (bounded and unbounded) are the preferred way to move owned data.
- `par_map`, `par_each` and `par_reduce` on collections run on a shared work-stealing pool, and the compiler checks that the block captures only frozen or shared values.
- Lightweight fibers for I/O-bound work (M:N scheduling) are a phase-4 feature, not v1.

**Built:** threads, channels and `par_*` as described. Fibers shipped in 0.2.0, but not with M:N scheduling: a fiber is a stackful coroutine that stays on the OS thread that started it, and a thread's fibers take turns. Blocking calls go to a pool of I/O threads (`CATX_IO_THREADS`) and sockets to a non-blocking reactor, so one thread can serve thousands of connections. Because a fiber never changes threads, it may capture owned values; the checker keeps a fiber handle from crossing to another thread (E0619).

**Globals and modules.** Module objects are shared. Top-level constants are frozen on assignment. Module-level mutable variables are allowed but are shared objects, so they pay locking costs, which nudges people toward constants and passing state explicitly. **Built differently:** there are no module objects at run time; a module is a compile-time namespace, imported by a path relative to the importing file (reference §2). A module-level `var` must have a frozen or shareable type (E0601), and its slot is one atomic word, so assigning it publishes the value as shared without taking a lock.

## Crate architecture and phases

Seven crates in one Cargo workspace, layered so that all `unsafe` code sits in `runtime` and `gc`, where it can be fuzzed and run under Miri and ThreadSanitizer. **Built:** the memory-management and object-model `unsafe` code is in `gc` and `runtime`. `vm` (the register file and object views), `stdlib` (native object access) and `capi` (the FFI boundary) also use `unsafe`, through those crates' APIs; the compiler and the `catallaxy` crate use none.

| Crate | Role | Depends on |
| --- | --- | --- |
| `catallaxy-gc` | segmented heap, thread-local allocation, safepoints, parallel mark, lazy sweep | — |
| `catallaxy-runtime` | NaN-boxed values, object layouts, classes, ownership states | `gc` |
| `catallaxy-vm` | bytecode format, interpreter, inline caches, program loader | `runtime`, `gc` |
| `catallaxy-compiler` | lexer, parser, type checker, bytecode emitter, formatter | `vm` |
| `catallaxy-stdlib` | native standard library | `vm`, `runtime`, `gc` |
| `catallaxy` | the `catx` CLI, REPL, language server, embedding API | all of the above |
| `catallaxy-capi` | C ABI for embedding and native extensions (`include/catallaxy.h`) | `catallaxy`, `stdlib`, `vm`, `runtime`, `gc` |

The compiler never touches the heap directly; it hands frozen `Code` objects to the VM, which keeps the parser and type checker testable in isolation. The type checker is the largest single module in the project and gets its own test suite of programs that must and must not compile.

**Phases.** Each ends with something runnable and a growing conformance test suite. **Built:** phases 1–4 shipped in 0.1.0, except fibers, which came in 0.2.0 with phase 5: thread-local young collection, seqlock dict reads and superinstructions shipped, and the JIT experiment ended in a "not now" (`experiments/jit`). Later work is tracked in [GitHub milestones](https://github.com/CodingAnarchy/catallaxy/milestones) and `CHANGELOG.md`.

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
- [x] **Native extensions.** Decided: v1 supports Rust extensions only, as ordinary crates against the VM's `HandleScope` API, so the borrow checker enforces rooting and ownership rules. A C ABI may come later as a thin wrapper, once the object model is stable. **Built:** both. The Rust API is `Engine::register` with `HandleScope`, and the C ABI shipped in 0.2.0 as `catallaxy-capi` (`include/catallaxy.h`, reference §11).
- [x] **Build approach.** Decided: the conformance suite comes first (programs that must check, programs that must be rejected with specific error codes, expected outputs) and serves as the definition of done. An agent fleet builds the parser, type checker, compiler, stdlib and tooling phase by phase against it. Code in `runtime` and `gc` must also pass Miri, Loom, ThreadSanitizer and fuzzing gates, plus human review.
- [x] **Name.** Decided: **Catallaxy**, Hayek's and Mises's term for the order that emerges from many independent actors exchanging without a central planner, just as threads here coordinate through ownership and explicit exchange with no global lock. The crate name `catallaxy` was unclaimed on crates.io as of October 12, 2026. Proposed short forms: `catx` for the CLI and `.cx` for source files.
- [x] **Crate name and repository.** Done: the crates are published on crates.io as `catallaxy` and `catallaxy-*`, and the code lives at [github.com/CodingAnarchy/catallaxy](https://github.com/CodingAnarchy/catallaxy). **Not built yet:** a GitHub organization and a domain.
