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
  `'...'` strings are raw except for `\'` and `\\`, and never interpolate. `#{expr:spec}` formats
  the value with a format spec (section 4.7).
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
import lib.shapes               # lib/shapes.cx, bound as `shapes`
import shapes.geometry as geo   # shapes/geometry.cx, bound as `geo`
```

Module paths are resolved relative to the importing file's directory (`a.b` → `a/b.cx`), then the
entry file's directory. A module that cannot be found is error E0204. Only `pub` items are visible to
importers. Cyclic imports are an error. A module is bound to the last segment of its path unless
renamed with `as`; two imports may not bind the same name to different modules or items (E0206), so
`import a.util` and `import b.util` need an `as` on one of them.

The standard library (`docs/stdlib.md`) needs no import: `Json`, `File`, `Regex`, `Time` and the rest
are in scope in every module, and there are no `std.*` modules to import. `Time.now()` (seconds,
`Float`) and `Time.millis()` (milliseconds, `Int`) are wall-clock time since the Unix epoch.
`Time.monotonic()` (seconds, `Float`) and `Instant` count from the start of the program and never go
backwards: use them to measure durations, not to tell the date (§3.3).

Locals assigned in a top-level statement are locals of the main body: functions cannot see them.
Module-wide state uses:

- `const NAME = expr` (or `const NAME: T = expr`): a constant. Its value is deeply frozen at assignment.
  Uppercase names are conventional.
- `var name: T = expr`: a module variable, visible to every function in the module (and importers if
  `pub`). Module variables are shared across threads, so `T` must be a frozen or shareable type.

## 3. Types

| Type | Meaning |
| --- | --- |
| `Int` | arbitrary-precision integer (so there is no `Int.MAX` or `Int.MIN`: start a running extreme from `nil` or the first element, or use `min`/`max`/`min_by`/`max_by`) |
| `Float` | IEEE 754 double |
| `Bool` | `true` / `false` |
| `String` | immutable UTF-8 text (always frozen); `s[i]` and slices index characters, in O(1) for ASCII text |
| `Nil` | the type of `nil` |
| `List[T]`, `Dict[K, V]`, `Set[T]`, `Deque[T]` | mutable, owned collections (`Deque` is a ring buffer: O(1) at both ends) |
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
| `Bytes` | immutable byte string (always frozen); build from `List[Int]`, text, hex, base64 or a file |
| `NativeHandle` | opaque frozen native data (inside `Regex`, `Subprocess`; see §11) |
| `Duration`, `Instant`, `DateTime` | time spans, monotonic clock readings, dates (frozen; §3.3) |

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

### 3.3 Time

The prelude's time types are frozen classes, so they cross threads freely:

- `Duration`: a signed span in nanoseconds. `Duration.from_secs(90)` (also `from_nanos`,
  `from_micros`, `from_millis`, `from_mins`, `from_hours`, `from_days`, `from_secs_f(1.5)`);
  `+`, `-`, `* Int`, `/ Int`, comparisons; `as_secs`, `as_millis`, `as_secs_f`, ...; `to_s` gives
  `1h2m3.5s`, `250ms`.
- `Instant`: a reading of the monotonic clock. `start = Instant.now()`, `start.elapsed()`,
  `b.since(a)`, `+`/`-` a `Duration`.
- `DateTime`: an instant with a fixed UTC offset (seconds east), on the proleptic Gregorian
  calendar. `DateTime.now()` (at the OS's local offset), `DateTime.now_utc()`,
  `DateTime.of(2024, 2, 29, 13, 5, offset: 3600)`, `DateTime.from_unix(secs)`,
  `DateTime.parse("2024-02-29T13:05:00+01:00")` (ISO 8601 / RFC 3339; a `Result`). Fields
  `year month day hour minute second nanosecond weekday` (0 = Sunday) `yday offset unix`;
  `+`/`-` a `Duration`, `b.since(a)`; `==` and `<=>` compare instants, whatever the offsets;
  `to_utc`, `to_local`, `with_offset`; `format("%Y-%m-%d %H:%M")` (`strftime` directives);
  `to_s` is RFC 3339.

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
- Unary minus binds looser than postfix operators, including method calls: `-5.abs + 1` is
  `-(5.abs) + 1`, which is `-4`. Write `(-5).abs + 1` for `6`.
- `==` is structural for `Int Float Bool Nil String`, tuples, lists, dicts, sets and enums; identity for
  class instances unless the class defines `==`.
- `<=>` returns `-1`, `0` or `1`.
- `a in b` calls `b.contains(a)`.
- `x ?? d` is `x` unless nil. `x?.m` short-circuits to `nil`. `x!` panics on nil.
- `e?`: on `Result[T, E]`, unwraps `Ok` or returns `Err` from the enclosing function (whose result must be
  a `Result` with a compatible error type); on `T?`, unwraps or returns `nil` (enclosing result must be
  optional). In a function that returns a `Result`, convert an optional first:
  `x.ok_or(error)?` or `x.ok_or_else { error }?` (`T?.ok_or(e: E) -> Result[T, E]`).
- `e as? T` returns `T?` (runtime check; mainly for `Dyn` and unions).
- A field or method of a union value (`x.n`, `x.m(a)` with `x: A | B`) must exist on every member type;
  otherwise it is E0311, naming the members that lack it. A field's type is the union of the members'
  field types, and a call's result is the union of the members' results. The call is dispatched on the
  runtime value and passes its arguments by position, so each member's method must take the same
  parameters (names, in order) with a default for each omitted argument (E0332), and each argument must
  suit every member's parameter type (E0300). Members whose class lacks the method at runtime (they
  reach it as an interface default or extension method) must all reach the same function (E0311).
- Operators on class instances call methods named by the operator: `fn +(self, o: Vec) -> Vec`,
  `fn ==(self, o: Self) -> Bool`, `fn <=>(self, o: Self) -> Int`, `fn [](self, i: Int) -> T`,
  `fn []=(self, i: Int, v: T)`. Comparison operators on a type with `<=>` come from `Comparable`.
- `Int`, `Float`, `String` and classes with `<=>` are `Comparable`; tuples and lists are `Comparable` when
  their elements are, and order lexicographically (`(1, "b") < (1, "c")`, `[1, 2] < [1, 2, 0]`). Sorting
  needs it: `sorted`, `sort`, `sort_in_place`, `min` and `max` require `Comparable` items, and `sort_by`,
  `min_by` and `max_by` a `Comparable` key (`recs.sort_by { |r| (r.last, r.first) }`). Use `sort_with`
  for any other order.

### 4.3 Control flow

```
if c { a } else if d { b } else { e }      # expression; without else, type is T?/Nil
if let u = maybe_user { ... } else { ... } # unwraps T?
if let Ok(v) = res { ... }                 # pattern form
while c { ... }
loop { ... break }                         # `break value` makes `loop` an expression
for x in xs { ... }                        # any Enumerable, Range, Dict ((k, v) tuples), String (chars)
for (x, i) in xs.with_index() { ... }      # element and its position
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
| `Int`, `s: String`, `b: Box` | type test on a union / `Dyn` / supertype, optionally binding the narrowed value (see below) |
| `p1 \| p2` | either (no bindings) |
| `pattern if guard` | guard |

Type tests (`x is T`, `x as? T`, and type patterns) check the value's run-time type. Type arguments are
erased at run time: a test for `Box[String]` can check only that the value is a `Box`, a function type
only that it is a function, and a type parameter nothing. Such a test is an error (E0374) unless the
static type already determines the unchecked part (`x: Box[Int] | Foo` may be tested for `Box[Int]`).
A generic type named without arguments is the erased test (`x is Box`, `x as? Box`, `b: Box =>`,
`Box =>`): it narrows to the members of the static type that are a `Box` (with their own arguments), or
to `Box[Dyn]` when the static type does not say (`Dyn`, an interface, a superclass). `List`, `Set`,
`Dict` and tuple types are checked element by element, so `x is List[Int]` is allowed on any value.
A variant pattern (`Ok(v)`) binds its payload with every instantiation the subject may have: on
`Result[Int, String] | Result[String, Int]`, `v` is an `Int | String`, and on a `Dyn` it is a `Dyn`.

Narrowing: after `if x != nil`, `if x is T`, `if let`, and in `match` arms on a variable subject, the
variable has the narrowed type for the rest of that branch. `x ??= v` and `x = v` with a non-nil `v` also
narrow `x` to non-nil.

Each branch of an `if` / `else if` chain, an `if let` or a `match` starts from the narrowings in effect
before it, so an assignment in one branch does not affect the others. After it, a variable (or field
path) is narrowed only if it is narrowed at the end of every branch that falls through (a missing `else`
falls through with the condition false), to the union of those types; a branch that leaves with
`return`, `break`, `continue` or `panic(...)` does not count. So `if x == nil { return }` and
`if x == nil { x = 0 }` both leave `x` non-nil, and `if c { x = nil }` leaves it possibly nil. A `match`
guard that fails has already run when the next arm is tried.

A loop body runs again after itself, so a narrowing from before a loop holds in the loop (condition
included) only if every assignment to the variable in the loop visibly keeps it: `x -= 1`, `x = x * 2`,
`x ??= 0`, a literal, `T.new(...)`, `v!` or a call of a function whose declared result fits. Otherwise,
as for `x = maybe()` or an assignment inside a closure, the narrowing does not apply in the loop or after
it, and the body must check `x` again. A narrowing the loop keeps also holds after the loop. In the rare
case where a kept narrowing is broken by the checked type of an assignment (an operator method that
returns an optional, say), the assignment is E0324.

A closure (a `fn` literal, a block passed to a call, a `Fiber.start` block, the body of a `for` over a
user `Enumerable`) may be stored and run during any later call, so a narrowing of a variable that some
closure assigns ends at the next call, wherever the closure is written. Inside a closure, a captured
variable that is assigned anywhere else in its function keeps no narrowing from outside the closure,
and a nil test inside the closure holds only until the next call (another fiber may run meanwhile).
When the narrowing must outlive a call, bind the value: `if let v = x { ... }`.

A field path on a local or `self` (`n.next`, `self.head.next`) is narrowed by `!= nil` / `== nil` tests
and by assigning it a non-nil value, as long as nothing could have changed it since: the narrowing ends at
any assignment to a field of that name, a reassignment of the root variable, any call (unless every field
on the path belongs to a `frozen class` or is read through a frozen value), and it does not reach into
closures or across loop iterations that might change it. Fields of shared classes are never narrowed.
When it does not apply, bind the value: `if let next = n.next { ... }`.

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
(`s.upcase`, `xs.length`); fields and zero-argument methods share one namespace. `obj.f(x)` calls the
method `f`; to call a function stored in a field, wrap the callee in parentheses: `(obj.f)(x)`. Any
parenthesized callee (`(xs[0])(x)`, `(make())(x)`) is evaluated first and then called. Closures capture
variables by reference.

A bare `{ ... }` in value position is a block that runs immediately, so `inc = { count += 1 }` would
store `nil`; that is error E0372. Write `inc = { || count += 1 }` or `fn() { count += 1 }` for code to run
later.

`return` inside a closure block (`{ |x| ... }`, `{ ... }` passed to a call) or an anonymous function
returns from that block only, like `next` in Ruby, not from the enclosing function. Bodies of `if`,
`while`, `for`, `loop` and `match` are not closures, so `return` there leaves the function:

```
fn first_big(xs: List[Int]) -> Int? {
  xs.each { |x| if x > 10 { return } }    # ends this call of the block; the loop goes on
  for x in xs { if x > 10 { return x } }  # returns from first_big
  nil
}
```

### 4.6 Collection literals

- `[1, 2, 3]` is a `List`; `[]` an empty list (its element type is inferred from later use).
- `["a": 1, "b": 2]` is a `Dict`; `[:]` an empty dict. A bare identifier before `:` is a **string
  key**: `[host: "x"]` is `["host": "x"]`, even if a local named `host` exists. Use parentheses for a
  computed key: `[(host): 1]`.
- `(1, "a")` is a tuple; `(x,)` a one-element tuple.
- Dicts and sets iterate in insertion order.
- Dict and set keys are hashed by value, so they must not change once inserted: a `Dict` or `Set` whose
  keys are owned lists, dicts or sets (also inside a tuple) is error E0373. Use frozen keys
  (`Set[frozen List[Int]]`, filled with `freeze(k)`) or tuples.

Indexing a `List` or `String` with a range slices it: `xs[1..2]` and `xs[1...3]` are new lists of the
elements at those positions, `"hello"[1..3]` is `"ell"` (positions count characters). Negative positions
count from the end (`xs[-2..-1]` is the last two elements), and a range reaching past either end is
clipped, so a slice never panics: `[1, 2][5..6]` is `[]`.

### 4.7 String formatting

An interpolation may end with `:` and a format spec: `"#{price:>10,.2f}"`. The spec runs to the
closing `}`; the expression ends at the first `:` outside brackets (no expression continues with a
top-level `:`, so named arguments, dict literals and typed block parameters are unaffected).

```
spec      := [[fill] align] [sign] ["#"] ["0"] [width] [grouping] ["." precision] [type]
align     := "<" left | ">" right | "^" center | "=" pad after the sign
sign      := "+" always | "-" negatives only (default) | " " space for non-negatives
grouping  := "," every 3 digits | "_" every 3 decimal or 4 hex/octal/binary digits
type      := s d x X o b e E f F %
```

| Type | Applies to | Output |
|---|---|---|
| none | anything | `Int` as `d`; `Float` as its `to_s`, or as `f` when a precision is given; others via `to_s` |
| `s` | anything | `to_s` |
| `d` | `Int` | decimal |
| `x` `X` `o` `b` | `Int` | hex (lower/upper), octal, binary; `#` adds `0x`/`0X`/`0o`/`0b` |
| `f` `F` | `Int`, `Float` | fixed point, precision 6 by default |
| `e` `E` | `Int`, `Float` | scientific, `1.234500e+03` |
| `%` | `Int`, `Float` | times 100, fixed point, then `%` |

Numbers align right and text left by default. `0` before the width pads with zeros after the sign
(`"#{-42:06}"` is `-00042`). For text, the precision is a maximum length in characters. Fill may be
any character except `{`, `}` and `"`. Examples: `"#{3.14159:.2f}"` is `3.14`, `"#{1234567:,}"` is
`1,234,567`, `"#{255:#06x}"` is `0x00ff`, `"#{"ab":*^6}"` is `**ab**`, `"#{0.25:.0%}"` is `25%`.

The spec is checked against the expression's static type at compile time: a malformed spec is error
`E0110`, a spec that does not apply to the type (`.2f` on a `String`, `x` on a `Float`, a precision
or `+` on an `Int` or `String` respectively) is `E0380`. For `Dyn`, generic and union values the same
check runs when the string is built, and a mismatch panics with `FormatError`. The spec is parsed
once by the compiler; at run time formatting is a single instruction.

`template.format(args)` does the same at run time: `"{} is {:>6.1f}".format([name, x])`. `{}` takes
the next argument's `to_s`, `{:spec}` formats it, `{{` and `}}` are literal braces; a bad spec or an
argument count mismatch panics with `FormatError`. `Int.to_s(base)` writes an integer in base 2 to
36.

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
  Methods may be overridden with a compatible signature: the same number of parameters (E0226), each
  accepting the overridden parameter's type, a result that fits the overridden result, a default
  wherever the overridden method has one, the same type parameters with no stronger bounds, and no
  parameter renamed to another parameter's name (E0227). A call through the superclass type may run the
  override, so an override that mutates `self` makes the overridden method mutating. Fields are inherited.
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
called directly on the concrete type. An interface whose methods take `Self` as a parameter (or inside a
generic type, as in `List[Self]`), such as `Comparable`, can only be a bound (`[T: Comparable]`): two
values of the interface type may have different implementing types, so it is not a value type (E0375).
Interfaces that use `Self` only in results (`fn copy(self) -> Self`) are. If two in-scope interfaces provide the same default method to one
type, the call is ambiguous and must be resolved by defining the method.

`Enumerable`'s `map`, `select` and the like are eager and return lists. `xs.lazy()` returns a
`Lazy[T]`, whose adaptors (`map`, `flat_map`, `select`/`filter`, `reject`, `take`, `take_while`,
`drop`, `drop_while`, `with_index`, `zip`, `uniq`) build a pipeline that runs only when consumed
(`to_list`, `each`, `first`, `find`, `for`, ...), one element at a time. `take`, `take_while`,
`first` and `find` stop the source early, so pipelines over infinite sources such as
`Lazy.iterate(0) { |n| n + 1 }` terminate. `Lazy.new { |emit| ... }` makes a sequence from any
function that feeds elements to `emit`; each consumption calls it again.

```
enum Shape {
  Circle(r: Float)
  Rect(w: Float, h: Float)
  Empty
}
s = Shape.Circle(2.0)
area = match s { .Circle(r) => 3.14 * r * r, .Rect(w, h) => w * h, .Empty => 0.0 }
```

Enums may be generic and have methods. Enum values are immutable. When no variant has a payload,
`E.values()` returns every variant in declaration order (`List[E]`), unless the enum declares its own
static `values`; on an enum with payloads it is an error (E0367).

`extend T { fn ... }` adds methods to an existing type (`String`, `List[T]`, a user class). Extensions
resolve statically and are visible only in modules that declare or import them (`import m` brings
`m`'s `pub extend` blocks into scope).

## 6. Generics

```
fn max[T: Comparable](xs: List[T]) -> T? { ... }
pub fn total(xs: Enumerable[Int]) -> Int { xs.reduce(0) { |a, b| a + b } }   # any Enumerable[Int]
```

A parameter of an interface type accepts a value of any type that satisfies the interface; each such
parameter (`fn f(a: I, b: I)`) may get a different type. To require the same type, use a type parameter
(`fn f[T: I](a: T, b: T)`); an interface with `Self` parameters must be used that way (E0375).

Type arguments are erased at run time: one compiled body serves every instantiation, and a type test
cannot check them (see §4.4 and E0374).

A member reached through a bound `T: I` (or `T: I & J`) is seen with `Self` replaced by `T`, so with
`interface Clone { fn clone(self) -> Self }`, `fn dup[T: Clone](x: T) -> T { x.clone() }` checks.

### 6.3 Implicit generics

A non-`pub` function may leave parameter types off. The checker infers a structural requirement for each
such parameter from how the body uses it, and checks every call site against it with the rules typed code
follows. A requirement records the types it is used at: the members the body reads or calls (with the
argument types), operators on either side, indexing and element assignment, iteration, calls of the
parameter as a function, and typed places the value flows into (an annotated variable, an argument, the
declared result). Values derived from a parameter carry requirements too, so `x.name` must have an
`upcase` below, and passing a value whose `name` is an `Int` is an error at the call.

```
fn shout(x) { x.name.upcase }      # x: { fn name(self) -> { fn upcase(self) -> R } }
fn greet(p) { "hi " + p.name }     # p.name must be a String
```

An argument that does not meet a requirement is E0342 (E0341 when the value flows into a typed place),
reported at the call with a note at the use in the body. A union argument must meet each requirement with
every member, and the result is the union of what each member produces. Requirements on a parameter that
is passed to another implicit-generic function become requirements of the caller's own parameter.

Generics are erased: one compiled body serves every instantiation, and it reaches members by name at
runtime. So, through an untyped parameter:

- a method must be the argument type's own method or field, or the only extension method of that name
  for its class; interface default methods and extensions of interfaces (such as `max` and `sort` on
  lists) need an annotated parameter;
- arguments are passed by position, so named arguments must follow the parameter order, and an `Int` is
  not accepted where a `Float` is expected (typed calls convert it; this one cannot);
- comparisons (`<`, `>`, ...) use the type's `<=>`, and bitwise operators apply to `Int` only;
- assigning a field (`x.n = v`), `?`, and members of `freeze(x)` are not supported (E0369).

Annotate the parameter to use any of these.

## 7. Errors

- Recoverable errors use `Result[T, E]`, `Ok(v)`, `Err(e)` and `?`. There are no exceptions.
- `panic(msg)`, failed `assert(cond, msg)`, out-of-bounds indexing, `x!` on nil, integer division by zero
  and ownership violations panic. A panic unwinds only the current thread. In the main thread it prints the
  message and a stack trace to stderr and exits with status 101. Trace frames name functions, `<main>`
  for the entry file's top level, `<block in f>`, `<spawn in f>` and `<parallel block in f>` (a
  `par_*` block, whichever thread ran it).
- `Thread.join` returns `Result[T, String]`: `Err` carries the panic message. A thread that panicked and
  was never joined is reported, with its trace, on stderr when the program ends.
- Recursion too deep panics with `SystemStackError: stack level too deep`, like any other panic. Calls
  between Catallaxy functions are limited (to at most 200,000 frames per thread or fiber). Some recursion
  also uses the native stack: comparing and hashing nested values (`==`, `<=>`, dict and set keys), and
  blocks called by native methods (`Mutex.lock`, `Atomic.update`, `par_map`, a class's own `==` called
  by a dict lookup). Native stacks are 256 MB on the main thread, 16 MB on spawned threads and pool
  workers, and 256 KB on a fiber, so a fiber reaches this limit first (at a few hundred nested
  `Mutex.lock` blocks, or about a thousand levels of nested lists).
- `with lock` is reentrant: a thread may lock an object it already holds, up to 1,000 levels deep.
  Deeper nesting panics with `LockError:`.
- An internal error of the runtime (a bug in Catallaxy, or in a native extension) is a panic whose
  message starts `internal error:`. It ends only the thread, fiber or `par_*` call it happened in, like
  any panic, and releases the locks that thread held.
- Running out of memory panics with a message starting `MemoryError:`, like any other panic (so on another
  thread `join` returns it as `Err`, and the program can carry on once the data is dropped):
  - A single request larger than physical memory (or than `CATX_MAX_HEAP`), such as `List.filled(n, x)`,
    `s * n` or `s.ljust(n)` with a huge `n`, is refused up front.
  - When the system refuses memory for the heap, or for one of the runtime's internal buffers, the runtime
    keeps going on a small emergency reserve, collects, and if the collection frees too little, the next
    thread to reach a safepoint (a call or a loop iteration) panics. With `CATX_MAX_HEAP` set, a
    collection that finds more live data than the limit does the same.
  - Building strings (`+`, interpolation, `*`, padding, `StringBuilder`) and growing a list (`push`) or a
    dict or set (`d[k] = v`, `add`) panics where the allocator refuses the memory, after a collection and
    one retry. List methods that rebuild the list (`insert`, `unshift` and the like) panic without the retry.
  - Standard library methods that build a list, dict or string from their arguments panic where the
    allocator refuses its buffer or one of its strings: splitting strings (`split`, `chars`, `lines`,
    `bytes`, `Regex.split`, `Regex.find_all`), copying collections (`sorted`, `reverse`, `dup`,
    `slice`, `x[a...b]`, `+`, `concat`, `keys`, `values`, `entries`, `Set.from`), parsing
    (`Json.parse`, `Csv.parse`), reading whole files and input (`File.read`, `File.read_lines`,
    `File.read_bytes`, `Stdin.read_all`, `Stdin.read_lines`, `Stdin.read_bytes`) and `Bytes`
    conversions. (`map`, `select`, `flat_map` and the other `Enumerable` methods are written in
    Catallaxy and grow their result with `push` or `concat`.) Most do not collect and retry first, because the values they have built so far are
    not yet reachable from the program; those that allocate their result before anything else
    (`chars`, `bytes`, `List.filled`, `List.with_capacity`) do. Once the system has refused memory,
    these methods also refuse heap objects that need a new heap segment until the next collection
    (the collection runs first where it can).
  - Starting a thread or fiber whose stacks cannot be allocated panics in `spawn` or `Fiber.start`
    (neither returns a `Result`). For a thread, the OS may refuse it instead, for lack of memory or a
    thread limit: that panic starts `ThreadError:`.
- What is not covered: other allocations inside the runtime get one retry with the headroom released
  and then a `MemoryError` at the next safepoint. If that retry fails too, the process aborts
  (`memory allocation of N bytes failed`). These include the scratch buffer of a stable sort (which
  `sorted` checks can be had just before), the text `Json.stringify` and `Csv.format` build, the keys
  of a parsed JSON object, and the collector's mark stack, which needs 8 bytes for each object in
  the longest list it marks (a list of 100,000 strings: 800 KB). The same happens when the reserve is used up before a
  safepoint is reached. The retry comes from the global allocator `catallaxy_gc::Allocator`, which
  `catx` and the C API libraries use; a Rust program embedding the runtime gets it only if it opts in
  (section 11). On Linux with memory overcommit, and under memory pressure on macOS, a program that
  grows gradually is usually killed by the OS before any allocation fails; set `CATX_MAX_HEAP` below
  the machine's memory to turn that into a `MemoryError`.

## 8. Concurrency

Every type has a capability:

| Capability | Types | Crosses threads |
| --- | --- | --- |
| frozen | `Int Float Bool Nil String Range NativeHandle`, `frozen T`, `frozen class` (e.g. `Regex`, `Bytes`, `DateTime`), enums/tuples of frozen | freely |
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
Only locals and fresh literals can be moved; a literal has no other reference, so `move` on it
(`Mutex.new(move ["hits": 0])`) is accepted and optional.
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
`d.method(...)`, arithmetic) is checked at runtime and yields `Dyn`. As a missing object key gives `nil`, so does a list index out
of range (`doc["users"][5]`). Convert with `as?`:
`name = (d["name"] as? String) ?? "anon"`. Every typed value converts implicitly to `Dyn`. JSON objects parse to records that behave exactly like `Dict[String, Dyn]` (`as? Dict[String, Dyn]` succeeds); records parsed with the same keys share a hidden class, so `d.name` is served from a per-site cache. `Json.parse` reads a number with no fraction or exponent as an `Int` of any size, so `123456789012345678901234567890` stays exact and `as? Int` succeeds; any other number (`1.5`, `1e3`) is a `Float`. `Json.stringify` writes object keys in sorted order and integers of any size (including big integers) as JSON numbers; it writes a class instance as an object of its fields, or as whatever its `to_json` method returns (`Deque` writes an array).

Generic types are erased at runtime, so a call through `Dyn` can put a value of the wrong type into
typed data: `d: Dyn = xs` then `d.push(5)` adds an `Int` to a `List[String]`. Such a value is caught
where it is used as its static type (a standard-library method's receiver or argument, a field
access, a list or tuple access, a method call), with a `TypeError` panic that names the
expected and actual types: `xs[0].length` panics with `TypeError: expected String, got Int`.

## 10. Tooling

`catx run file.cx [args]`, `catx check [--json] paths...`, `catx fmt [--check] paths...`, `catx repl`,
`catx lsp`, `catx test [paths...]`. `check` and `fmt` take files and directories; a directory stands for
every `*.cx` file under it, skipping hidden directories and `target/`.

`catx test paths...` runs each named `*.cx` file, and every `*.cx` file under each named directory
(skipping hidden directories, `target/`, and `lib/` directories, which hold modules for tests to
import), each in its own process. With no path it runs the tests under the current directory. A
path that does not exist is an error (exit status 2), and so is finding no tests at all. A file whose first line is `# expect: CODE[, CODE...]`
must fail `catx check` with those diagnostic codes (if every code is a warning, `W...`, it need only
report them). Any other file must run with standard output
equal to its `.out` file and exit with status 0, or with the status given by a `# exit: N` line; a
`.in` file next to it is fed to standard input. A program still running after `CATX_TEST_TIMEOUT`
seconds (default 120) is killed and fails with its output so far. The exit status is 1 if anything
failed.

`catx fmt` rewrites files in place (`catx fmt -` filters stdin to stdout). It keeps comments and the
program's line structure; it re-indents with two spaces, normalizes spacing between tokens, and breaks a
`{ ... }` block across lines when its line would exceed 100 columns. `--check` lists the files that
would change and exits with status 1 if there are any. Files with syntax errors are left untouched and
reported (exit status 2). When walking a directory, `fmt` skips `reject/` directories, whose
conformance programs are meant to be wrong; a file named on the command line is always formatted.

`catx lsp` is a language server over stdio. Open buffers overlay the file system, so imports see
unsaved edits. It provides:

- diagnostics (published on open, change and save), hover (types, signatures), go to definition,
  completion (members after `.`, names in scope, keywords) and document formatting (as `catx fmt`);
- find references and rename (with prepare-rename) for locals, parameters (including named
  arguments), functions, methods (an override and the method it overrides rename together), fields,
  classes, enums, variants, interfaces and constants. An interface member renames together with the
  same-named methods of every type whose values are used as that interface (passed, assigned,
  returned, put in a list or dict, or checked against a generic bound), so those types keep
  conforming; renaming such a method renames the member too. They cover the workspace (below);
  names from the prelude cannot be renamed, nor can a member that a built-in type implements;
- signature help for function, method and constructor calls, showing default values and marking
  the trailing-block parameter, with the active parameter following commas and named arguments;
- inlay hints with the inferred types of unannotated variables, `for`/`let`/`if let` bindings and
  block parameters, and the inferred requirement of each untyped parameter of an implicit-generic
  function (`fn shout(x)` shows `x: { fn upcase(self) -> _ }`);
- document symbols (an outline of items, members and top-level variables) and workspace symbols
  (over the workspace);
- code actions: every fix attached to a diagnostic is offered as a quick fix, and the first
  applicable one (see `catx fix`) is marked preferred.

The workspace of references, rename and workspace symbols is every `.cx` file under the workspace
folders the client names (`workspaceFolders`, else `rootUri`; folders added or removed later count)
plus every open document, each with the modules it imports, whether or not the file is open.
Directories named `target` or `node_modules` and hidden directories (`.git` and the like) are
skipped. An open document's text takes precedence over the file on disk. Each file's compilation is
cached and reused while the files it loaded are unchanged, so after the first request only edited
files and the files that import them are checked again. These compilations share one checked copy
of the prelude rather than each checking it again. The cache is filled in the background after
the server starts and whenever workspace folders are added, on half the cores and pausing while
requests and diagnostics run, so the first request usually finds it ready; a request that comes
earlier waits only for the files being compiled at that moment and compiles the rest itself. The
client is shown its progress (`window/workDoneProgress`) and the time taken goes to the server's
log. At most 2000 files are taken from the
workspace folders (set `CATX_LSP_MAX_FILES` to change the limit); a larger workspace is cut, and the
server says so in its log.

`catx fix [--dry-run] paths...` applies the checker's fixes. For each file (directories are walked as
for `catx fmt`, skipping `reject/`) it applies the first applicable fix of every diagnostic, checks
again, and repeats until nothing more applies (at most 10 rounds). Fixes that guess (`did you mean`)
or leave a placeholder (`: Int`, `?? <default>`) are suggestions only and are never applied. Only the
named files are edited, not the modules they import. A file that was formatted before is formatted
again afterwards. It prints each fix it applies; `--dry-run` prints them without writing. The exit
status is 1 if errors remain, 2 if a file could not be read or written.

Every diagnostic has a code. `docs/errors.md` explains each one with an example and a fix, and
`catx check` (like `catx run` when a program does not compile) ends its report with a link to the
entries for the codes it printed.

Warnings never make `catx check` fail. A comment `# allow(CODE, ...)` on the warning's line, or on
the line above it, silences those warnings there (`s += x # allow(W0004)`). The warnings are:

- `W0001`: `!` on a value that is never nil (fix: remove it). `W0002`: the left side of `??` is never
  nil. `W0003`: `?.` on a value that is never nil.
- `W0004`: a `String` local declared outside a loop grows by `s += x` or `s = s + x` inside it (a
  `for`, `while` or `loop` body, or the block of `each*`, `times`, `upto`, `downto`, `map`,
  `flat_map`, `filter`, `filter_map`, `reject` or `select`). Each append copies the whole string, so
  the loop is quadratic; a `StringBuilder` appends in place. Loops with a literal range of at most 16
  elements (`for i in 0..3`, `(1..4).each`, `4.times`, `1.upto(8)`) are not reported. When the
  rewrite is mechanical (the loop is a statement on its own lines, it touches the local only by
  appending, it cannot `return` or `?` out, and no closure elsewhere captures the local), the fix
  adds `s_sb = StringBuilder.new()` and `s_sb.push(s)` before the loop (or replaces a `s = ""` just
  before it), turns each append into `s_sb.push(x)`, and adds `s = s_sb.to_s` after the loop.

`check --json` prints `{"diagnostics": [...]}` where each diagnostic has `code`, `severity`, `message`,
`file`, `span` (`start`/`end` as `{line, col, offset}`, 1-based lines and columns), `notes`, and `fixes`
(each with a `message`, `applicable` (false for a suggestion a person must check), and a list of
`{span, replacement}` `edits`).

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

`native::new_list` and `Mutator::alloc_str` abort if the allocator refuses the memory. A native
that may build large results can raise a `MemoryError` instead: `vm.m.try_alloc_list(items)` and
`vm.m.try_alloc_str_nogc(s)` never collect, so they are safe while the native holds values that
nothing roots; `vm.m.try_alloc_str(s)` collects and retries once, so everything the native holds
must be rooted.

`native::new_handle(vm, data)` wraps Rust data (`Send + Sync`) in a `NativeHandle`, and
`native::handle::<T>(v)` gets it back. A handle is frozen, so Catallaxy code may keep it in a
`frozen class` and pass it between threads: the data must be immutable or synchronize itself
(the standard library's `Regex` holds its compiled regex this way). It is dropped when the
collector frees the handle.

The `catallaxy` crate does not choose a global allocator. To get the out-of-memory handling `catx`
has (section 7: a refused allocation inside the runtime releases the headroom and is retried once,
and a `MemoryError` follows at the next safepoint instead of an abort), add one line to the program:

```rust
#[global_allocator]
static ALLOC: catallaxy::Allocator = catallaxy::Allocator;
```

It forwards to the system allocator and does extra work only when a request is refused.

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
example, and `crates/catallaxy-capi/examples/dlopen.c` loads the shared library at run time.

The libraries' own Rust code allocates through `catallaxy::Allocator` (the crate's default
`global-allocator` feature), so they handle running out of memory as `catx` does; the host's
`malloc` is not affected. `cx_allocator_active()` returns 1 when this is in effect. Each shared
library has its own copy of the Rust standard library and allocator, so it can be loaded with
`dlopen` next to other Rust libraries. A Rust program that links `catallaxy-capi` as a Rust crate
and has its own `#[global_allocator]` turns the feature off with `default-features = false`.

