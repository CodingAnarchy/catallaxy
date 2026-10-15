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
- `fn count(self, f: (fn(T) -> Bool)? = nil) -> Int`

  The number of elements for which `f` is true; without a block, the number of elements.

- `fn size(self) -> Int`
- `fn any(self, f: fn(T) -> Bool) -> Bool`
- `fn all(self, f: fn(T) -> Bool) -> Bool`
- `fn none(self, f: fn(T) -> Bool) -> Bool`
- `fn find(self, f: fn(T) -> Bool) -> T?`
- `fn find_index(self, f: fn(T) -> Bool) -> Int?`
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
- `fn take_while(self, f: fn(T) -> Bool) -> List[T]`
- `fn drop(self, n: Int) -> List[T]`
- `fn drop_while(self, f: fn(T) -> Bool) -> List[T]`
- `fn group_by[K](self, f: fn(T) -> K) -> Dict[K, List[T]]`
- `fn partition(self, f: fn(T) -> Bool) -> (List[T], List[T])`
- `fn tally(self) -> Dict[T, Int]`

  How many times each element occurs, in order of first occurrence.

- `fn tally_by[K](self, f: fn(T) -> K) -> Dict[K, Int]`
- `fn sort_by[K: Comparable](self, f: fn(T) -> K) -> List[T]`
- `fn min_by[K: Comparable](self, f: fn(T) -> K) -> T?`
- `fn max_by[K: Comparable](self, f: fn(T) -> K) -> T?`
- `fn sum_by(self, f: fn(T) -> Int) -> Int`
- `fn sum_by_float(self, f: fn(T) -> Float) -> Float`

  As `sum_by`, for a `Float` block.

- `fn take_last(self, n: Int) -> List[T]`

  The last `n` elements (all of them if there are fewer).

- `fn join(self, sep: String = "") -> String`

  The elements' `to_s` joined with `sep` between them.

- `fn zip[U](self, other: List[U]) -> List[(T, U)]`
- `fn uniq(self) -> List[T]`
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
- `fn max(self) -> T?`
- `fn sort(self) -> List[T]`
- `fn minmax(self) -> (T?, T?)`

## Enumerable[Int]

- `fn sum(self) -> Int`
- `fn product(self) -> Int`

## Enumerable[Float]

- `fn sum(self) -> Float`
- `fn product(self) -> Float`

## Global functions

- `fn puts(value: Dyn = "")`
- `fn print(value: Dyn)`
- `fn eputs(value: Dyn = "")`
- `fn panic(message: String) -> Never`
- `fn assert(condition: Bool, message: String = "assertion failed")`
- `fn assert_eq(actual: Dyn, expected: Dyn, message: String = "")`
- `fn inspect(value: Dyn) -> String`
- `fn compare_values(a: Dyn, b: Dyn) -> Int`
- `fn read_line() -> String?`
- `fn args() -> List[String]`
- `fn exit(code: Int = 0) -> Never`
- `fn sleep(seconds: Float)`
- `fn gc()`

## enum Result[T, E]

- variant `Ok(T)`
- variant `Err(E)`
- `fn is_ok(self) -> Bool`
- `fn is_err(self) -> Bool`
- `fn ok(self) -> T?`
- `fn err(self) -> E?`
- `fn unwrap(self) -> T`
- `fn unwrap_err(self) -> E`
- `fn expect(self, message: String) -> T`
- `fn unwrap_or(self, default: T) -> T`
- `fn unwrap_or_else(self, f: fn(E) -> T) -> T`
- `fn map[U](self, f: fn(T) -> U) -> Result[U, E]`
- `fn map_err[F](self, f: fn(E) -> F) -> Result[T, F]`
- `fn and_then[U](self, f: fn(T) -> Result[U, E]) -> Result[U, E]`

## T?

Optionals: a nil-able value as a `Result`, so `?` can propagate a missing value from a function
that returns a `Result`: `port = env["PORT"].ok_or("PORT is not set")?`.

- `fn ok_or[E](self, error: E) -> Result[T, E]`
- `fn ok_or_else[E](self, f: fn() -> E) -> Result[T, E]`

## Int

- `fn to_s(self, base: Int = 10) -> String`

  Digits in `base` (2 to 36, lowercase letters above 9), with a leading `-` if negative.

- `fn hash(self) -> Int`
- `fn <=>(self, other: Int) -> Int`
- `fn to_f(self) -> Float`
- `fn to_i(self) -> Int`
- `fn abs(self) -> Int`
- `fn sign(self) -> Int`
- `fn is_even(self) -> Bool`
- `fn is_odd(self) -> Bool`
- `fn is_zero(self) -> Bool`
- `fn succ(self) -> Int`
- `fn pred(self) -> Int`
- `fn min(self, other: Int) -> Int`
- `fn max(self, other: Int) -> Int`
- `fn times(self, f: fn(Int))`
- `fn upto(self, limit: Int, f: fn(Int))`
- `fn downto(self, limit: Int, f: fn(Int))`
- `fn gcd(self, other: Int) -> Int`
- `fn lcm(self, other: Int) -> Int`
- `fn pow(self, exp: Int) -> Int`
- `fn to_s_radix(self, radix: Int) -> String`
- `fn chr(self) -> String`
- `fn sqrt(self) -> Int`
- `fn digits(self) -> List[Int]`
- `fn bit_length(self) -> Int`

## Float

- `fn to_s(self) -> String`
- `fn hash(self) -> Int`
- `fn <=>(self, other: Float) -> Int`
- `fn to_f(self) -> Float`
- `fn to_i(self) -> Int`
- `fn floor(self) -> Int`
- `fn ceil(self) -> Int`
- `fn round(self) -> Int`
- `fn round_to(self, digits: Int) -> Float`
- `fn truncate(self) -> Int`
- `fn abs(self) -> Float`
- `fn sqrt(self) -> Float`
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
- `fn is_infinite(self) -> Bool`
- `fn is_finite(self) -> Bool`
- `fn min(self, other: Float) -> Float`
- `fn max(self, other: Float) -> Float`
- `fn format(self, decimals: Int) -> String`

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
- `fn [](self, index: Int) -> String`
- `fn length(self) -> Int`
- `fn byte_length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn upcase(self) -> String`
- `fn downcase(self) -> String`
- `fn capitalize(self) -> String`
- `fn swapcase(self) -> String`
- `fn reverse(self) -> String`
- `fn strip(self) -> String`
- `fn lstrip(self) -> String`
- `fn rstrip(self) -> String`
- `fn trim(self) -> String`
- `fn chomp(self) -> String`
- `fn split(self, sep: String | Regex = "", limit: Int = 0) -> List[String]`

  The pieces between occurrences of `sep` (a string or a `Regex`); with the default `""`, the
  words between runs of whitespace. With a positive `limit`, at most `limit` pieces: the last
  holds the rest of the text.

- `fn lines(self) -> List[String]`
- `fn chars(self) -> List[String]`
- `fn bytes(self) -> List[Int]`
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
- `fn rindex_of(self, sub: String) -> Int?`
- `fn replace(self, old: String, new: String) -> String`
- `fn replace_first(self, old: String, new: String) -> String`
- `fn slice(self, start: Int, len: Int) -> String`
- `fn substring(self, start: Int, end: Int) -> String`
- `fn count(self, sub: String) -> Int`
- `fn to_i(self) -> Int?`
- `fn to_f(self) -> Float?`
- `fn parse_int(self, radix: Int = 10) -> Int?`
- `fn ord(self) -> Int`
- `fn center(self, width: Int, pad: String = " ") -> String`
- `fn ljust(self, width: Int, pad: String = " ") -> String`
- `fn rjust(self, width: Int, pad: String = " ") -> String`
- `fn format(self, args: List[Dyn]) -> String`

  Fills a runtime template: `{}` takes the next argument's `to_s`, `{:spec}` formats it with a
  spec as in `"#{x:spec}"`, `{{` and `}}` are literal braces. Panics with a `FormatError` on a bad
  spec or when the placeholders and arguments do not match in number.

- `fn is_digit(self) -> Bool`
- `fn is_alpha(self) -> Bool`
- `fn is_alnum(self) -> Bool`
- `fn is_space(self) -> Bool`
- `fn is_upper(self) -> Bool`
- `fn is_lower(self) -> Bool`
- `fn delete_prefix(self, prefix: String) -> String`
- `fn delete_suffix(self, suffix: String) -> String`
- `fn squeeze(self) -> String`
- `fn tr(self, from: String, to: String) -> String`
- `fn each_char(self, f: (fn(String))? = nil) -> List[String]`

  Calls `f` with each character; returns the characters (so without a block, `chars`).

- `fn repeat(self, n: Int) -> String`
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

  `n` copies of `value`. Panics with a MemoryError if the list cannot fit in memory.

- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn push(mut self, item: T)`
- `fn append(mut self, item: T)`
- `fn pop(mut self) -> T?`
- `fn shift(mut self) -> T?`
- `fn unshift(mut self, item: T)`
- `fn insert(mut self, index: Int, item: T)`
- `fn remove_at(mut self, index: Int) -> T`
- `fn clear(mut self)`
- `fn concat(mut self, other: List[T])`
- `fn [](self, index: Int) -> T`
- `fn []=(mut self, index: Int, value: T)`
- `fn get(self, index: Int) -> T?`
- `fn slice(self, start: Int, len: Int) -> List[T]`
- `fn dup(self) -> List[T]`
- `fn reverse(self) -> List[T]`
- `fn reverse_in_place(mut self)`
- `fn swap(mut self, i: Int, j: Int)`
- `fn truncate(mut self, len: Int)`
- `fn +(self, other: List[T]) -> List[T]`
- `fn first(self) -> T?`
- `fn last(self) -> T?`
- `fn take_last(self, n: Int) -> List[T]`
- `fn each(self, f: fn(T))`
- `fn each_with_index(self, f: fn(T, Int))`
- `fn each_index(self, f: fn(Int))`
- `fn map[U](self, f: fn(T) -> U) -> List[U]`
- `fn map_with_index[U](self, f: fn(T, Int) -> U) -> List[U]`
- `fn select(self, f: fn(T) -> Bool) -> List[T]`
- `fn reject(self, f: fn(T) -> Bool) -> List[T]`
- `fn reduce[U](self, init: U, f: fn(U, T) -> U) -> U`
- `fn index_of(self, item: T) -> Int?`
- `fn contains(self, item: T) -> Bool`
- `fn delete(mut self, item: T) -> Bool`
- `fn delete_if(mut self, f: fn(T) -> Bool)`
- `fn fill(mut self, value: T)`
- `fn rotate(self, n: Int = 1) -> List[T]`
- `fn chunk(self, size: Int) -> List[List[T]]`
- `fn sort_by_keys[K: Comparable](self, keys: List[K]) -> List[T]`
- `fn sort_with(self, f: fn(T, T) -> Int) -> List[T]`

  A stable sort by the comparator `f`: negative, zero or positive as its first argument sorts
  before, with or after its second.

- `fn min_by[K: Comparable](self, f: fn(T) -> K) -> T?`

  As `Enumerable`'s, natively: the first item with the smallest (largest) key.

- `fn max_by[K: Comparable](self, f: fn(T) -> K) -> T?`
- `fn par_map[U](self, f: fn(T) -> U) -> List[U]`
- `fn par_each(self, f: fn(T))`
- `fn par_reduce(self, identity: T, f: fn(T, T) -> T) -> T`
- `fn par_select(self, f: fn(T) -> Bool) -> List[T]`

## List[T] (where T: Comparable)

Sorting by the natural order needs `Comparable` items (tuples and lists of comparable items are
compared lexicographically).

- `fn sorted(self) -> List[T]`
- `fn sort_in_place(mut self)`

## List[List[U]]

- `fn flatten(self) -> List[U]`

## Deque[T]

A double-ended queue: a growable ring buffer with O(1) amortized `push`/`pop` at the back,
`unshift`/`shift` at the front, and O(1) indexing (negative indices count from the end). Use it
instead of `List.shift`/`unshift`, which move every item. Ownership is as for `List`: a deque is
owned by one thread, `freeze` makes it (and its items) immutable and shareable, and `Mutex`
shares a mutable one. It is `Enumerable`; `==` compares items in order and `Json.stringify`
writes it as an array.

- `fn init(self)`
- `fn from(items: Enumerable[T]) -> Deque[T]`
- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn push(self, item: T)`

  Appends at the back.

- `fn pop(self) -> T?`

  Removes and returns the last item.

- `fn unshift(self, item: T)`

  Prepends at the front.

- `fn shift(self) -> T?`

  Removes and returns the first item.

- `fn first(self) -> T?`
- `fn last(self) -> T?`
- `fn [](self, index: Int) -> T`
- `fn []=(self, index: Int, value: T)`
- `fn get(self, index: Int) -> T?`
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
- `fn []=(mut self, key: K, value: V)`
- `fn get(self, key: K) -> V?`
- `fn fetch(self, key: K, default: V) -> V`
- `fn has_key(self, key: K) -> Bool`
- `fn contains_key(self, key: K) -> Bool`
- `fn delete(mut self, key: K) -> V?`
- `fn clear(mut self)`
- `fn keys(self) -> List[K]`
- `fn values(self) -> List[V]`
- `fn entries(self) -> List[(K, V)]`
- `fn dup(self) -> Dict[K, V]`
- `fn each(self, f: fn((K, V)))`
- `fn each_key(self, f: fn(K))`
- `fn each_value(self, f: fn(V))`
- `fn get_or_insert(mut self, key: K, default: V) -> V`
- `fn merge(self, other: Dict[K, V]) -> Dict[K, V]`
- `fn update(mut self, other: Dict[K, V])`
- `fn map_values[W](self, f: fn(V) -> W) -> Dict[K, W]`
- `fn select_entries(self, f: fn(K, V) -> Bool) -> Dict[K, V]`
- `fn invert(self) -> Dict[V, K]`

## Dict[K, V] (where K: Comparable)

- `fn sort_by_key(self) -> List[(K, V)]`

## Dict[K, V] (where V: Comparable)

- `fn sort_by_value(self) -> List[(K, V)]`

## Set[T]

- `fn new() -> Set[T]`
- `fn from(items: List[T]) -> Set[T]`
- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn is_empty(self) -> Bool`
- `fn add(mut self, item: T) -> Bool`
- `fn remove(mut self, item: T) -> Bool`
- `fn contains(self, item: T) -> Bool`
- `fn clear(mut self)`
- `fn to_list(self) -> List[T]`
- `fn dup(self) -> Set[T]`
- `fn each(self, f: fn(T))`
- `fn union(self, other: Set[T]) -> Set[T]`
- `fn intersection(self, other: Set[T]) -> Set[T]`
- `fn difference(self, other: Set[T]) -> Set[T]`
- `fn |(self, other: Set[T]) -> Set[T]`
- `fn &(self, other: Set[T]) -> Set[T]`
- `fn -(self, other: Set[T]) -> Set[T]`
- `fn is_subset(self, other: Set[T]) -> Bool`
- `fn is_superset(self, other: Set[T]) -> Bool`

## Range

- `fn first(self) -> Int`
- `fn last(self) -> Int`
- `fn is_exclusive(self) -> Bool`
- `fn each(self, f: fn(Int))`
- `fn length(self) -> Int`
- `fn size(self) -> Int`
- `fn contains(self, x: Int) -> Bool`
- `fn step(self, n: Int) -> List[Int]`
- `fn to_list(self) -> List[Int]`
- `fn sum(self) -> Int`

## StringBuilder

- `fn new() -> StringBuilder`
- `fn push(mut self, s: String)`
- `fn push_value(mut self, v: Dyn)`
- `fn push_char(mut self, code: Int)`
- `fn to_s(self) -> String`
- `fn length(self) -> Int`
- `fn clear(mut self)`

## Mutex[T]

- `fn new(move value: T) -> Mutex[T]`
- `fn lock[R](self, f: fn(T) -> R) -> R`
- `fn update(self, f: fn(T) -> T) -> T`
- `fn get(self) -> T`
- `fn set(self, move value: T)`

## Atomic[T]

- `fn new(value: T) -> Atomic[T]`
- `fn get(self) -> T`
- `fn set(self, value: T)`
- `fn swap(self, value: T) -> T`
- `fn compare_and_set(self, expected: T, value: T) -> Bool`
- `fn add(self, delta: Int) -> Int`
- `fn update(self, f: fn(T) -> T) -> T`

## Channel[T]

- `fn new(capacity: Int = 0) -> Channel[T]`
- `fn send(self, move item: T)`
- `fn recv(self) -> T?`
- `fn try_recv(self) -> T?`
- `fn close(self)`
- `fn is_closed(self) -> Bool`
- `fn length(self) -> Int`
- `fn each(self, f: fn(T))`

## Thread[T]

- `fn join(self) -> Result[T, String]`
- `fn is_done(self) -> Bool`

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
- `fn pow(x: Float, y: Float) -> Float`
- `fn sin(x: Float) -> Float`
- `fn cos(x: Float) -> Float`
- `fn tan(x: Float) -> Float`
- `fn atan2(y: Float, x: Float) -> Float`
- `fn exp(x: Float) -> Float`
- `fn log(x: Float) -> Float`
- `fn log10(x: Float) -> Float`
- `fn hypot(x: Float, y: Float) -> Float`
- `fn min_int(a: Int, b: Int) -> Int`
- `fn max_int(a: Int, b: Int) -> Int`

## File

- `fn read(path: String) -> Result[String, String]`
- `fn write(path: String, contents: String) -> Result[Nil, String]`
- `fn append(path: String, contents: String) -> Result[Nil, String]`
- `fn exists(path: String) -> Bool`
- `fn delete(path: String) -> Result[Nil, String]`
- `fn read_lines(path: String) -> Result[List[String], String]`
- `fn list_dir(path: String) -> Result[List[String], String]`
- `fn metadata(path: String) -> Result[Metadata, String]`
- `fn read_bytes(path: String) -> Result[Bytes, String]`

  The file's contents as they are, for binary files (`read` needs valid UTF-8).

- `fn write_bytes(path: String, data: Bytes) -> Result[Nil, String]`

## Bytes (frozen)

An immutable byte string. Like `String` it is frozen, so it is shared freely between threads
and fibers; build one from a list of byte values, from text (`s.to_bytes`), from hex or base64,
or by reading a file (`File.read_bytes`). To assemble bytes piece by piece, collect them in a
`List[Int]` and convert once with `Bytes.new`, or join pieces with `+`.

- field `handle: NativeHandle`
- `fn new(values: List[Int] = []) -> Bytes`

  Bytes with the given values, each in 0..255.

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
- `fn slice(self, start: Int, len: Int) -> Bytes`
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
- `fn basename(path: String) -> String`

  The last component: `Path.basename("a/b.txt") == "b.txt"`.

- `fn dirname(path: String) -> String`

  Everything before the last component, or "." if there is none.

- `fn extension(path: String) -> String?`

  The extension without its dot, or nil: `Path.extension("a/b.tar.gz") == "gz"`.

- `fn stem(path: String) -> String`

  The last component without its extension.

- `fn is_absolute(path: String) -> Bool`

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
- `fn from_micros(n: Int) -> Duration`
- `fn from_millis(n: Int) -> Duration`
- `fn from_secs(n: Int) -> Duration`
- `fn from_mins(n: Int) -> Duration`
- `fn from_hours(n: Int) -> Duration`
- `fn from_days(n: Int) -> Duration`
- `fn from_secs_f(secs: Float) -> Duration`

  Rounded to the nearest nanosecond.

- `fn as_nanos(self) -> Int`
- `fn as_micros(self) -> Int`

  The whole microseconds, milliseconds or seconds, truncated toward zero.

- `fn as_millis(self) -> Int`
- `fn as_secs(self) -> Int`
- `fn as_secs_f(self) -> Float`
- `fn is_zero(self) -> Bool`
- `fn is_negative(self) -> Bool`
- `fn abs(self) -> Duration`
- `fn neg(self) -> Duration`
- `fn +(self, other: Duration) -> Duration`
- `fn -(self, other: Duration) -> Duration`
- `fn *(self, k: Int) -> Duration`
- `fn /(self, k: Int) -> Duration`

  Truncates toward zero, to whole nanoseconds.

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
- `fn from_unix(secs: Int, nanosecond: Int = 0, offset: Int = 0) -> DateTime`

  The instant `secs` seconds (plus `nanosecond` nanoseconds) after the epoch, at `offset`.

- `fn from_unix_millis(ms: Int, offset: Int = 0) -> DateTime`
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

- `fn seed(n: Int)`
- `fn int(lo: Int, hi: Int) -> Int`
- `fn float() -> Float`
- `fn shuffle[T](items: List[T]) -> List[T]`
- `fn choice[T](items: List[T]) -> T?`

## Env

The program's environment variables. `set` and `remove` change what `get` and `all` see and
what processes started with `Process` inherit, from any thread. They do not change the
environment of the `catx` process itself (that is unsound while other threads may read it), so
native code that reads the environment directly does not see them.

- `fn get(name: String) -> String?`
- `fn set(name: String, value: String)`

  `name` must be non-empty and contain no `=`; neither may contain a NUL character.

- `fn remove(name: String)`
- `fn all() -> Dict[String, String]`

  Every variable, sorted by name.

- `fn cpu_count() -> Int`

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
- `fn stringify(value: Dyn) -> String`
- `fn pretty(value: Dyn) -> String`

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
- `fn post(url: String, body: String, headers: Dict[String, String] = [:]) -> Result[HttpResponse, String]`
