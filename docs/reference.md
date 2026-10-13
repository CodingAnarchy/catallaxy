# Catallaxy Language Reference (v1)

This is the normative description of Catallaxy v1. `docs/design.md` explains *why*; this file says *what*.
The conformance suite under `tests/conformance` is the executable definition: where the two disagree, the
suite wins and this file gets fixed.

Source files use the `.cx` extension and are UTF-8. The CLI is `catx`.

## 1. Lexical structure

- Comments: `#` to end of line. A file may start with a `#!` shebang line.
- Identifiers: `[A-Za-z_][A-Za-z0-9_]*`. Names starting with an uppercase letter are *type-like*
  (classes, enums, interfaces, variants, constants); others are values (locals, functions, fields, methods).
- Keywords: `and as break class const continue else enum extend false fn for frozen if import in interface
  is let loop match move native nil not or pub return self shared spawn true while with Self`.
- Integer literals: `42`, `1_000_000`, `0xff`, `0b1010`, `0o17`. Arbitrary precision.
- Float literals: `3.14`, `1e10`, `2.5e-3`, `1_000.5`. A float literal always contains `.` or an exponent.
- Strings: `"..."` with escapes `\n \t \r \0 \\ \" \# \u{1F600}` and interpolation `#{expr}`.
  `'...'` strings are raw except for `\'` and `\\`, and never interpolate.
- Statement termination: a newline ends a statement unless (a) the line ends with a binary operator, `,`,
  `=` or `->`, (b) a `(` or `[` is unclosed, or (c) the next non-blank line starts with `.` or `?.`
  (method chains). `;` also separates statements. Newlines are significant inside `{ }`.

## 2. Programs and modules

A file is a module. Its top level holds declarations (`fn`, `class`, `interface`, `enum`, `extend`,
`import`, `const`, `var`) and statements. Top-level statements of the entry file run in order as the
program's main body; top-level statements in an imported module run once, when it is first imported.

```
import geometry                 # binds module `geometry`; use geometry.area(...)
import geometry.{Point, area}   # binds selected public items directly
import std.json                 # standard library modules live under `std`
```

Module paths are resolved relative to the importing file's directory (`a.b` → `a/b.cx`), then the
standard library. Only `pub` items are visible to importers. Cyclic imports are an error.

Locals assigned in a top-level statement are locals of the main body: functions cannot see them.
Module-wide state uses:

- `const NAME = expr` (or `const NAME: T = expr`): a constant. Its value is deeply frozen at assignment.
  Uppercase names are conventional.
- `var name: T = expr`: a module variable, visible to every function in the module (and importers if
  `pub`). Module variables are shared across threads, so `T` must be a frozen or shareable type.

## 3. Types

| Type | Meaning |
| --- | --- |
| `Int` | arbitrary-precision integer |
| `Float` | IEEE 754 double |
| `Bool` | `true` / `false` |
| `String` | immutable UTF-8 text (always frozen) |
| `Nil` | the type of `nil` |
| `List[T]`, `Dict[K, V]`, `Set[T]` | mutable, owned collections |
| `(A, B, ...)` | tuples (immutable); `()` is not a type, use `Nil` |
| `fn(A, B) -> R` | function / closure; `fn(A)` returns `Nil` |
| `T?` | `T` or `nil` |
| `A \| B` | union |
| `A & B` | intersection of interfaces |
| `{ fn m(self) -> R }` | inline structural interface |
| `frozen T` | deeply immutable view of `T` |
| `Range` | integer range (`1..5` inclusive, `1...5` exclusive) |
| `Dyn` | dynamically checked value |
| `Never` | type of expressions that never produce a value (`return`, `panic(...)`) |
| `Mutex[T]`, `Atomic[T]`, `Channel[T]`, `Thread[T]` | concurrency types |
| `Result[T, E]` | `Ok(T)` or `Err(E)` |
| `StringBuilder` | mutable string buffer |
| `NativeHandle` | opaque frozen native data (inside `Regex`, `Subprocess`; see §11) |

Generic type arguments use square brackets. Plain types never hold `nil`.

### 3.1 Subtyping and assignability

- `Never` is assignable to everything; everything is assignable to `Dyn`.
- `T` is assignable to `T?` and to any union containing it; `nil` to any `T?`.
- A subclass is assignable to its superclass.
- A type is assignable to an interface if it has every required method with a compatible signature
  (structural). Interfaces never need an `implements` clause.
- Function types are contravariant in parameters and covariant in results.
- Generic classes are invariant: `List[Int]` is not a `List[Int?]`.
- `Int` converts implicitly to `Float` only in arithmetic with a `Float` operand.
- `frozen T` and `T` are distinct; `freeze(x: T) -> frozen T`. Frozen types allow only non-mutating methods.
  `Int`, `Float`, `Bool`, `Nil`, `String`, `Range` and tuples/enums of frozen types are already frozen.

### 3.2 Inference

Local variables, block parameters and generic arguments are inferred. Annotations are required on:
`pub` function parameters and results, class fields, interface members, and `var` declarations.
Non-`pub` functions may omit result types (inferred from the body; a recursive function needs one) and
may omit parameter types entirely (implicit generics, §6.3). An empty literal (`[]`, `[:]`, `nil`) takes
its type from later uses within the same function, or from an annotation: `xs: List[Int] = []`.

## 4. Expressions and statements

Everything is an expression. A block `{ ... }` evaluates to its last expression; a function returns its
body's last value unless it uses `return`.

### 4.1 Variables

```
x = 1               # declares x: Int (first assignment declares)
x = x + 1           # reassignment must keep the declared type
y: Float? = nil     # explicit annotation
let (a, b) = pair   # tuple destructuring
```

Assignment operators: `= += -= *= /= %= ||= ??=`. Variables are block-scoped.

### 4.2 Operators (lowest to highest precedence)

| Prec | Operators | Assoc |
| --- | --- | --- |
| 1 | `??` | right |
| 2 | `or` `\|\|` | left |
| 3 | `and` `&&` | left |
| 4 | `not` `!` (prefix) | — |
| 5 | `==` `!=` `<` `<=` `>` `>=` `<=>` `is` `in` | none |
| 6 | `..` `...` | none |
| 7 | `\|` `^` | left |
| 8 | `&` | left |
| 9 | `<<` `>>` | left |
| 10 | `+` `-` | left |
| 11 | `*` `/` `%` | left |
| 12 | `-` (unary) `~` | — |
| 13 | `**` | right |
| 14 | postfix: call `()`, index `[]`, `.field`, `?.field`, `!` (force unwrap), `?` (try), `as?` | left |

- Integer `/` and `%` floor (like Ruby): `-7 / 2 == -4`, `-7 % 2 == 1`. Division by zero panics.
- `==` is structural for `Int Float Bool Nil String`, tuples, lists, dicts, sets and enums; identity for
  class instances unless the class defines `==`.
- `<=>` returns `-1`, `0` or `1`.
- `a in b` calls `b.contains(a)`.
- `x ?? d` is `x` unless nil. `x?.m` short-circuits to `nil`. `x!` panics on nil.
- `e?`: on `Result[T, E]`, unwraps `Ok` or returns `Err` from the enclosing function (whose result must be
  a `Result` with a compatible error type); on `T?`, unwraps or returns `nil` (enclosing result must be
  optional).
- `e as? T` returns `T?` (runtime check; mainly for `Dyn` and unions).
- Operators on class instances call methods named by the operator: `fn +(self, o: Vec) -> Vec`,
  `fn ==(self, o: Self) -> Bool`, `fn <=>(self, o: Self) -> Int`, `fn [](self, i: Int) -> T`,
  `fn []=(self, i: Int, v: T)`. Comparison operators on a type with `<=>` come from `Comparable`.

### 4.3 Control flow

```
if c { a } else if d { b } else { e }      # expression; without else, type is T?/Nil
if let u = maybe_user { ... } else { ... } # unwraps T?
if let Ok(v) = res { ... }                 # pattern form
while c { ... }
loop { ... break }                         # `break value` makes `loop` an expression
for x in xs { ... }                        # any Enumerable, Range, Dict ((k, v) tuples), String (chars)
for (i, x) in xs.each_with_index() { ... }
break; continue; return expr
match v { pattern => expr, ... }           # exhaustive
with lock(shared_obj) { ... }              # lock one shared object for the block
```

Conditions must be `Bool` (no truthiness). Inside an `if`/`while`/`for`/`match` header, a trailing
block cannot follow a call without parentheses: write `if xs.any() { ... }` not `if xs.any { |x| ... } {`.

### 4.4 Patterns

| Pattern | Matches |
| --- | --- |
| `_` | anything |
| `name` | anything, binds it (lowercase) |
| `1`, `"s"`, `true`, `nil` | literal equality |
| `1..5` | integer range |
| `Shape.Circle(r)` / `.Circle(r)` | enum variant, binding payload positionally |
| `Ok(v)`, `Err(e)` | `Result` variants |
| `(a, b)` | tuple |
| `Int`, `s: String` | type test on a union / `Dyn` / supertype, optionally binding the narrowed value |
| `p1 \| p2` | either (no bindings) |
| `pattern if guard` | guard |

Narrowing: after `if x != nil`, `if x is T`, `if let`, and in `match` arms on a variable subject, the
variable has the narrowed type for the rest of that branch.

### 4.5 Functions, closures, blocks

```
fn add(a: Int, b: Int) -> Int { a + b }
pub fn greet(name: String, greeting: String = "Hello") -> String { "#{greeting}, #{name}" }
add(1, 2); greet("Ann", greeting: "Hi")      # positional then named arguments
sq = fn(x: Int) -> Int { x * x }             # anonymous function
inc = { |x: Int| x + 1 }                      # closure literal
xs.map { |x| x * 2 }                          # trailing block = last argument
xs.reduce(0) { |a, b| a + b }
xs.each { puts(it) }                          # `it` is the sole parameter of a param-less block
```

Calls always use parentheses, except that a zero-argument method may be called as `obj.name`
(`s.upcase`, `xs.length`); fields and zero-argument methods share one namespace. `return` inside a
block or anonymous function returns from that block. Closures capture variables by reference.

### 4.6 Collection literals

- `[1, 2, 3]` is a `List`; `[]` an empty list (its element type is inferred from later use).
- `["a": 1, "b": 2]` is a `Dict`; `[:]` an empty dict. A bare identifier before `:` is a **string
  key**: `[host: "x"]` is `["host": "x"]`, even if a local named `host` exists. Use parentheses for a
  computed key: `[(host): 1]`.
- `(1, "a")` is a tuple; `(x,)` a one-element tuple.
- Dicts and sets iterate in insertion order.

## 5. Classes, interfaces, enums, extensions

```
class Point {
  x: Float
  y: Float = 0.0                         # default value
  fn norm(self) -> Float { (self.x * self.x + self.y * self.y).sqrt }
  fn scale(self, k: Float) { self.x *= k; self.y *= k }   # mutating method (inferred)
  fn origin() -> Point { Point.new(x: 0.0) }              # static (no self)
}
p = Point.new(x: 3.0, y: 4.0)            # generated constructor: fields in order, named or positional
```

- A class may define `fn init(self, ...)`; then `Class.new(...)` takes `init`'s parameters, and `init` must
  assign every field that has no default.
- `class Admin < User { ... }`: single inheritance; `super.method(...)` calls the parent version.
  Methods may be overridden with a compatible signature. Fields are inherited.
- `frozen class P { ... }`: instances are deeply immutable (field types must be frozen); `P` is frozen.
- `shared class C { ... }`: instances may be used from any thread; field types must be frozen or
  shareable; each field read/write is atomic. `with lock(c) { ... }` makes a compound update atomic.
- Generic classes: `class Stack[T] { items: List[T] = [] }`.
- A static `fn new` declared in a class (other than a `shared class`) replaces the generated
  constructor. The standard library's `Regex.new` (which returns a `Result`) works this way.
- `Self` names the enclosing class/interface type.

```
interface Shape {
  fn area(self) -> Float
  fn describe(self) -> String { "shape with area #{self.area}" }   # default method
}
interface Enumerable[T] { fn each(self, f: fn(T)) ... }            # (prelude)
```

Any type that has all required methods satisfies an interface and gains its default methods, even when
called directly on the concrete type. If two in-scope interfaces provide the same default method to one
type, the call is ambiguous and must be resolved by defining the method.

```
enum Shape {
  Circle(r: Float)
  Rect(w: Float, h: Float)
  Empty
}
s = Shape.Circle(2.0)
area = match s { .Circle(r) => 3.14 * r * r, .Rect(w, h) => w * h, .Empty => 0.0 }
```

Enums may be generic and have methods. Enum values are immutable.

`extend T { fn ... }` adds methods to an existing type (`String`, `List[T]`, a user class). Extensions
resolve statically and are visible only in modules that declare or import them (`import m` brings
`m`'s `pub extend` blocks into scope).

## 6. Generics

```
fn max[T: Comparable](xs: List[T]) -> T? { ... }
pub fn total(xs: Enumerable[Int]) -> Int { xs.reduce(0) { |a, b| a + b } }   # interface param = bound
```

A member reached through a bound `T: I` (or `T: I & J`) is seen with `Self` replaced by `T`, so with
`interface Clone { fn clone(self) -> Self }`, `fn dup[T: Clone](x: T) -> T { x.clone() }` checks.

### 6.3 Implicit generics

A non-`pub` function may leave parameter types off. The checker infers a structural requirement for each
such parameter from how the body uses it, and checks every call site against it.

```
fn shout(x) { x.name.upcase }      # x: { fn name(self) -> { fn upcase(self) -> R } }
```

Generics are erased: one compiled body serves every instantiation.

## 7. Errors

- Recoverable errors use `Result[T, E]`, `Ok(v)`, `Err(e)` and `?`. There are no exceptions.
- `panic(msg)`, failed `assert(cond, msg)`, out-of-bounds indexing, `x!` on nil, integer division by zero
  and ownership violations panic. A panic unwinds only the current thread. In the main thread it prints the
  message and a stack trace to stderr and exits with status 101.
- `Thread.join` returns `Result[T, String]`: `Err` carries the panic message.

## 8. Concurrency

Every type has a capability:

| Capability | Types | Crosses threads |
| --- | --- | --- |
| frozen | `Int Float Bool Nil String Range NativeHandle`, `frozen T`, `frozen class` (e.g. `Regex`), enums/tuples of frozen | freely |
| shareable | `Mutex[T] Atomic[T] Channel[T] Thread[T]`, `shared class` | by reference |
| owned | everything else (`List`, `Dict`, ordinary classes, closures) | only by `move` |

```
t = spawn { heavy(42) }                  # captures must be frozen/shareable
t2 = spawn move { process(data) }        # `move` transfers captured owned values; using `data` afterwards is an error
r = t.join()                             # Result[T, String]
ch: Channel[List[Int]] = Channel.new()   # unbounded; Channel.new(capacity: 8) is bounded
ch.send(move batch)                      # owned values are moved through channels
m = Mutex.new(move counts)               # m.lock { |c| c["a"] = 1 }
a = Atomic.new(0); a.add(1); a.get
squares = nums.par_map { |n| n * n }     # par_map / par_each / par_reduce: element and capture types must be frozen/shareable
```

Captured variables inside `spawn` and `par_*` blocks are copied (they cannot be reassigned there).
`freeze(x)` freezes the object in place, so `x` itself has the frozen type afterwards (until it is
reassigned); moving a value (`move x`, `spawn move`) makes `x` unusable until reassigned. Both apply
to every local the compiler knows refers to the same object (`b = a`). Aliases it cannot see, such as
an object also stored in a field or list, are caught at run time by the ownership checks below.
At runtime every object is owned (by one thread), frozen or shared. Touching another thread's owned
object panics with `OwnershipError`; under `catx run --promote-on-access` the object is promoted to
shared instead and a warning is logged. Single operations on shared objects are atomic; compound ones
(`d[k] += 1`) need `Mutex`, `Atomic` or `with lock`.

### 8.1 Atomics and channels

`Atomic.add(n)` returns the new value. `Channel.recv()` blocks until a value arrives and returns
`nil` once the channel is closed and drained; `for x in ch` iterates until then.

### 8.2 Fibers

A fiber is a lightweight cooperative thread for I/O-bound work. Thousands can run on one OS thread.

```
fs = urls.map { |u| Fiber.start { fetch(u) } }     # fetch waits on files, channels, sleep...
pages = fs.map { |f| f.join().unwrap() }
```

- `Fiber.start { ... }` creates a fiber; `join()` waits for it and returns `Result[T, String]`
  (`Err` holds the message of a panic, which ends only that fiber); `is_done()`;
  `Fiber.yield()` lets the thread's other fibers run.
- A fiber runs on the OS thread that started it, for its whole life. Fibers on one thread
  interleave only where one of them waits: channel `send`/`recv`, `join`, `sleep`, socket,
  file, directory, process and console I/O, HTTP requests, `Fiber.yield()`. Their blocks may therefore capture and mutate owned values (no
  capability rules apply), and code between two waits runs without interruption.
- Waiting parks the fiber instead of blocking the thread. Blocking system calls (files,
  directories, stdin, starting and waiting for processes and reading or writing their pipes)
  run on a shared pool of I/O threads (`CATX_IO_THREADS`, default 16) meanwhile.
- A fiber does not park while it holds a lock keyed by its OS thread (inside `with lock` or a
  `Mutex` closure): it blocks the thread instead, so its sibling fibers cannot enter the lock.
- A thread, and the program, finishes only after the fibers it started. A fiber is joined on
  the thread that started it. Its result is stored in the `Fiber` handle, so a result nobody
  joins is freed with the handle.
- Keywords are valid method names after `.` and in method declarations (`obj.match(x)`).

### 8.3 Networking

`Net.connect(host, port)` and `Net.listen(host, port)` (port 0 picks a free one; see
`listener.port`) give `Socket`s and `Listener`s with `read`, `read_line`, `write`, `accept`,
`set_timeout` and `close`; failures, including timeouts, are `Err` values. A socket operation
that would block parks the calling fiber, so one OS thread serves thousands of connections:
sockets are non-blocking and a single reactor thread wakes waiting fibers (or, outside fibers,
waiting threads). `catx` raises its open-file limit at startup so servers are not capped at the
OS default. `Http.get` and `Http.post` (HTTP and HTTPS) return an `HttpResponse` (`status`,
`headers` with lowercase names, `body`); they run on the I/O pool.

## 9. Dyn

`Dyn` values come from `Json.parse` and other dynamic sources. Any operation on `Dyn` (`d["k"]`, `d[0]`,
`d.method(...)`, arithmetic) is checked at runtime and yields `Dyn`. Convert with `as?`:
`name = (d["name"] as? String) ?? "anon"`. Every typed value converts implicitly to `Dyn`. JSON objects parse to records that behave exactly like `Dict[String, Dyn]` (`as? Dict[String, Dyn]` succeeds); records parsed with the same keys share a hidden class, so `d.name` is served from a per-site cache. `Json.stringify` writes object keys in sorted order and integers of any size (including big integers) as JSON numbers.

## 10. Tooling

`catx run file.cx [args]`, `catx check [--json] files...`, `catx fmt [--check] files...`, `catx repl`,
`catx lsp`, `catx test dir`.

`catx test dir` runs every `*.cx` file under `dir` (except those in `lib/` directories, which hold
modules for tests to import) in its own process. A file whose first line is `# expect: CODE[, CODE...]`
must fail `catx check` with those diagnostic codes. Any other file must run with standard output
equal to its `.out` file and exit with status 0, or with the status given by a `# exit: N` line; a
`.in` file next to it is fed to standard input. A program still running after `CATX_TEST_TIMEOUT`
seconds (default 120) is killed and fails with its output so far. The exit status is 1 if anything
failed.

`catx fmt` rewrites files in place (`catx fmt -` filters stdin to stdout). It keeps comments and the
program's line structure; it re-indents with two spaces, normalizes spacing between tokens, and breaks a
`{ ... }` block across lines when its line would exceed 100 columns. `--check` lists the files that
would change and exits with status 1 if there are any. Files with syntax errors are left untouched.

`catx lsp` is a language server over stdio. Open buffers overlay the file system, so imports see
unsaved edits. It provides:

- diagnostics (published on open, change and save), hover (types, signatures), go to definition,
  completion (members after `.`, names in scope, keywords) and document formatting (as `catx fmt`);
- find references and rename (with prepare-rename) for locals, parameters (including named
  arguments), functions, methods (an override and the method it overrides rename together), fields,
  classes, enums, variants, interfaces and constants. They cover the document and every module it
  imports; names from the prelude cannot be renamed;
- signature help for function, method and constructor calls, showing default values and marking
  the trailing-block parameter, with the active parameter following commas and named arguments;
- inlay hints with the inferred types of unannotated variables, `for`/`let`/`if let` bindings and
  block parameters, and the inferred requirement of each untyped parameter of an implicit-generic
  function (`fn shout(x)` shows `x: { fn upcase(self) -> _ }`);
- document symbols (an outline of items, members and top-level variables) and workspace symbols
  (over the open documents and the modules they import).

`check --json` prints `{"diagnostics": [...]}` where each diagnostic has `code`, `severity`, `message`,
`file`, `span` (`start`/`end` as `{line, col, offset}`, 1-based lines and columns), `notes`, and `fixes`
(each a list of `{span, replacement}` edits).

## 11. Embedding and native extensions

Native functions are written in Rust against the `catallaxy` crate and declared in Catallaxy with
`native fn` (a top-level `native fn name(...)` is registered as `name`; one inside `class C` as
`C.name`):

```rust
use catallaxy::{native, Engine, RtResult, Value, Vm};

fn apply_twice(vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
    let (f, x) = (args[0], args[1]);
    let scope = vm.handle_scope();                // roots values across calls that may collect
    let first = scope.root(native::call(vm, f, &[x])?);
    let second = native::call(vm, f, &[scope.get(first)])?;
    Ok(native::new_list(vm, vec![scope.get(first), second]))
}

fn main() {
    let mut engine = Engine::new();
    engine.register("apply_twice", apply_twice);
    std::process::exit(engine.run_file("app.cx".as_ref()).unwrap_or(2));
}
```

```
native fn apply_twice(f: fn(Int) -> Int, x: Int) -> List[Int]
puts(apply_twice({ |v| v * 10 }, 3))     # [30, 300]
```

Rules for native code: values held across a call back into the VM (`native::call`, or anything that
runs Catallaxy code) must be rooted in a `HandleScope`; handles borrow the scope, so they cannot
escape it. Allocation alone never collects. Missing optional arguments arrive as the empty marker
(`Value::is_empty_marker`). A process runs one program.

`native::new_handle(vm, data)` wraps Rust data (`Send + Sync`) in a `NativeHandle`, and
`native::handle::<T>(v)` gets it back. A handle is frozen, so Catallaxy code may keep it in a
`frozen class` and pass it between threads: the data must be immutable or synchronize itself
(the standard library's `Regex` holds its compiled regex this way). It is dropped when the
collector frees the handle.

### C ABI

The `catallaxy-capi` crate builds a static and a shared library (`libcatallaxy_capi`) with the C
interface declared in `crates/catallaxy-capi/include/catallaxy.h`: register natives
(`cx_register`), run programs (`cx_run_file`, `cx_run_source`), build and inspect values
(`cx_int`, `cx_string`, `cx_list`, `cx_to_int`, `cx_string_data`, ...), call closures (`cx_call`),
root values across calls (`cx_scope_open`, `cx_root`, `cx_rooted`, `cx_scope_close`) and raise
panics (`cx_panic`). A native has the signature

```c
cx_status native(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user);
```

and is declared in Catallaxy with `native fn` exactly like a Rust one. The rooting rule is the same
as for Rust natives, but unchecked: a value held across `cx_call` must be rooted. Up to 256 C
natives can be registered per process. `crates/catallaxy-capi/examples/ext.c` is a complete
example.

