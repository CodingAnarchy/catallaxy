# Catallaxy Standard Library

Generated from the prelude by `scripts/gen_stdlib_docs.py`; do not edit by hand.
Everything here is available in every module without an import. The text under an
entry is its documentation comment, which editors also show on hover and in completion.

## interface Display

- `fn to_s(self) -> String`

## interface Hashable

- `fn hash(self) -> Int`

## interface Comparable

- `fn <=>(self, other: Self) -> Int`
- `fn <(self, other: Self) -> Bool`
- `fn <=(self, other: Self) -> Bool`
- `fn >(self, other: Self) -> Bool`
- `fn >=(self, other: Self) -> Bool`
- `fn between(self, lo: Self, hi: Self) -> Bool`
- `fn clamp(self, lo: Self, hi: Self) -> Self`

## interface Indexable[T]

- `fn [](self, index: Int) -> T`
- `fn length(self) -> Int`

## interface Callable

- `fn call(self) -> Dyn`

## interface Enumerable[T]

- `fn each(self, f: fn(T))`
- `fn map[U](self, f: fn(T) -> U) -> List[U]`
- `fn flat_map[U](self, f: fn(T) -> List[U]) -> List[U]`
- `fn select(self, f: fn(T) -> Bool) -> List[T]`
- `fn filter(self, f: fn(T) -> Bool) -> List[T]`
- `fn reject(self, f: fn(T) -> Bool) -> List[T]`
- `fn filter_map[U](self, f: fn(T) -> U?) -> List[U]`

  The non-nil results of `f`, in order: `select` and `map` in one pass.

- `fn reduce[U](self, init: U, f: fn(U, T) -> U) -> U`

  Folds the elements left to right: `f(f(init, x0), x1)`...; `init` if there are none.

- `fn count(self, f: (fn(T) -> Bool)? = nil) -> Int`

  The number of elements for which `f` is true; without a block, the number of elements.

- `fn size(self) -> Int`
- `fn any(self, f: fn(T) -> Bool) -> Bool`
- `fn all(self, f: fn(T) -> Bool) -> Bool`
- `fn none(self, f: fn(T) -> Bool) -> Bool`
- `fn find(self, f: fn(T) -> Bool) -> T?`

  The first element for which `f` is true, or nil.

- `fn find_index(self, f: fn(T) -> Bool) -> Int?`

  The position of the first element for which `f` is true, or nil.

- `fn to_list(self) -> List[T]`
- `fn first(self) -> T?`
- `fn each_with_index(self, f: fn(T, Int))`
- `fn with_index(self) -> List[(T, Int)]`
- `fn each_slice(self, n: Int, f: (fn(List[T]))? = nil) -> Lazy[List[T]]`

  Calls `f` with consecutive slices of `n` elements (the last may be shorter). Returns the
  slices as a `Lazy`, so without a block `xs.each_slice(2).to_list()` lists them.

- `fn each_cons(self, n: Int, f: (fn(List[T]))? = nil) -> Lazy[List[T]]`

  Calls `f` with each run of `n` consecutive elements. Returns the runs as a `Lazy`, as
  `each_slice` does.

- `fn take(self, n: Int) -> List[T]`

  The first `n` elements (all of them if there are fewer; none if `n <= 0`).

- `fn take_while(self, f: fn(T) -> Bool) -> List[T]`
- `fn drop(self, n: Int) -> List[T]`

  The elements after the first `n` (all of them if `n <= 0`).

- `fn drop_while(self, f: fn(T) -> Bool) -> List[T]`
- `fn group_by[K](self, f: fn(T) -> K) -> Dict[K, List[T]]`

  The elements grouped by `f`'s result, keys in order of first occurrence and each group in
  element order.

- `fn partition(self, f: fn(T) -> Bool) -> (List[T], List[T])`

  `(matching, rest)`: the elements for which `f` is true, and the others, each in order.

- `fn tally(self) -> Dict[T, Int]`

  How many times each element occurs, in order of first occurrence.

- `fn tally_by[K](self, f: fn(T) -> K) -> Dict[K, Int]`
- `fn sort_by[K: Comparable](self, f: fn(T) -> K) -> List[T]`

  The elements sorted by the key `f` computes once per element. The sort is stable: elements
  with equal keys keep their order.

- `fn min_by[K: Comparable](self, f: fn(T) -> K) -> T?`

  The first element with the smallest key, or nil if there are none.

- `fn max_by[K: Comparable](self, f: fn(T) -> K) -> T?`

  The first element with the largest key, or nil if there are none.

- `fn sum_by(self, f: fn(T) -> Int) -> Int`
- `fn sum_by_float(self, f: fn(T) -> Float) -> Float`

  As `sum_by`, for a `Float` block.

- `fn take_last(self, n: Int) -> List[T]`

  The last `n` elements (all of them if there are fewer).

- `fn join(self, sep: String = "") -> String`

  The elements' `to_s` joined with `sep` between them.

- `fn zip[U](self, other: List[U]) -> List[(T, U)]`

  Pairs of elements of this sequence and `other`, as long as the shorter.

- `fn uniq(self) -> List[T]`

  The elements without repeats (keeping first occurrences), compared by `==` and `hash`.

- `fn contains(self, item: T) -> Bool`
- `fn include(self, item: T) -> Bool`
- `fn to_set(self) -> Set[T]`
- `fn lazy(self) -> Lazy[T]`

  A lazy view of the elements: `map`, `select` and the like on it build a pipeline instead of a
  list, and nothing runs until it is consumed. See `Lazy`.

## Lazy[T]

A lazy sequence. `source` is called with a function to feed each element to; the adaptors
(`map`, `select`, `take`, ...) wrap it in another such function, so elements flow through the
whole pipeline one at a time, only when the sequence is consumed (`each`, `to_list`, `first`,
`find`, a `for` loop or any other `Enumerable` method). `take`, `take_while`, `first` and `find`
stop pulling from the source once they are done, so a pipeline over an infinite source
terminates:

```catallaxy
Lazy.iterate(1) { |n| n + 1 }.map { |n| n * n }.select { |n| n.is_odd }.take(3).to_list()
# [1, 9, 25]
```

A Lazy may be consumed more than once: each consumption runs the source again.

- field `source: fn(fn(T))`
- `fn iterate(seed: T, f: fn(T) -> T) -> Lazy[T]`

  The infinite sequence `seed`, `f(seed)`, `f(f(seed))`, ...

- `fn repeat(value: T) -> Lazy[T]`

  `value` forever.

- `fn each(self, f: fn(T))`
- `fn lazy(self) -> Lazy[T]`
- `fn map[U](self, f: fn(T) -> U) -> Lazy[U]`
- `fn flat_map[U](self, f: fn(T) -> List[U]) -> Lazy[U]`
- `fn select(self, f: fn(T) -> Bool) -> Lazy[T]`
- `fn filter(self, f: fn(T) -> Bool) -> Lazy[T]`
- `fn reject(self, f: fn(T) -> Bool) -> Lazy[T]`
- `fn filter_map[U](self, f: fn(T) -> U?) -> Lazy[U]`
- `fn take(self, n: Int) -> Lazy[T]`

  The first `n` elements; the source is not asked for an element past them.

- `fn take_while(self, f: fn(T) -> Bool) -> Lazy[T]`
- `fn drop(self, n: Int) -> Lazy[T]`
- `fn drop_while(self, f: fn(T) -> Bool) -> Lazy[T]`
- `fn with_index(self) -> Lazy[(T, Int)]`

  Each element paired with its position.

- `fn zip[U](self, other: List[U]) -> Lazy[(T, U)]`

  Pairs of elements of this sequence and `other`, as long as the shorter.

- `fn uniq(self) -> Lazy[T]`

  The elements without repeats (keeping first occurrences).

- `fn force(self) -> List[T]`

  The elements as a list (like `to_list`).

- `fn to_s(self) -> String`

  Not the elements: printing a sequence must not run it (it may be infinite).

## Enumerable[T] (where T: Comparable)

- `fn min(self) -> T?`

  The smallest element (the first of equals), or nil if there are none.

- `fn max(self) -> T?`

  The largest element (the first of equals), or nil if there are none.

- `fn sort(self) -> List[T]`
- `fn minmax(self) -> (T?, T?)`

## Enumerable[Int]

- `fn sum(self) -> Int`

  The total, 0 if there are no elements. `Int`s never overflow (they grow as needed).

- `fn product(self) -> Int`

## Enumerable[Float]

- `fn sum(self) -> Float`
- `fn product(self) -> Float`

## Global functions

- `fn puts(value: Dyn = "")`

  Writes `value.to_s` and a newline to standard output (`puts(nil)` writes `nil`). Output is
  buffered, and flushed at each newline when stdout is a terminal, before reading input, before
  writing to stderr and at exit.

- `fn print(value: Dyn)`

  As `puts`, without the newline.

- `fn eputs(value: Dyn = "")`

  Writes `value.to_s` and a newline to standard error, unbuffered, after flushing stdout.

- `fn panic(message: String) -> Never`

  Ends the current thread or fiber with `message`; the program exits with an error if it is the
  main one, else `join` returns `Err(message)`.

- `fn assert(condition: Bool, message: String = "assertion failed")`

  Panics with `message` unless `condition` is true.

- `fn assert_eq(actual: Dyn, expected: Dyn, message: String = "")`

  Panics unless `actual == expected` (structural equality), with
  `assertion failed: <message>: expected <expected>, got <actual>`.

- `fn inspect(value: Dyn) -> String`

  The value as source-like text: strings quoted and escaped, `nil` as `nil`, collections with
  their items inspected (`["a", 1]`).

- `fn compare_values(a: Dyn, b: Dyn) -> Int`

  -1, 0 or 1 by the natural order of two values (an `Int` and a `Float` compare numerically).
  Panics with an `ArgumentError` if they are not comparable (different types, `nil`, NaN).

- `fn read_line() -> String?`

  The next line of standard input without its `\n` or `\r\n`, or nil at end of input. Flushes
  stdout first, so a prompt written with `print` shows.

- `fn args() -> List[String]`

  The command-line arguments after the script path.

- `fn exit(code: Int = 0) -> Never`

  Ends the whole program with exit status `code`, from any thread; buffered output is flushed.

- `fn sleep(seconds: Float)`

  Pauses the calling fiber or thread for `seconds` (fractions allowed; negative or NaN is 0).
  Other fibers on the thread run meanwhile.

- `fn gc()`

  Runs a garbage collection now.

## enum Result[T, E]

- variant `Ok(T)`
- variant `Err(E)`
- `fn is_ok(self) -> Bool`
- `fn is_err(self) -> Bool`
- `fn ok(self) -> T?`

  The `Ok` value, or nil for an `Err`.

- `fn err(self) -> E?`

  The `Err` value, or nil for an `Ok`.

- `fn unwrap(self) -> T`

  The `Ok` value. Panics on an `Err` with `called unwrap on Err: <error>`.

- `fn unwrap_err(self) -> E`

  The `Err` value. Panics on an `Ok` with `called unwrap_err on Ok: <value>`.

- `fn expect(self, message: String) -> T`

  The `Ok` value. Panics on an `Err` with `<message>: <error>`.

- `fn unwrap_or(self, default: T) -> T`

  The `Ok` value, or `default` for an `Err`.

- `fn unwrap_or_else(self, f: fn(E) -> T) -> T`

  The `Ok` value, or `f(error)` for an `Err` (`f` runs only then).

- `fn map[U](self, f: fn(T) -> U) -> Result[U, E]`

  `Ok(f(value))`, or the same `Err`.

- `fn map_err[F](self, f: fn(E) -> F) -> Result[T, F]`

  `Err(f(error))`, or the same `Ok`.

- `fn and_then[U](self, f: fn(T) -> Result[U, E]) -> Result[U, E]`

  `f(value)` for an `Ok` (chaining steps that can fail), or the same `Err`.

## T?

Optionals: a nil-able value as a `Result`, so `?` can propagate a missing value from a function
that returns a `Result`: `port = env["PORT"].ok_or("PORT is not set")?`.

- `fn ok_or[E](self, error: E) -> Result[T, E]`

  `Ok(value)`, or `Err(error)` if nil.

- `fn ok_or_else[E](self, f: fn() -> E) -> Result[T, E]`

  `Ok(value)`, or `Err(f())` if nil (`f` runs only then).

## Int

- `fn to_s(self, base: Int = 10) -> String`

  Digits in `base` (2 to 36, lowercase letters above 9), with a leading `-` if negative.
  Panics with an `ArgumentError` for another base.

- `fn hash(self) -> Int`
- `fn <=>(self, other: Int) -> Int`
- `fn to_f(self) -> Float`

  The nearest `Float`; `Infinity` (or `-Infinity`) beyond its range.

- `fn to_i(self) -> Int`
- `fn abs(self) -> Int`
- `fn sign(self) -> Int`

  -1, 0 or 1.

- `fn is_even(self) -> Bool`
- `fn is_odd(self) -> Bool`
- `fn is_zero(self) -> Bool`
- `fn succ(self) -> Int`
- `fn pred(self) -> Int`
- `fn min(self, other: Int) -> Int`
- `fn max(self, other: Int) -> Int`
- `fn times(self, f: fn(Int))`

  Calls `f` with 0, 1, ... up to `self - 1`; not at all if `self <= 0`.

- `fn upto(self, limit: Int, f: fn(Int))`

  Calls `f` with `self`, `self + 1`, ... up to `limit` inclusive; not at all if `limit < self`.

- `fn downto(self, limit: Int, f: fn(Int))`

  Calls `f` with `self`, `self - 1`, ... down to `limit` inclusive; not at all if
  `limit > self`.

- `fn gcd(self, other: Int) -> Int`

  The greatest common divisor, never negative; `0.gcd(0)` is 0.

- `fn lcm(self, other: Int) -> Int`

  The least common multiple, never negative; 0 if either is 0.

- `fn pow(self, exp: Int) -> Int`

  `self ** exp`, exact (`Int`s grow as needed; `0.pow(0)` is 1). Panics if `exp` is negative;
  use `Float` powers for those.

- `fn to_s_radix(self, radix: Int) -> String`

  As `to_s(radix)`. Panics with an `ArgumentError` unless `radix` is 2 to 36.

- `fn chr(self) -> String`

  The character with this Unicode code point: `65.chr == "A"`. Panics with a `RangeError` for a
  negative number, a surrogate (0xD800-0xDFFF) or one above 0x10FFFF.

- `fn sqrt(self) -> Int`

  The integer square root (rounded down), exact for any size. Panics with a `Math::DomainError`
  if negative.

- `fn digits(self) -> List[Int]`

  The decimal digits of the absolute value, least significant first:
  `1234.digits == [4, 3, 2, 1]`, `0.digits == [0]`.

- `fn bit_length(self) -> Int`

  The number of bits in the absolute value: `255.bit_length == 8`, `0.bit_length == 0`.

## Float

- `fn to_s(self) -> String`

  The shortest text that reads back as the same value, always with a `.`: `1.0`, `0.1`; with an
  exponent below 1e-4 and from 1e16 on: `1.0e+21`, `1.0e-07`. Also `NaN`, `Infinity`,
  `-Infinity`.

- `fn hash(self) -> Int`
- `fn <=>(self, other: Float) -> Int`
- `fn to_f(self) -> Float`
- `fn to_i(self) -> Int`

  Truncates toward zero, exactly (an `Int` holds any finite Float). Panics with a
  `FloatDomainError` for NaN or an infinity, as `floor`, `ceil`, `round` and `truncate` do.

- `fn floor(self) -> Int`

  The largest `Int` not above this. Panics with a `FloatDomainError` for NaN or an infinity.

- `fn ceil(self) -> Int`

  The smallest `Int` not below this. Panics with a `FloatDomainError` for NaN or an infinity.

- `fn round(self) -> Int`

  The nearest `Int`, halves away from zero (`2.5.round == 3`, `(-2.5).round == -3`). Panics with
  a `FloatDomainError` for NaN or an infinity.

- `fn round_to(self, digits: Int) -> Float`

  Rounded to `digits` decimal places (halves away from zero), as a Float: `2.345.round_to(2)`
  is `2.35`; a negative `digits` rounds to tens, hundreds...: `1234.5.round_to(-2) == 1200.0`.
  `digits` is clamped to -15..15. NaN and infinities stay as they are.

- `fn truncate(self) -> Int`

  As `to_i`: toward zero. Panics with a `FloatDomainError` for NaN or an infinity.

- `fn abs(self) -> Float`
- `fn sqrt(self) -> Float`

  NaN for a negative number (no panic), as the other math methods return NaN or an infinity
  outside their domain (`(-1.0).ln` is NaN, `0.0.ln` is `-Infinity`).

- `fn cbrt(self) -> Float`
- `fn sin(self) -> Float`
- `fn cos(self) -> Float`
- `fn tan(self) -> Float`
- `fn asin(self) -> Float`
- `fn acos(self) -> Float`
- `fn atan(self) -> Float`
- `fn atan2(self, x: Float) -> Float`
- `fn exp(self) -> Float`
- `fn ln(self) -> Float`
- `fn log10(self) -> Float`
- `fn log2(self) -> Float`
- `fn is_nan(self) -> Bool`

  True only for NaN (which is not equal to itself).

- `fn is_infinite(self) -> Bool`
- `fn is_finite(self) -> Bool`

  False for NaN and the infinities.

- `fn min(self, other: Float) -> Float`

  The smaller of the two. If either is NaN the result is `other`.

- `fn max(self, other: Float) -> Float`

  The larger of the two. If either is NaN the result is `other`.

- `fn format(self, decimals: Int) -> String`

  Fixed-point text with exactly `decimals` digits after the point (clamped to 0..30), no
  exponent: `3.14159.format(2) == "3.14"`. Rounds the exact binary value, ties to even
  (`2.5.format(0) == "2"`), unlike `round_to`. Infinities are `inf` / `-inf`, NaN is `NaN`.

## Bool

- `fn to_s(self) -> String`
- `fn hash(self) -> Int`

## Nil

- `fn to_s(self) -> String`

## String

- `fn to_s(self) -> String`
- `fn hash(self) -> Int`
- `fn <=>(self, other: String) -> Int`
- `fn +(self, other: String) -> String`
- `fn *(self, times: Int) -> String`

  The text repeated `times` times (`""` for 0). Panics with an `ArgumentError` if negative.

- `fn [](self, index: Int) -> String`

  The character (Unicode scalar value) at character position `index`; negative counts from the
  end. Indexing with a range (`s[1..3]`) returns those characters, clipped to the string.

- `fn length(self) -> Int`

  The number of characters (Unicode scalar values), not bytes; see `byte_length`.

- `fn byte_length(self) -> Int`

  The number of bytes in the UTF-8 encoding.

- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn upcase(self) -> String`

  Unicode uppercase; may change the length (`"straße".upcase == "STRASSE"`).

- `fn downcase(self) -> String`

  Unicode lowercase.

- `fn capitalize(self) -> String`

  The first character uppercased and the rest lowercased: `"hELLO wORLD"` becomes
  `"Hello world"`.

- `fn swapcase(self) -> String`
- `fn reverse(self) -> String`

  The characters in reverse order (by Unicode scalar value, so combining marks move).

- `fn strip(self) -> String`

  Without leading and trailing Unicode whitespace (including `\n`, `\t` and U+00A0).

- `fn lstrip(self) -> String`

  Without leading Unicode whitespace.

- `fn rstrip(self) -> String`

  Without trailing Unicode whitespace.

- `fn trim(self) -> String`
- `fn chomp(self) -> String`

  Without one trailing `\n` or `\r\n` (only one: `"a\n\n".chomp == "a\n"`).

- `fn split(self, sep: String | Regex = "", limit: Int = 0) -> List[String]`

  The pieces between occurrences of `sep` (a string or a `Regex`); with the default `""`, the
  words between runs of whitespace. With a positive `limit`, at most `limit` pieces: the last
  holds the rest of the text.

- `fn lines(self) -> List[String]`

  The lines without their `\n` or `\r\n` endings; a final line ending adds no empty line, and
  `"".lines` is `[]`.

- `fn chars(self) -> List[String]`

  Each character (Unicode scalar value) as a one-character string.

- `fn bytes(self) -> List[Int]`

  The UTF-8 bytes, each 0 to 255 (`"é".bytes == [195, 169]`).

- `fn to_bytes(self) -> Bytes`

  The UTF-8 encoding as `Bytes`.

- `fn from_bytes(bytes: Bytes) -> Result[String, String]`

  Decodes UTF-8, or returns why the bytes are not valid UTF-8.

- `fn from_bytes_lossy(bytes: Bytes) -> String`

  Decodes UTF-8, replacing invalid sequences with U+FFFD.

- `fn starts_with(self, prefix: String) -> Bool`
- `fn ends_with(self, suffix: String) -> Bool`
- `fn contains(self, sub: String) -> Bool`
- `fn include(self, sub: String) -> Bool`
- `fn index_of(self, sub: String, from: Int = 0) -> Int?`

  The character position of the first occurrence of `sub` at or after character position
  `from` (a negative `from` counts as 0), or nil. An empty `sub` is found at `from`, or at the
  end if `from` is past it.

- `fn rindex_of(self, sub: String) -> Int?`

  The character position of the last occurrence of `sub`, or nil.

- `fn replace(self, old: String, new: String) -> String`

  Every non-overlapping occurrence of `old` replaced, left to right; an empty `old` changes
  nothing. For patterns, see `gsub`.

- `fn replace_first(self, old: String, new: String) -> String`

  The first occurrence of `old` replaced. For patterns, see `sub`.

- `fn slice(self, start: Int, len: Int) -> String`

  Up to `len` characters from character position `start` (negative counts from the end). Never
  panics: the range is clipped to the string, so it may be shorter or `""`.

- `fn substring(self, start: Int, end: Int) -> String`

  The characters from position `start` up to (not including) `end`; negative positions count
  from the end. Clipped to the string; `""` if `end <= start`. Never panics.

- `fn count(self, sub: String) -> Int`

  The number of non-overlapping occurrences of `sub` (`"aaaa".count("aa") == 2`); 0 for `""`.

- `fn to_i(self) -> Int?`

  The decimal integer, or nil if the text is not one. Surrounding whitespace, a leading `+` or
  `-` and `_` separators are allowed (`" 1_000 "` is 1000); there is no size limit. For other
  bases, see `parse_int`.

- `fn to_f(self) -> Float?`

  The number, or nil if the text is not one. Surrounding whitespace, exponents (`"1e3"`) and
  `inf`, `infinity` and `NaN` (any case) are allowed; `_` is not.

- `fn parse_int(self, radix: Int = 10) -> Int?`

  The integer in base `radix` (digits and letters of either case, an optional sign, surrounding
  whitespace), or nil. No `0x`-style prefix. Panics with an `ArgumentError` unless `radix` is 2
  to 36.

- `fn ord(self) -> Int`

  The Unicode code point of the first character (`"é".ord == 233`). Panics with an
  `ArgumentError` on an empty string.

- `fn center(self, width: Int, pad: String = " ") -> String`

  The text centered in `width` characters, padded with repeats of `pad` (any extra one goes on
  the right). Unchanged if already that wide or `pad` is empty.

- `fn ljust(self, width: Int, pad: String = " ") -> String`

  The text padded on the right to `width` characters with repeats of `pad`. Unchanged if already
  that wide or `pad` is empty.

- `fn rjust(self, width: Int, pad: String = " ") -> String`

  The text padded on the left to `width` characters with repeats of `pad`: `"7".rjust(3, "0")` is
  `"007"`. Unchanged if already that wide or `pad` is empty.

- `fn format(self, args: List[Dyn]) -> String`

  Fills a runtime template: `{}` takes the next argument's `to_s`, `{:spec}` formats it with a
  spec as in `"#{x:spec}"`, `{{` and `}}` are literal braces. Panics with a `FormatError` on a bad
  spec or when the placeholders and arguments do not match in number.

- `fn is_digit(self) -> Bool`

  True if the text is non-empty and all ASCII digits `0`-`9` (other scripts' digits are not).
  The `is_` tests are all false for `""`.

- `fn is_alpha(self) -> Bool`

  True if non-empty and every character is a Unicode letter (`"é"` is).

- `fn is_alnum(self) -> Bool`

  True if non-empty and every character is a Unicode letter or digit.

- `fn is_space(self) -> Bool`

  True if non-empty and every character is Unicode whitespace.

- `fn is_upper(self) -> Bool`

  True if non-empty and no letter in it is lowercase; non-letters are ignored, so `"AB1"` and
  `"12"` are upper.

- `fn is_lower(self) -> Bool`

  True if non-empty and no letter in it is uppercase; non-letters are ignored.

- `fn delete_prefix(self, prefix: String) -> String`

  The text without `prefix` if it starts with it, else unchanged.

- `fn delete_suffix(self, suffix: String) -> String`

  The text without `suffix` if it ends with it, else unchanged.

- `fn squeeze(self) -> String`

  Each run of a repeated character collapsed to one: `"aaabcc".squeeze == "abc"`.

- `fn tr(self, from: String, to: String) -> String`

  Each character found in `from` replaced by the one at the same position in `to` (or the
  last character of `to` if it is shorter): `"hello".tr("el", "ip") == "hippo"`. There are no
  ranges: `-` is an ordinary character. With an empty `to`, nothing changes.

- `fn each_char(self, f: (fn(String))? = nil) -> List[String]`

  Calls `f` with each character; returns the characters (so without a block, `chars`).

- `fn repeat(self, n: Int) -> String`

  As `self * n`: panics with an `ArgumentError` if `n` is negative.

- `fn matches(self, re: Regex) -> Bool`

  Regex conveniences: `re.matches(self)`, the texts of `re.find_all(self)`, `re.replace(self, r)`
  and `re.replace_all(self, r)`.

- `fn scan(self, re: Regex) -> List[String]`
- `fn sub(self, re: Regex, replacement: String) -> String`
- `fn gsub(self, re: Regex, replacement: String) -> String`

## List[T]

- `fn new() -> List[T]`
- `fn with_capacity(n: Int) -> List[T]`

  An empty list with room for `n` items. The capacity is a hint: at most 2^24 items are
  reserved up front, and the list grows past that as usual. Panics if `n` is negative.

- `fn filled(n: Int, value: T) -> List[T]`

  `n` copies of `value` (the same object `n` times, not copies of it). Panics with an
  `ArgumentError` if `n` is negative, or a MemoryError if the list cannot fit in memory.

- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn push(mut self, item: T)`

  Appends `item` at the end (amortized O(1)).

- `fn append(mut self, item: T)`

  As `push`: appends one item. To append a whole list, use `concat`.

- `fn pop(mut self) -> T?`

  Removes and returns the last item, or nil if the list is empty.

- `fn shift(mut self) -> T?`

  Removes and returns the first item, or nil if the list is empty. It moves every other item;
  use a `Deque` for a queue.

- `fn unshift(mut self, item: T)`

  Inserts `item` at the front, moving every other item (see `Deque`).

- `fn insert(mut self, index: Int, item: T)`

  Inserts `item` before position `index`, so `index == length` appends; a negative `index`
  counts from the end with -1 appending. Panics with an `IndexError` past either end.

- `fn remove_at(mut self, index: Int) -> T`

  Removes and returns the item at `index` (negative counts from the end). Panics with an
  `IndexError` if out of range.

- `fn clear(mut self)`
- `fn concat(mut self, other: List[T])`

  Appends every item of `other`, in place (`xs.concat(xs)` doubles `xs`). `+` returns a new list
  instead.

- `fn [](self, index: Int) -> T`

  The item at `index`; negative counts from the end (`xs[-1]` is the last). Panics with an
  `IndexError` if out of range; `get` returns nil instead. Indexing with a range (`xs[1..3]`)
  returns a new list of those items, clipped to the list.

- `fn []=(mut self, index: Int, value: T)`

  Replaces the item at `index` (negative counts from the end). Panics with an `IndexError` if
  out of range: it never grows the list (use `push`).

- `fn get(self, index: Int) -> T?`

  The item at `index` (negative counts from the end), or nil if out of range.

- `fn slice(self, start: Int, len: Int) -> List[T]`

  A new list of up to `len` items from position `start` (negative counts from the end). Never
  panics: the range is clipped to the list, so the result may be shorter or empty.

- `fn dup(self) -> List[T]`

  A shallow copy: a new list holding the same items.

- `fn reverse(self) -> List[T]`

  A new list with the items in reverse order; `reverse_in_place` reverses this one.

- `fn reverse_in_place(mut self)`
- `fn swap(mut self, i: Int, j: Int)`

  Exchanges the items at `i` and `j` (negative counts from the end). Panics with an
  `IndexError` if either is out of range.

- `fn truncate(mut self, len: Int)`

  Removes the items from position `len` on; does nothing if the list is not longer. A negative
  `len` empties it.

- `fn +(self, other: List[T]) -> List[T]`

  A new list: this one's items followed by `other`'s. Neither list changes.

- `fn first(self) -> T?`

  The first item, or nil if the list is empty.

- `fn last(self) -> T?`

  The last item, or nil if the list is empty.

- `fn take_last(self, n: Int) -> List[T]`

  The last `n` items (all of them if there are fewer; none if `n <= 0`).

- `fn each(self, f: fn(T))`
- `fn each_with_index(self, f: fn(T, Int))`
- `fn each_index(self, f: fn(Int))`
- `fn map[U](self, f: fn(T) -> U) -> List[U]`
- `fn map_with_index[U](self, f: fn(T, Int) -> U) -> List[U]`
- `fn select(self, f: fn(T) -> Bool) -> List[T]`
- `fn reject(self, f: fn(T) -> Bool) -> List[T]`
- `fn reduce[U](self, init: U, f: fn(U, T) -> U) -> U`
- `fn index_of(self, item: T) -> Int?`

  The position of the first item `== item`, or nil.

- `fn contains(self, item: T) -> Bool`
- `fn delete(mut self, item: T) -> Bool`

  Removes the first item `== item`; returns whether there was one. Later equal items stay.

- `fn delete_if(mut self, f: fn(T) -> Bool)`

  Removes, in place, every item for which `f` is true. Returns nothing; `reject` returns a new
  list instead.

- `fn fill(mut self, value: T)`

  Sets every existing item to `value`, in place (the length does not change).

- `fn rotate(self, n: Int = 1) -> List[T]`

  A new list rotated left by `n` (right if negative), wrapping around:
  `[1, 2, 3, 4].rotate(1) == [2, 3, 4, 1]`, `[1, 2, 3, 4].rotate(-1) == [4, 1, 2, 3]`.

- `fn chunk(self, size: Int) -> List[List[T]]`

  The items in consecutive lists of `size` (the last may be shorter). `size` must be positive:
  with 0 or less it never returns.

- `fn sort_by_keys[K: Comparable](self, keys: List[K]) -> List[T]`

  The items reordered as a stable sort of `keys` (one per item, at the same position) orders
  them. Panics with an `ArgumentError` if the lengths differ.

- `fn sort_with(self, f: fn(T, T) -> Int) -> List[T]`

  A stable sort by the comparator `f`: negative, zero or positive as its first argument sorts
  before, with or after its second.

- `fn min_by[K: Comparable](self, f: fn(T) -> K) -> T?`

  As `Enumerable`'s, natively: the first item with the smallest (largest) key.

- `fn max_by[K: Comparable](self, f: fn(T) -> K) -> T?`
- `fn par_map[U](self, f: fn(T) -> U) -> List[U]`

  `map` on a pool of worker threads; the results keep the items' order. The items and what
  `f` captures must be frozen or shareable. If `f` panics, `par_map` panics with that message.

- `fn par_each(self, f: fn(T))`

  Calls `f` with every item on a pool of worker threads, in no particular order; returns when
  all calls are done. Requirements as for `par_map`.

- `fn par_reduce(self, identity: T, f: fn(T, T) -> T) -> T`

  Combines the items with `f` on worker threads: each chunk is folded from `identity`, then the
  chunk results are combined. So `f` must be associative and `identity` neutral for it (0 for
  `+`); an empty list gives `identity`.

- `fn par_select(self, f: fn(T) -> Bool) -> List[T]`

  `select` on worker threads; the kept items stay in order. Requirements as for `par_map`.

## List[T] (where T: Comparable)

Sorting by the natural order needs `Comparable` items (tuples and lists of comparable items are
compared lexicographically).

- `fn sorted(self) -> List[T]`

  A new, stably sorted list; the list itself is unchanged. Panics with an `ArgumentError` if
  two items cannot be compared (a NaN, for one).

- `fn sort_in_place(mut self)`

  Sorts this list, stably, in place; panics as `sorted` does.

## List[List[U]]

- `fn flatten(self) -> List[U]`

  The inner lists' items in one new list (one level deep).

## Deque[T]

A double-ended queue: a growable ring buffer with O(1) amortized `push`/`pop` at the back,
`unshift`/`shift` at the front, and O(1) indexing (negative indices count from the end). Use it
instead of `List.shift`/`unshift`, which move every item. Ownership is as for `List`: a deque is
owned by one thread, `freeze` makes it (and its items) immutable and shareable, and `Mutex`
shares a mutable one. It is `Enumerable`; `==` compares items in order and `Json.stringify`
writes it as an array.

- `fn init(self)`
- `fn from(items: Enumerable[T]) -> Deque[T]`

  A deque of `items`, in order.

- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn push(self, item: T)`

  Appends at the back.

- `fn pop(self) -> T?`

  Removes and returns the last item, or nil if empty.

- `fn unshift(self, item: T)`

  Prepends at the front.

- `fn shift(self) -> T?`

  Removes and returns the first item, or nil if empty.

- `fn first(self) -> T?`

  The front item, or nil if empty.

- `fn last(self) -> T?`

  The back item, or nil if empty.

- `fn [](self, index: Int) -> T`

  The item at `index` from the front (negative counts from the back). Panics with an
  `IndexError` if out of range; `get` returns nil instead.

- `fn []=(self, index: Int, value: T)`

  Replaces the item at `index` (negative counts from the back). Panics with an `IndexError` if
  out of range.

- `fn get(self, index: Int) -> T?`

  The item at `index` (negative counts from the back), or nil if out of range.

- `fn clear(self)`
- `fn each(self, f: fn(T))`
- `fn to_list(self) -> List[T]`
- `fn dup(self) -> Deque[T]`
- `fn ==(self, other: Deque[T]) -> Bool`
- `fn hash(self) -> Int`
- `fn to_s(self) -> String`
- `fn inspect(self) -> String`
- `fn to_json(self) -> Dyn`

## Dict[K, V]

- `fn new() -> Dict[K, V]`
- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn [](self, key: K) -> V?`

  The value for `key`, or nil if it is absent (never panics). Use `has_key` to tell an absent
  key from a nil value.

- `fn []=(mut self, key: K, value: V)`

  Sets the value for `key`. A new key goes last in iteration order; an existing one keeps its
  place.

- `fn get(self, key: K) -> V?`

  As `d[key]`: the value, or nil.

- `fn fetch(self, key: K, default: V) -> V`

  The value for `key`, or `default` if it is absent or its value is nil. Never panics.

- `fn has_key(self, key: K) -> Bool`
- `fn contains_key(self, key: K) -> Bool`
- `fn delete(mut self, key: K) -> V?`

  Removes `key`; returns its value, or nil if it was absent.

- `fn clear(mut self)`
- `fn keys(self) -> List[K]`

  The keys in insertion order (as `values` and `entries` are).

- `fn values(self) -> List[V]`

  The values in insertion order of their keys.

- `fn entries(self) -> List[(K, V)]`

  The `(key, value)` pairs in insertion order.

- `fn dup(self) -> Dict[K, V]`

  A shallow copy: a new dict holding the same keys and values.

- `fn each(self, f: fn((K, V)))`
- `fn each_key(self, f: fn(K))`
- `fn each_value(self, f: fn(V))`
- `fn get_or_insert(mut self, key: K, default: V) -> V`

  The value for `key`; if it is absent (or nil), stores `default` under it first and returns
  that. `default` is evaluated either way.

- `fn merge(self, other: Dict[K, V]) -> Dict[K, V]`

  A new dict with this one's entries and then `other`'s: on a shared key `other`'s value wins
  (the key keeps this dict's position). Neither dict changes.

- `fn update(mut self, other: Dict[K, V])`

  As `merge`, in place: copies `other`'s entries into this dict, overwriting shared keys.

- `fn map_values[W](self, f: fn(V) -> W) -> Dict[K, W]`

  A new dict with the same keys (in order) and `f` applied to each value.

- `fn select_entries(self, f: fn(K, V) -> Bool) -> Dict[K, V]`

  A new dict of the entries for which `f(key, value)` is true, in order.

- `fn invert(self) -> Dict[V, K]`

  Values as keys and keys as values. When values repeat, the last key with that value wins.

## Dict[K, V] (where K: Comparable)

- `fn sort_by_key(self) -> List[(K, V)]`

  The `(key, value)` pairs sorted by key (the dict itself keeps its order).

## Dict[K, V] (where V: Comparable)

- `fn sort_by_value(self) -> List[(K, V)]`

  The `(key, value)` pairs sorted by value, stably (equal values keep insertion order).

## Set[T]

- `fn new() -> Set[T]`
- `fn from(items: List[T]) -> Set[T]`

  The distinct items, in order of first occurrence.

- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn add(mut self, item: T) -> Bool`

  Adds `item`; returns true if it was not already there (false leaves the set unchanged).

- `fn remove(mut self, item: T) -> Bool`

  Removes `item`; returns true if it was there.

- `fn contains(self, item: T) -> Bool`
- `fn clear(mut self)`
- `fn to_list(self) -> List[T]`

  The items in insertion order.

- `fn dup(self) -> Set[T]`

  A shallow copy.

- `fn each(self, f: fn(T))`
- `fn union(self, other: Set[T]) -> Set[T]`

  A new set of the items in either (`a | b`): this set's in order, then `other`'s new ones.

- `fn intersection(self, other: Set[T]) -> Set[T]`

  A new set of this set's items that are also in `other` (`a & b`), in this set's order.

- `fn difference(self, other: Set[T]) -> Set[T]`

  A new set of this set's items that are not in `other` (`a - b`), in this set's order.

- `fn |(self, other: Set[T]) -> Set[T]`
- `fn &(self, other: Set[T]) -> Set[T]`
- `fn -(self, other: Set[T]) -> Set[T]`
- `fn is_subset(self, other: Set[T]) -> Bool`

  True if every item is in `other` (so a set is a subset of itself, and the empty set of any set).

- `fn is_superset(self, other: Set[T]) -> Bool`

## Range

- `fn first(self) -> Int`

  The start bound, even for an empty range: `(5..1).first == 5`.

- `fn last(self) -> Int`

  The end bound as written, even when excluded: `(1...5).last == 5`, not 4.

- `fn is_exclusive(self) -> Bool`

  True for `a...b` (end excluded), false for `a..b`.

- `fn each(self, f: fn(Int))`
- `fn length(self) -> Int`

  The number of integers in the range; 0 if it is empty (`5..1` counts nothing, it does not
  count down).

- `fn size(self) -> Int`
- `fn contains(self, x: Int) -> Bool`

  True if `first <= x` and `x` is before (`...`) or at most (`..`) `last`.

- `fn step(self, n: Int) -> List[Int]`

  Every `n`th integer from `first`: `(1..10).step(3) == [1, 4, 7, 10]`. `n` must be positive:
  with 0 or less it never returns.

- `fn to_list(self) -> List[Int]`
- `fn sum(self) -> Int`

  The total of the integers in the range; 0 if it is empty.

## StringBuilder

- `fn new() -> StringBuilder`
- `fn push(mut self, s: String)`

  Appends `s`.

- `fn push_value(mut self, v: Dyn)`

  Appends `v.to_s` (as string interpolation does).

- `fn push_char(mut self, code: Int)`

  Appends the character with Unicode code point `code`. Panics with a `RangeError` if it is not
  a valid one, as `Int.chr` does.

- `fn to_s(self) -> String`

  The text built so far (the builder can keep growing).

- `fn length(self) -> Int`

  The length so far in characters, like `String.length`.

- `fn clear(mut self)`

## Mutex[T]

- `fn new(move value: T) -> Mutex[T]`

  A mutex guarding `value`, which becomes shared (it and what it holds may then be used from
  any thread, under the lock).

- `fn lock[R](self, f: fn(T) -> R) -> R`

  Calls `f` with the value while holding the lock and returns `f`'s result. Mutate the value
  inside `f`; do not keep it after.

- `fn update(self, f: fn(T) -> T) -> T`

  Replaces the value with `f(value)` under the lock; returns the new value.

- `fn get(self) -> T`

  The value, read under the lock (for an object, the object itself, not a copy).

- `fn set(self, move value: T)`

  Replaces the value under the lock.

## Atomic[T]

- `fn new(value: T) -> Atomic[T]`
- `fn get(self) -> T`
- `fn set(self, value: T)`
- `fn swap(self, value: T) -> T`

  Stores `value` and returns the previous one, atomically.

- `fn compare_and_set(self, expected: T, value: T) -> Bool`

  Stores `value` if the current value `==` `expected`; returns whether it did.

- `fn add(self, delta: Int) -> Int`

  Adds `delta` atomically and returns the new value. Panics with a `TypeError` unless the
  value is an `Int`.

- `fn update(self, f: fn(T) -> T) -> T`

  Replaces the value with `f(value)` atomically (retrying `f` if another thread changed it
  meanwhile, so `f` should have no side effects); returns the new value.

## Channel[T]

- `fn new(capacity: Int = 0) -> Channel[T]`

  A channel holding at most `capacity` queued items (`send` waits when it is full); 0, the
  default, means unbounded. Panics with an `ArgumentError` if negative.

- `fn send(self, move item: T)`

  Queues `item` (moving it, if owned, to the receiver), waiting while a bounded channel is
  full. Panics with a `ChannelError` if the channel is closed.

- `fn recv(self) -> T?`

  The next item, waiting for one; nil once the channel is closed and drained.

- `fn try_recv(self) -> T?`

  The next item if one is queued, else nil at once (also when the channel is closed and empty).

- `fn close(self)`

  Closes the channel: later `send`s panic, receivers get the queued items and then nil.
  Closing twice is allowed.

- `fn is_closed(self) -> Bool`
- `fn length(self) -> Int`

  The number of queued items.

- `fn each(self, f: fn(T))`

  Calls `f` with each item received until the channel is closed and drained.

## Thread[T]

- `fn join(self) -> Result[T, String]`

  Waits for the thread and returns its result, or `Err` with the message of the panic that
  ended it. Only the first join gets the result: later ones return
  `Err("thread was already joined")`.

- `fn is_done(self) -> Bool`

  True once the thread has finished (returned or panicked); never waits.

## Fiber[T]

A fiber: a lightweight cooperative thread for I/O-bound work. It runs on the OS thread that
spawned it, interleaving with that thread's other fibers wherever one waits (channels, `join`,
`sleep`, file and console I/O, `Fiber.yield`), so its block may capture owned values. A thread
finishes only after its fibers do.

- field `id: Int`
- `fn start(f: fn() -> T) -> Fiber[T]`
- `fn join(self) -> Result[T, String]`

  Waits for the fiber and returns its result, or the message of the panic that ended it.

- `fn is_done(self) -> Bool`
- `fn yield()`

  Lets the thread's other fibers run.

## Math

- `const PI: Float = 3.141592653589793`
- `const E: Float = 2.718281828459045`
- `const INFINITY: Float = 1.0 / 0.0`
- `fn sqrt(x: Float) -> Float`

  The square root; NaN for a negative `x` (the Math functions return NaN or an infinity outside
  their domain rather than panicking).

- `fn pow(x: Float, y: Float) -> Float`

  `x` to the power `y`, in floating point (`Math.pow(2.0, -1.0) == 0.5`). For exact integer
  powers use `Int.pow` or `**`.

- `fn sin(x: Float) -> Float`

  The sine of `x` radians (`cos` and `tan` take radians too).

- `fn cos(x: Float) -> Float`
- `fn tan(x: Float) -> Float`
- `fn atan2(y: Float, x: Float) -> Float`

  The angle of the point `(x, y)` from the positive x axis, in radians from -π to π. Note the
  argument order: `y` first.

- `fn exp(x: Float) -> Float`
- `fn log(x: Float) -> Float`

  The natural logarithm: `-Infinity` for 0, NaN for a negative `x`.

- `fn log10(x: Float) -> Float`

  The base-10 logarithm: `-Infinity` for 0, NaN for a negative `x`.

- `fn hypot(x: Float, y: Float) -> Float`

  `sqrt(x² + y²)`, without overflow or underflow in between.

- `fn min_int(a: Int, b: Int) -> Int`
- `fn max_int(a: Int, b: Int) -> Int`

## File

Whole-file operations. None of them panics on an I/O failure: they return `Err` with
`"<path>: <reason>"` (`"x.txt: No such file or directory (os error 2)"`).

- `fn read(path: String) -> Result[String, String]`

  The whole file as text. `Err` if it is missing, unreadable, a directory or not valid UTF-8
  (see `read_bytes`).

- `fn write(path: String, contents: String) -> Result[Nil, String]`

  Creates or truncates the file and writes `contents`. The directory must exist.

- `fn append(path: String, contents: String) -> Result[Nil, String]`

  Appends `contents`, creating the file if it is missing.

- `fn exists(path: String) -> Bool`

  True if `path` exists, whether file or directory (see `Dir.exists`).

- `fn delete(path: String) -> Result[Nil, String]`

  Deletes a file (not a directory; see `Dir.remove`). `Err` if it is missing.

- `fn read_lines(path: String) -> Result[List[String], String]`

  The file's lines without their `\n` or `\r\n` endings, as `String.lines` splits them.

- `fn list_dir(path: String) -> Result[List[String], String]`

  Entry names (not paths) in the directory, sorted; as `Dir.list`.

- `fn metadata(path: String) -> Result[Metadata, String]`

  Size, kind and modification time; `Err` if `path` does not exist.

- `fn read_bytes(path: String) -> Result[Bytes, String]`

  The file's contents as they are, for binary files (`read` needs valid UTF-8).

- `fn write_bytes(path: String, data: Bytes) -> Result[Nil, String]`

  Creates or truncates the file and writes `data`.

## Bytes (frozen)

An immutable byte string. Like `String` it is frozen, so it is shared freely between threads
and fibers; build one from a list of byte values, from text (`s.to_bytes`), from hex or base64,
or by reading a file (`File.read_bytes`). To assemble bytes piece by piece, collect them in a
`List[Int]` and convert once with `Bytes.new`, or join pieces with `+`.

- field `handle: NativeHandle`
- `fn new(values: List[Int] = []) -> Bytes`

  Bytes with the given values, each in 0..255. Panics with an `ArgumentError` for any other.

- `fn from_hex(text: String) -> Result[Bytes, String]`

  Parses hex digits (either case, no separators).

- `fn from_base64(text: String) -> Result[Bytes, String]`

  Parses standard base64 (with padding).

- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn [](self, index: Int) -> Int`

  The byte at `index` (negative counts from the end); panics out of range.

- `fn get(self, index: Int) -> Int?`

  The byte at `index` (negative counts from the end), or nil if out of range.

- `fn slice(self, start: Int, len: Int) -> Bytes`

  Up to `len` bytes from `start` (negative counts from the end), clipped to the bytes; never
  panics.

- `fn +(self, other: Bytes) -> Bytes`
- `fn ==(self, other: Bytes) -> Bool`
- `fn hash(self) -> Int`
- `fn to_list(self) -> List[Int]`
- `fn each(self, f: fn(Int))`
- `fn to_hex(self) -> String`

  Lowercase hex, two digits per byte.

- `fn to_base64(self) -> String`

  Standard base64 with padding.

- `fn to_s(self) -> String`

  `Bytes(` hex `)`. Decode text with `String.from_bytes`.

## Digest

Message digests of text (its UTF-8 bytes) or bytes, as lowercase hex. MD5 and SHA-1 are
broken for security purposes; use them only for checksums and compatibility.

- `fn md5(data: String | Bytes) -> String`
- `fn sha1(data: String | Bytes) -> String`
- `fn sha256(data: String | Bytes) -> String`
- `fn sha512(data: String | Bytes) -> String`

## Csv

Comma-separated values (RFC 4180). A field holding the separator, a quote or a line break is
quoted, with its quotes doubled. Rows may end with `\n` or `\r\n`; blank lines are skipped. A
quote inside an unquoted field is kept as text. `sep` must be a single character.

- `fn parse(text: String, sep: String = ",") -> Result[List[List[String]], String]`

  The rows of `text`, or an error for an unterminated quoted field or text after one.

- `fn parse_records(text: String, sep: String = ",") -> Result[List[Dict[String, String]], String]`

  The rows after the first as dicts keyed by the first (header) row's fields. A row with
  fewer fields lacks the missing keys; one with more fields is an error.

- `fn format(rows: List[List[String]], sep: String = ",") -> String`

  Every row followed by `\n`.

- `fn format_row(row: List[String], sep: String = ",") -> String`

  One row, without a line ending.

- `fn read(path: String, sep: String = ",") -> Result[List[List[String]], String]`
- `fn write(path: String, rows: List[List[String]], sep: String = ",") -> Result[Nil, String]`

## Metadata

What `File.metadata` reports. `modified` is in seconds since the Unix epoch, like `Time.now`.

- field `size: Int`
- field `is_dir: Bool`
- field `is_file: Bool`
- field `modified: Float`

## Dir

- `fn list(path: String) -> Result[List[String], String]`

  Entry names (not paths), sorted.

- `fn create(path: String, recursive: Bool = false) -> Result[Nil, String]`

  With `recursive`, creates missing parents too and succeeds if the directory exists.

- `fn remove(path: String, recursive: Bool = false) -> Result[Nil, String]`

  Without `recursive` the directory must be empty.

- `fn exists(path: String) -> Bool`
- `fn current() -> String`
- `fn walk(path: String) -> Result[List[String], String]`

  Every file and directory under `path`, recursively, as paths that start with `path`, sorted.
  Symbolic links are listed but not followed.

- `fn glob(pattern: String) -> Result[List[String], String]`

  The paths matching `pattern`, sorted. In each `/`-separated component, `*` matches any run of
  characters, `?` one character and `[abc]` / `[a-z]` / `[!abc]` one of a set; a `**` component
  matches any number of directories (at the end, everything beneath, and the directory itself).
  Names starting with `.` match only a pattern component that starts with `.`. A relative
  pattern is relative to the current directory.

## Path

Path strings. `join` uses `/`, which every supported OS accepts; the others accept the host
separators.

- `fn join(base: String, part: String) -> String`

  `base/part`, or `part` alone if it is absolute.

- `fn join_all(parts: List[String]) -> String`

  The parts joined left to right as by `join`, so an absolute part restarts the path:
  `Path.join_all(["a", "/b", "c"]) == "/b/c"`; `""` for no parts.

- `fn basename(path: String) -> String`

  The last component: `Path.basename("a/b.txt") == "b.txt"`.

- `fn dirname(path: String) -> String`

  Everything before the last component, or "." if there is none.

- `fn extension(path: String) -> String?`

  The extension without its dot, or nil: `Path.extension("a/b.tar.gz") == "gz"`.

- `fn stem(path: String) -> String`

  The last component without its extension.

- `fn is_absolute(path: String) -> Bool`

  True if the path starts at the root (`/a` on Unix). `~` is not expanded, so `~/a` is
  relative.

## Regex (frozen)

A compiled regular expression, in the syntax of Rust's `regex` crate (no look-around or
backreferences; matching takes linear time). A Regex is frozen: compile it once and share it
freely between threads and fibers.

- field `pattern: String`
- field `handle: NativeHandle`
- `fn new(pattern: String) -> Result[Regex, String]`

  Compiles `pattern`, or returns the syntax error.

- `fn escape(text: String) -> String`

  `text` with every regex metacharacter escaped.

- `fn matches(self, text: String) -> Bool`
- `fn find(self, text: String) -> Match?`

  The leftmost match, or nil.

- `fn find_all(self, text: String) -> List[Match]`

  All non-overlapping matches, left to right.

- `fn replace(self, text: String, replacement: String) -> String`

  Replaces the first match. In `replacement`, `$1` / `${1}` insert a positional group, `$name` /
  `${name}` a named one, and `$$` a literal `$`.

- `fn replace_all(self, text: String, replacement: String) -> String`
- `fn split(self, text: String) -> List[String]`

  The pieces of `text` between matches.

- `fn capture_names(self) -> List[String]`
- `fn to_s(self) -> String`

## Match

One regex match. `start` and `end` are character positions (`end` exclusive), like `String`
indexing. `groups[0]` is the whole match; a group that did not participate is nil.

- field `text: String`
- field `start: Int`
- field `end: Int`
- field `groups: List[String?]`
- field `named_groups: Dict[String, String]`
- `fn [](self, index: Int) -> String?`

  Group `index` (0 is the whole match), or nil.

- `fn captures(self) -> List[String?]`

  The positional groups after the whole match.

- `fn named(self, name: String) -> String?`

  A named group, or nil.

- `fn to_s(self) -> String`

## Process

Running other programs. `cmd` is looked up on the PATH; `args` are passed as they are (no
shell). `env` adds to (or overrides) the inherited environment; `dir` sets the working
directory. Waiting parks the calling fiber (the I/O pool waits) instead of blocking its thread.

- `fn run(cmd: String, args: List[String] = [], env: Dict[String, String] = [:], dir: String? = nil, input: String? = nil) -> Result[Output, String]`

  Runs to completion, feeding it `input` on stdin (or none), and captures its output.

- `fn start(cmd: String, args: List[String] = [], env: Dict[String, String] = [:], dir: String? = nil) -> Result[Subprocess, String]`

  Starts a process with piped stdin and stdout (stderr goes to ours).

## Output

The result of `Process.run`. `status` is the exit code, or -1 if a signal ended the process.

- field `status: Int`
- field `stdout: String`
- field `stderr: String`
- `fn success(self) -> Bool`

## Subprocess

A process started by `Process.start`. Its pipes are locked separately, so one fiber may write
while another reads.

- field `pid: Int`
- field `handle: NativeHandle`
- `fn write(self, text: String) -> Result[Nil, String]`
- `fn close_stdin(self)`

  Sends end-of-file on its stdin.

- `fn read_line(self) -> String?`

  The next line of its stdout without the line ending, or nil at end of output.

- `fn read_all(self) -> String`

  The rest of its stdout.

- `fn wait(self) -> Result[Int, String]`

  Closes stdin and waits for the process to exit; returns its status (-1 if a signal ended it).

- `fn kill(self) -> Result[Nil, String]`

## Time

- `fn now() -> Float`

  Seconds since the Unix epoch (1970-01-01T00:00:00Z) on the wall clock.

- `fn monotonic() -> Float`

  Seconds since the program started, on the monotonic clock (which never jumps, unlike the
  wall clock). `Instant` measures elapsed time with the same clock and nanosecond precision.

- `fn millis() -> Int`

  Milliseconds since the Unix epoch on the wall clock. (Before 0.4 it counted from the start of
  the program; use `Instant` or `Time.monotonic` for that.)

## Duration (frozen)

A signed span of time with nanosecond precision: `Duration.from_secs(90)`,
`Duration.from_millis(250)`, `a.since(b)`. It is frozen, so it crosses threads freely.
`to_s` writes the units that matter: `1h2m3.5s`, `250ms`, `-1.5µs`, `0s`.

- field `nanos: Int`
- `fn from_nanos(n: Int) -> Duration`

  The `from_` constructors take a count of the unit, which may be negative.

- `fn from_micros(n: Int) -> Duration`
- `fn from_millis(n: Int) -> Duration`
- `fn from_secs(n: Int) -> Duration`
- `fn from_mins(n: Int) -> Duration`
- `fn from_hours(n: Int) -> Duration`
- `fn from_days(n: Int) -> Duration`
- `fn from_secs_f(secs: Float) -> Duration`

  Rounded to the nearest nanosecond. Panics with a `FloatDomainError` for NaN or an infinity.

- `fn as_nanos(self) -> Int`
- `fn as_micros(self) -> Int`

  The whole microseconds, milliseconds or seconds, truncated toward zero.

- `fn as_millis(self) -> Int`
- `fn as_secs(self) -> Int`
- `fn as_secs_f(self) -> Float`

  The length in seconds, with the fraction.

- `fn is_zero(self) -> Bool`
- `fn is_negative(self) -> Bool`
- `fn abs(self) -> Duration`
- `fn neg(self) -> Duration`
- `fn +(self, other: Duration) -> Duration`
- `fn -(self, other: Duration) -> Duration`
- `fn *(self, k: Int) -> Duration`
- `fn /(self, k: Int) -> Duration`

  Truncates toward zero, to whole nanoseconds. Panics with a `ZeroDivisionError` if `k` is 0.

- `fn <=>(self, other: Duration) -> Int`
- `fn ==(self, other: Duration) -> Bool`
- `fn hash(self) -> Int`
- `fn to_s(self) -> String`

## Instant (frozen)

A reading of the monotonic clock, for measuring elapsed time:
`start = Instant.now(); work(); puts(start.elapsed())`. Frozen.

- field `nanos: Int`

  Nanoseconds since the program started.

- `fn now() -> Instant`

  The current reading of the monotonic clock.

- `fn elapsed(self) -> Duration`

  The time since this instant.

- `fn since(self, earlier: Instant) -> Duration`

  `self - earlier`, negative if `earlier` is later.

- `fn +(self, d: Duration) -> Instant`
- `fn -(self, d: Duration) -> Instant`
- `fn <=>(self, other: Instant) -> Int`
- `fn ==(self, other: Instant) -> Bool`
- `fn hash(self) -> Int`

## DateTime (frozen)

A point in time with a fixed offset from UTC, read on the proleptic Gregorian calendar. Two
DateTimes are equal (and compare) by the instant they denote, whatever their offsets. Frozen.

```catallaxy
t = DateTime.of(2024, 2, 29, 13, 5, offset: 3600)   # 2024-02-29T13:05:00+01:00
t.year; t.month; t.day; t.hour; t.weekday           # 2024, 2, 29, 13, 4 (Thursday)
(t + Duration.from_days(1)).to_s                    # "2024-03-01T13:05:00+01:00"
t.to_utc().format("%a %d %b %Y %H:%M %Z")            # "Thu 29 Feb 2024 12:05 UTC"
DateTime.parse("2024-02-29T12:05:00Z")              # Ok(...), equal to t
```

`DateTime.now()` uses the operating system's current UTC offset (its time zone, including
daylight saving time); `to_local` converts with the offset in force at that instant. Only the
offset is kept, not the zone, so adding a Duration never changes the offset.

- field `unix: Int`

  Whole seconds since the Unix epoch (1970-01-01T00:00:00Z); independent of `offset`.

- field `nanosecond: Int`

  Nanoseconds past `unix`, from 0 to 999_999_999.

- field `offset: Int`

  Seconds east of UTC: 3600 for +01:00, -18000 for -05:00.

- `fn now() -> DateTime`

  The current time at the operating system's local offset.

- `fn now_utc() -> DateTime`

  The current time at offset 0 (UTC).

- `fn from_unix(secs: Int, nanosecond: Int = 0, offset: Int = 0) -> DateTime`

  The instant `secs` seconds (plus `nanosecond` nanoseconds) after the epoch, at `offset`.

- `fn from_unix_millis(ms: Int, offset: Int = 0) -> DateTime`

  The instant `ms` milliseconds after the epoch (before it if negative), at `offset`.

- `fn of(year: Int, month: Int, day: Int, hour: Int = 0, minute: Int = 0, second: Int = 0, nanosecond: Int = 0, offset: Int = 0) -> DateTime`

  The given civil date and time at `offset` (seconds east of UTC). Panics if a field is out of
  range (`month` 1-12, `day` within the month, `hour` 0-23, `minute` and `second` 0-59,
  `nanosecond` 0-999_999_999, `offset` within ±24 hours).

- `fn parse(text: String) -> Result[DateTime, String]`

  Parses an ISO 8601 / RFC 3339 date-time: `YYYY-MM-DD`, optionally followed by `T` (or a
  space) and `HH:MM`, `HH:MM:SS` or `HH:MM:SS.fraction`, then optionally `Z` or an offset
  `+HH:MM` / `+HHMM` / `+HH`. Without an offset the time is taken as UTC.

- `fn year(self) -> Int`

  The calendar fields, as seen at this DateTime's offset.

- `fn month(self) -> Int`

  1 to 12.

- `fn day(self) -> Int`

  1 to 31.

- `fn hour(self) -> Int`
- `fn minute(self) -> Int`
- `fn second(self) -> Int`
- `fn weekday(self) -> Int`

  The day of the week: 0 for Sunday to 6 for Saturday (like `%w`).

- `fn yday(self) -> Int`

  The day of the year: 1 to 366.

- `fn days(self) -> Int`

  Days from 1970-01-01 to this date (at this offset).

- `fn second_of_day(self) -> Int`
- `fn unix_millis(self) -> Int`

  Milliseconds since the epoch.

- `fn to_f(self) -> Float`

  Seconds since the epoch, as a Float (like `Time.now`).

- `fn with_offset(self, offset: Int) -> DateTime`

  The same instant at another offset.

- `fn to_utc(self) -> DateTime`

  The same instant at offset 0.

- `fn to_local(self) -> DateTime`

  The same instant at the operating system's local offset for it.

- `fn +(self, d: Duration) -> DateTime`
- `fn -(self, d: Duration) -> DateTime`
- `fn since(self, earlier: DateTime) -> Duration`

  `self - earlier`, negative if `earlier` is later.

- `fn <=>(self, other: DateTime) -> Int`
- `fn ==(self, other: DateTime) -> Bool`
- `fn hash(self) -> Int`
- `fn to_s(self) -> String`

  RFC 3339 (ISO 8601): `2024-02-29T13:05:00+01:00`, `2024-02-29T12:05:00.25Z`. The fraction
  appears only when non-zero, without trailing zeros. `DateTime.parse` reads it back.

- `fn format(self, pattern: String) -> String`

  Formats with `strftime` directives: `%Y` year (at least 4 digits), `%y` 2-digit year, `%m`
  month 01-12, `%d` day 01-31, `%e` day padded with a space, `%j` day of the year 001-366, `%H`
  hour 00-23, `%I` hour 01-12, `%l` hour padded with a space, `%M` minute, `%S` second, `%L`
  milliseconds, `%f` microseconds, `%N` nanoseconds, `%p` AM/PM, `%P` am/pm, `%a`/`%A`
  weekday name (`Mon`/`Monday`), `%b`/`%h`/`%B` month name (`Jan`/`January`), `%w` weekday 0-6
  from Sunday, `%u` weekday 1-7 from Monday, `%s` Unix seconds, `%z` offset `+0100`, `%:z`
  offset `+01:00`, `%Z` `UTC` or the offset, `%F` = `%Y-%m-%d`, `%T` = `%H:%M:%S`, `%D` =
  `%m/%d/%y`, `%R` = `%H:%M`, `%c` = `%a %b %e %H:%M:%S %Y`, `%n` newline, `%t` tab, `%%` `%`.
  Names are English. Other directives are copied through.

## Random

Pseudo-random numbers (not cryptographically secure). Each thread has its own generator,
seeded from the clock when the thread first uses it.

- `fn seed(n: Int)`

  Reseeds the calling thread's generator, making its later results repeatable.

- `fn int(lo: Int, hi: Int) -> Int`

  A random integer from `lo` to `hi`, both inclusive. Panics with an `ArgumentError` if
  `hi < lo`.

- `fn float() -> Float`

  A random Float in `[0.0, 1.0)`: 0.0 is possible, 1.0 is not.

- `fn shuffle[T](items: List[T]) -> List[T]`

  A new list with the items in random order; `items` is unchanged.

- `fn choice[T](items: List[T]) -> T?`

  A random item, or nil if the list is empty.

## Env

The program's environment variables. `set` and `remove` change what `get` and `all` see and
what processes started with `Process` inherit, from any thread. They do not change the
environment of the `catx` process itself (that is unsound while other threads may read it), so
native code that reads the environment directly does not see them.

- `fn get(name: String) -> String?`

  The variable's value, or nil if it is not set.

- `fn set(name: String, value: String)`

  `name` must be non-empty and contain no `=`; neither may contain a NUL character. Panics with
  an `ArgumentError` otherwise.

- `fn remove(name: String)`

  Unsets the variable (nothing happens if it is not set). Panics on an invalid name, as `set`.

- `fn all() -> Dict[String, String]`

  Every variable, sorted by name.

- `fn cpu_count() -> Int`

  The number of CPUs available to the program (at least 1).

## Stdin

Standard input. Reads park the calling fiber (the I/O pool waits) instead of blocking its
thread. All of these, and the global `read_line()`, read the same buffered stream.

- `fn read_line() -> String?`

  The next line without its line ending, or nil at end of input.

- `fn lines() -> StdinLines`

  The remaining lines, read one at a time as they are iterated:
  `for line in Stdin.lines() { ... }`. A single pass: each line is read once.

- `fn read_lines() -> List[String]`

  All remaining lines at once.

- `fn read_all() -> String`

  All remaining input (invalid UTF-8 is replaced).

- `fn read_bytes() -> Bytes`

## StdinLines

The lines of standard input as an `Enumerable[String]`; see `Stdin.lines`.

- `fn each(self, f: fn(String))`

## Json

- `fn parse(text: String) -> Result[Dyn, String]`

  Parses one JSON value: objects become `Dict[String, Dyn]` (keys sorted; of duplicate keys the
  last wins), arrays `List[Dyn]`, `null` nil, numbers an `Int` (any size) when written without
  a fraction or exponent, else a `Float`. `Err("invalid JSON: ... at line L column C")` for a
  syntax error, including text after the value.

- `fn stringify(value: Dyn) -> String`

  Compact JSON. Dict keys are sorted (non-string keys become their `to_s`); lists and tuples
  become arrays; NaN and infinities `null`. A class instance is what its `to_json` returns, or
  an object of its fields; other values (enum values, sets, functions) are written as their
  `to_s` in a string.

- `fn pretty(value: Dyn) -> String`

  As `stringify`, indented by two spaces with one item per line.

## Net

- `fn connect(host: String, port: Int, timeout: Float? = nil) -> Result[Socket, String]`

  Connects to `host:port`; `timeout` (seconds) bounds the connection attempt.

- `fn listen(host: String, port: Int) -> Result[Listener, String]`

  Listens on `host:port`; port 0 picks a free port (see `Listener.port`).

## Socket

A TCP connection. Closed by `close`, or when it is garbage.

- field `peer: String`
- field `handle: NativeHandle`
- `fn read(self, max: Int = 65536) -> Result[String, String]`

  Up to `max` bytes as text (invalid UTF-8 is replaced); "" at end of stream.

- `fn read_line(self) -> Result[String?, String]`

  The next line without its line ending, or nil at end of stream.

- `fn write(self, data: String) -> Result[Int, String]`

  Writes all of `data`; returns the number of bytes written.

- `fn close(self)`
- `fn set_timeout(self, seconds: Float?)`

  Makes reads and writes fail with "timed out" after `seconds` (nil: wait forever).

## Listener

- field `port: Int`
- field `handle: NativeHandle`
- `fn accept(self) -> Result[Socket, String]`
- `fn close(self)`

## HttpResponse

- field `status: Int`
- field `headers: Dict[String, String]`
- field `body: String`
- `fn ok(self) -> Bool`

## Http

HTTP and HTTPS client requests. They run on the I/O pool, so a fiber waiting on one lets the
thread's other fibers run. Non-2xx statuses are responses, not errors.

- `fn get(url: String, headers: Dict[String, String] = [:]) -> Result[HttpResponse, String]`

  Sends a GET request with the extra `headers`. `Err` only when no response arrives (an
  invalid URL, a refused connection...).

- `fn post(url: String, body: String, headers: Dict[String, String] = [:]) -> Result[HttpResponse, String]`

  Sends a POST request with `body` and the extra `headers`; errors as for `get`.
