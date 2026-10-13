# Catallaxy Standard Library

Generated from the prelude by `scripts/gen_stdlib_docs.py`; do not edit by hand.
Everything here is available in every module without an import.

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
- `fn reduce[U](self, init: U, f: fn(U, T) -> U) -> U`
- `fn count(self, f: fn(T) -> Bool) -> Int`
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
- `fn each_slice(self, n: Int, f: fn(List[T]))`
- `fn each_cons(self, n: Int, f: fn(List[T]))`
- `fn take(self, n: Int) -> List[T]`
- `fn take_while(self, f: fn(T) -> Bool) -> List[T]`
- `fn drop(self, n: Int) -> List[T]`
- `fn drop_while(self, f: fn(T) -> Bool) -> List[T]`
- `fn group_by[K](self, f: fn(T) -> K) -> Dict[K, List[T]]`
- `fn partition(self, f: fn(T) -> Bool) -> (List[T], List[T])`
- `fn tally_by[K](self, f: fn(T) -> K) -> Dict[K, Int]`
- `fn sort_by[K](self, f: fn(T) -> K) -> List[T]`
- `fn min_by[K](self, f: fn(T) -> K) -> T?`
- `fn max_by[K](self, f: fn(T) -> K) -> T?`
- `fn sum_by(self, f: fn(T) -> Int) -> Int`
- `fn zip[U](self, other: List[U]) -> List[(T, U)]`
- `fn uniq(self) -> List[T]`
- `fn contains(self, item: T) -> Bool`
- `fn include(self, item: T) -> Bool`
- `fn to_set(self) -> Set[T]`

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

## Enumerable[String]

- `fn join(self, sep: String = "") -> String`

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

## Int

- `fn to_s(self) -> String`
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
- `fn split(self, sep: String = "") -> List[String]`
- `fn lines(self) -> List[String]`
- `fn chars(self) -> List[String]`
- `fn bytes(self) -> List[Int]`
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
- `fn each_char(self, f: fn(String))`
- `fn repeat(self, n: Int) -> String`
- `fn matches(self, re: Regex) -> Bool`
- `fn scan(self, re: Regex) -> List[String]`
- `fn sub(self, re: Regex, replacement: String) -> String`
- `fn gsub(self, re: Regex, replacement: String) -> String`

## List[T]

- `fn new() -> List[T]`
- `fn with_capacity(n: Int) -> List[T]`
- `fn filled(n: Int, value: T) -> List[T]`
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
- `fn sort_by_keys[K](self, keys: List[K]) -> List[T]`
- `fn sorted(self) -> List[T]`
- `fn sort_in_place(mut self)`
- `fn sort_with(self, f: fn(T, T) -> Int) -> List[T]`
- `fn par_map[U](self, f: fn(T) -> U) -> List[U]`
- `fn par_each(self, f: fn(T))`
- `fn par_reduce(self, identity: T, f: fn(T, T) -> T) -> T`
- `fn par_select(self, f: fn(T) -> Bool) -> List[T]`

## List[List[U]]

- `fn flatten(self) -> List[U]`

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
- `fn sort_by_value(self) -> List[(K, V)]`
- `fn sort_by_key(self) -> List[(K, V)]`

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

- field `id: Int`
- `fn start(f: fn() -> T) -> Fiber[T]`
- `fn join(self) -> Result[T, String]`
- `fn is_done(self) -> Bool`
- `fn yield()`

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

## Metadata

- field `size: Int`
- field `is_dir: Bool`
- field `is_file: Bool`
- field `modified: Float`

## Dir

- `fn list(path: String) -> Result[List[String], String]`
- `fn create(path: String, recursive: Bool = false) -> Result[Nil, String]`
- `fn remove(path: String, recursive: Bool = false) -> Result[Nil, String]`
- `fn exists(path: String) -> Bool`
- `fn current() -> String`

## Path

- `fn join(base: String, part: String) -> String`
- `fn join_all(parts: List[String]) -> String`
- `fn basename(path: String) -> String`
- `fn dirname(path: String) -> String`
- `fn extension(path: String) -> String?`
- `fn stem(path: String) -> String`
- `fn is_absolute(path: String) -> Bool`

## Regex (frozen)

- field `pattern: String`
- field `handle: NativeHandle`
- `fn new(pattern: String) -> Result[Regex, String]`
- `fn escape(text: String) -> String`
- `fn matches(self, text: String) -> Bool`
- `fn find(self, text: String) -> Match?`
- `fn find_all(self, text: String) -> List[Match]`
- `fn replace(self, text: String, replacement: String) -> String`
- `fn replace_all(self, text: String, replacement: String) -> String`
- `fn split(self, text: String) -> List[String]`
- `fn capture_names(self) -> List[String]`
- `fn to_s(self) -> String`

## Match

- field `text: String`
- field `start: Int`
- field `end: Int`
- field `groups: List[String?]`
- field `named_groups: Dict[String, String]`
- `fn [](self, index: Int) -> String?`
- `fn captures(self) -> List[String?]`
- `fn named(self, name: String) -> String?`
- `fn to_s(self) -> String`

## Process

- `fn run(cmd: String, args: List[String] = [], env: Dict[String, String] = [:], dir: String? = nil, input: String? = nil) -> Result[Output, String]`
- `fn start(cmd: String, args: List[String] = [], env: Dict[String, String] = [:], dir: String? = nil) -> Result[Subprocess, String]`

## Output

- field `status: Int`
- field `stdout: String`
- field `stderr: String`
- `fn success(self) -> Bool`

## Subprocess

- field `pid: Int`
- field `handle: NativeHandle`
- `fn write(self, text: String) -> Result[Nil, String]`
- `fn close_stdin(self)`
- `fn read_line(self) -> String?`
- `fn read_all(self) -> String`
- `fn wait(self) -> Result[Int, String]`
- `fn kill(self) -> Result[Nil, String]`

## Time

- `fn now() -> Float`
- `fn monotonic() -> Float`
- `fn millis() -> Int`

## Random

- `fn seed(n: Int)`
- `fn int(lo: Int, hi: Int) -> Int`
- `fn float() -> Float`
- `fn shuffle[T](items: List[T]) -> List[T]`
- `fn choice[T](items: List[T]) -> T?`

## Env

- `fn get(name: String) -> String?`
- `fn cpu_count() -> Int`

## Json

- `fn parse(text: String) -> Result[Dyn, String]`
- `fn stringify(value: Dyn) -> String`
- `fn pretty(value: Dyn) -> String`

## Net

- `fn connect(host: String, port: Int, timeout: Float? = nil) -> Result[Socket, String]`
- `fn listen(host: String, port: Int) -> Result[Listener, String]`

## Socket

- field `peer: String`
- field `handle: NativeHandle`
- `fn read(self, max: Int = 65536) -> Result[String, String]`
- `fn read_line(self) -> Result[String?, String]`
- `fn write(self, data: String) -> Result[Int, String]`
- `fn close(self)`
- `fn set_timeout(self, seconds: Float?)`

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

- `fn get(url: String, headers: Dict[String, String] = [:]) -> Result[HttpResponse, String]`
- `fn post(url: String, body: String, headers: Dict[String, String] = [:]) -> Result[HttpResponse, String]`
