# Known bugs

Bugs found while writing the example corpus (`examples/`) and the extra reject tests. Each entry has a
minimal reproduction, the actual and expected behavior, and the workaround the corpus uses so that it
stays green. Nothing here has been fixed in the compiler or runtime.

## 1. Release builds lose every method of builtin classes for dynamic dispatch

**Severity:** critical. `cargo build --release` produces a `catx` that fails 10 of the 63 existing
conformance tests (and `examples/readme_tour.cx`); the debug build passes all of them.

```
puts([3, 1].sort())
```

- Actual (release): `panic: NoMethodError: undefined method 'each' for List` from
  `Enumerable.to_list (<prelude>/core.cx:101)`, exit 101.
- Expected (and what the debug build does): `[1, 3]`.

Any call that reaches a builtin type (`Int`, `String`, `List`, `Dict`, `Range`, ...) through dynamic
dispatch fails: Enumerable default methods on builtins (`sum`, `join`, `sort`, `sort_by`, `min`, `max`,
`any`, `all`, `find`, `uniq`, `group_by`, `tally_by`, `take`, `drop`, `map` on a `Range`/`Dict`, ...),
Comparable defaults on builtins inside generics (`fn big[T: Comparable](a: T, b: T)` with `Int`
panics with ``undefined method `<=>` for Int``), structural interface parameters
(`fn r(x: { fn to_s(self) -> String }) -> String { x.to_s() }; r(5)`) and implicit generics
(`fn shout(x) { x.name.upcase }` panics on `upcase` for `String`). Statically dispatched calls
(`5.to_s()`, `xs.map {...}`, `xs.reduce(...)`, `xs.sorted()`) and user classes work.

Probable cause: `catallaxy_runtime::class::set_methods` writes the method table of a builtin class
through a `&'static Class` cast to `*mut Class` (`crates/catallaxy-runtime/src/class.rs`). `Class` has
no `UnsafeCell`, so this is undefined behaviour; with `lto = "thin"` and `codegen-units = 1` the store
is optimized away and the builtin classes keep empty method tables. `program.rs` (the builtin-method
loader) calls it.

**Workaround in the corpus:** the examples only call statically dispatched methods on builtin types:
`xs.reduce(0) { |a, b| a + b }` instead of `xs.sum()`, `sorted()`/`sort_with`/`sort_by_keys` instead of
`sort()`/`sort_by`, explicit loops instead of `any`/`all`/`find`/`min`/`max`, and a `StringBuilder`
(or a small `join` helper) instead of `join`. Generic `Comparable` code and Enumerable defaults are
exercised only with user classes.

## 2. A duplicate function declaration reports its own parameters as unknown

**Severity:** low (confusing diagnostic). Found while writing `tests/conformance/reject/duplicate_fn.cx`.

```
fn area(r: Float) -> Float { r * r }
fn area(w: Float, h: Float) -> Float { w * h }
```

- Actual: `E0201 area is declared more than once`, followed by two spurious
  ``E0303 cannot find `w` in this scope`` / `` `h` `` errors pointing into the second body.
- Expected: only E0201. The second body should be checked against its own parameters, or skipped.

**Workaround:** none needed; the reject test only requires E0201.

## 3. A mistyped block parameter is reported twice

**Severity:** low (noisy diagnostic). Found while writing `tests/conformance/reject/block_param_type.cx`.

```
puts([1, 2].map { |s: String| s.length })
```

- Actual: `E0300 mismatched types in argument f: expected fn(Int) -> Int, found fn(String) -> Int` and
  ``E0357 block parameter `s` is `String` but `Int` is passed`` for the same mistake.
- Expected: only E0357, the more specific error. (`wrong_arity_block.cx` shows the same E0355 + E0300
  pair, and `unexpected_block.cx` adds an E0356 "cannot infer block parameter" after E0337.)

**Workaround:** none needed.

## 4. `self` is nil inside blocks and closures created in a method

**Severity:** high (valid program crashes; debug and release alike). Hit independently while writing
`examples/sudoku.cx`, `examples/matrix.cx` and `examples/bank_accounts.cx`.

```
class C {
  n: Int = 7
  fn f(self) -> fn() -> Int { { || self.n } }
  fn d(self) -> List[Int] { [1, 2].map { |i| i + self.n } }
}
puts(C.new().f()())
puts(C.new().d())
```

- Actual: `panic: internal error: field access on Nil` at `<block in f>`, exit 101. Calling
  `self.method(...)` inside a block enters the method with a nil receiver and panics there.
- Expected: `7` and `[8, 9]`. Closures capture variables by reference (reference §4.5) and the checker
  accepts the program, so `self` should be captured like any other local.

**Workaround:** bind the receiver (or the field) to a local before the block (`me = self`, then
`me.n` inside the block), or use a `for` loop instead of a block. Used in `examples/sudoku.cx`,
`examples/matrix.cx` (`transpose`) and `examples/bank_accounts.cx` (`Checking.withdraw`).

## 5. GC stress mode segfaults on a top-level program

**Severity:** high (memory unsafety). Found by running `examples/run_length.cx` with
`CATX_GC_STRESS=1`.

Reduced repro:

```
fn rle_encode(s: String) -> String {
  sb = StringBuilder.new()
  chars = s.chars
  i = 0
  while i < chars.length {
    j = i
    while j < chars.length and chars[j] == chars[i] { j += 1 }
    sb.push((j - i).to_s)
    i = j
  }
  sb.to_s
}
fn rle_decode(s: String) -> Result[String, String] {
  sb = StringBuilder.new()
  Ok(sb.to_s)
}
stripes = "W" * 12 + "B" + "W" * 12 + "B" * 3 + "W" * 24 + "B" + "W" * 14
for s in [stripes, "aaabccddd", "abc", ""] {
  enc = rle_encode(s)
  back = rle_decode(enc).unwrap()
  puts("#{s.length} -> #{enc.length} chars: #{inspect(enc)} (round trip: #{back == s})")
}
codes: Dict[String, String] = [:]
for sym in codes.keys { puts("#{sym}: #{codes[sym]!}") }
```

- Actual: `env CATX_GC_STRESS=1 ./target/release/catx run repro.cx` dies with SIGSEGV (exit 139) and
  prints nothing (buffered stdout is lost). The debug build panics with `null pointer dereference` at
  `crates/catallaxy-gc/src/collector.rs:417` (`mark_worker` reading `(*h).vtable()`), reached from a
  safepoint in `Vm::run`.
- Expected: the same four lines as without GC stress, exit 0.
- Notes: the crash depends on the body of the last loop, which never runs (the dict is empty).
  Replacing its `puts` with `puts(sym)` hides the crash. This suggests the collector scans main-frame
  registers that hold stale pointers to objects that were already swept, or skips live ones at some
  safepoints.

**Workaround:** `examples/run_length.cx` moves its top-level driver code into functions (`demo_rle`,
`demo_huffman`).

## 6. Exhaustive `match` on a tuple is rejected ("does not cover _")

**Severity:** medium (valid program rejected). Found while writing `examples/state_machine.cx`.

```
enum A { X, Y }
fn g(a: A, b: A) -> Int {
  match (a, b) {
    (.X, .X) => 1
    (.X, .Y) => 2
    (.Y, _) => 3
  }
}
puts(g(A.X, A.Y))
```

- Actual: ``error[E0360]: `match` does not cover _``, exit 2. The same happens for
  `(_, true) => ..., (.X, false) | (.Y, false) => ...`.
- Expected: accepted, printing `2`. If a case really were missing, the message should name it, for
  example `(.Y, .X)`, not `_`.

**Workaround:** end tuple matches with a `_ =>` arm (`Machine.transition` in `examples/state_machine.cx`).

## 7. Tuple patterns cannot match an optional tuple, even after a `nil` arm

**Severity:** medium (valid program rejected). Found while writing `examples/palindromes.cx`.

```
fn f(n: Int) -> (Int, Int)? { if n > 0 { (n, n) } else { nil } }
match f(1) {
  nil => puts("none")
  (a, b) => puts(a + b)
}
```

- Actual: ``E0362 tuple pattern cannot match `(Int, Int)?` ``, plus `E0360 match does not cover (Int, Int)`
  and two follow-on `E0303 cannot find a/b` errors.
- Expected: prints `2`. Enum-variant patterns already match through an optional (`match e { nil => ...,
  .A(v) => ... }` with `e: E?` is accepted), so tuple patterns should too.

**Workaround:** `if let found = f(1) { let (a, b) = found ... }` in `examples/palindromes.cx`.

## 8. A binding arm after `nil` does not narrow an optional subject

**Severity:** low (valid program rejected). Found while writing `examples/palindromes.cx`.

```
fn g(x: Int?) {
  match x {
    nil => puts("none")
    n => puts(n + 1)
  }
}
```

- Actual: `E0310 left side of + may be nil (type Int?)` on `n + 1`.
- Expected: accepted. The `nil` arm has already matched every nil value, so `n` is `Int` (reference
  §4.4: narrowing applies "in `match` arms on a variable subject").

**Workaround:** the corpus uses `if let` or `??` instead.

## 9. An annotated optional local is narrowed by its initializer, so a `nil` arm is rejected

**Severity:** low (valid program rejected, confusing message). Found while writing
`examples/palindromes.cx`.

```
y: Int? = 3
match y {
  nil => puts("none")
  n => puts(n)
}
```

- Actual: `E0362 pattern of type Nil can never match Int`.
- Expected: accepted, printing `3`. `y` is declared `Int?`, and the reference does not list assignment
  among the narrowing points. If assignment narrowing is intended, an unreachable arm should at most
  be a warning.

**Workaround:** not needed in the corpus.

## 10. Block parameters on a frozen collection lose `frozen`

**Severity:** medium (a static check is missed; the runtime still catches it). Found while writing
`examples/mandelbrot.cx`.

```
fn grow(xs: List[Int]) { xs.push(99) }
grid = freeze([[1, 2], [3]])
grid.each { |row| grow(row) }
```

- Actual: `catx check` accepts it. At run time: `panic: FrozenError: cannot modify frozen List`, exit 101.
- Expected: a compile-time error, as for the direct forms. `grow(grid[0])` gives
  ``E0300 expected `List[Int]`, found `frozen List[Int]` ``, and `for row in grid { row.push(7) }` gives
  E0614.
- The reverse fails as well: given `fn render(iters: frozen List[Int])`,
  `grid.map { |it| render(it) }` is rejected with ``E0300 expected `frozen List[Int]`, found `List[Int]` ``.

**Workaround:** `examples/mandelbrot.cx` renders the rows from the list before freezing it, so no
block ever sees a frozen element.

## 11. A trailing block in a `for` header produces a cascade of misleading errors

**Severity:** low (noisy diagnostic). Hit while writing `examples/mandelbrot.cx` and
`examples/roman_numerals.cx`. The existing `tests/conformance/reject/block_in_header.cx` shows the
same problem.

```
grid = [[1], [2]]
for line in grid.map { |it| it.length } { puts(line) }
```

- Actual: four errors. ``E0338 missing argument `f` for `map` `` comes first and is misleading. Then
  the correct E0109 ("a block argument cannot follow a call in an `if`/`while`/`for`/`match` header",
  with a fix), then `E0100 expected an expression, found |` and `E0102 expected end of statement, found {`.
  With a tuple loop pattern (`for (c, i) in ...`), E0361 and E0362 are also reported, ahead of E0109.
- Expected: only E0109.

**Workaround:** compute the collection into a local before the `for`.

## Notes (not bugs)

- Dict literal keys written as bare identifiers are string keys: given a local `source`,
  `[source: 0]` is `["source": 0]`, not a dict keyed by the local's value. The conformance suite relies
  on this, but the reference never documents dict literal syntax, and it caught out one example
  (`examples/dijkstra.cx`). It should be documented, and perhaps linted when the key shadows a local.
- Also undocumented but relied on by the corpus: dicts iterate in insertion order, `Json.stringify`
  sorts object keys, `Atomic.add` returns the new value, and `String.center` puts the extra padding
  on the right.
- `f()?.downcase` parses as optional chaining (`?.`), not as try followed by a method call. Warning
  W0003 explains this.
