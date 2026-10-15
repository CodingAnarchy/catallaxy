# Error codes

Every diagnostic from `catx check` (and `catx run`, `catx fix` and the language server) carries a code.
This file has one entry per code: what it means, a small program that triggers it, and how to fix it.
Codes starting with `E` are errors; the program does not run until they are fixed. Codes starting with
`W` are warnings; the program still runs, and `catx check` still exits with status 0. A comment
`# allow(CODE, ...)` on a warning's line, or on the line above it, silences those warnings there
(`s += x # allow(W0004)`); errors cannot be allowed.

The first digits group the codes:

- `E00xx`: reading the source (characters, literals, strings).
- `E01xx`: syntax.
- `E02xx`: declarations, modules and imports.
- `E03xx`: types and expressions.
- `E06xx`: ownership, freezing and threads.
- `E08xx`, `E09xx`: internal limits of the formatter and the code generator.
- `W0xxx`: warnings.

`catx check --json` gives the code of each diagnostic in its `code` field (see the reference, §10).
A test (`crates/catallaxy/tests/errors_doc.rs`) checks that every code the compiler can emit has an
entry here and that each example below still triggers its code.

## E0001

Unexpected character, or a file that cannot be read. The source contains a character that is not part
of any token, or the file named on the command line does not exist or is not readable.

```cx
x = 1 $ 2
```

Remove the character, or put it inside a string. For an unreadable file, check the path.

## E0002

Invalid integer literal. A `0x`, `0b` or `0o` prefix is followed by no digits, or by digits that the
base does not have.

```cx
flags = 0b102
```

Use only the digits of the base: `0`–`1` after `0b`, `0`–`7` after `0o`, `0`–`9` and `a`–`f` after `0x`.

## E0003

Invalid escape in a string. The string has a backslash escape that does not exist, or a `\u` escape
that is not `\u{...}` with a valid code point in hex.

```cx
puts("tab\q")
```

Use one of `\n \t \r \0 \\ \" \# \u{1F600}`, or a raw `'...'` string, where backslashes are literal.

## E0004

Unterminated string or interpolation. A string literal, or a `#{` inside one, is still open at the
end of the file.

```cx
name = "unterminated
puts(name)
```

Close the string with `"` (and the interpolation with `}`). A string cannot span lines; use `\n`.

## E0100

Syntax error. The parser found something it cannot read at this point, such as a missing expression
or a misplaced keyword. The message says what it expected.

```cx
x = * 2
```

Rewrite the code around the marked position; the message names what was expected.

## E0101

Missing token. A specific symbol was expected, most often a closing `)`, `]` or `}`.

```cx
fn area(w: Int, h: Int -> Int { w * h }
```

Insert the expected symbol. For a missing closing bracket, `catx check` suggests where.

## E0102

Expected end of statement. Two statements are on one line without a `;` between them, or an
expression is followed by stray tokens.

```cx
x = 1 2
```

Put each statement on its own line or separate them with `;`. If one expression was meant, add the
missing operator.

## E0103

Unmatched closing bracket. A `)`, `]` or `}` closes nothing.

```cx
x = 1 }
```

Remove the extra bracket, or add the opening one it was meant to close.

## E0104

Module variable without a type. A `var` at the top of a module must declare its type, because
every function in the module (and other threads) can see it.

```cx
var hits = 0
```

Add the type: `var hits: Int = 0`. For a value that never changes, use `const`.

## E0105

Invalid assignment target. The left side of `=` (or `+=` and the like) is not a variable, a field
(`a.b`) or an element (`a[i]`).

```cx
fn origin() -> Int { 0 }
origin() = 1
```

Assign to a variable, field or element instead.

## E0106

Chained comparison. Comparison operators do not chain: `0 < a < 2` is not a range test.

```cx
a = 1
puts(0 < a < 2)
```

Combine the comparisons with `and`: `0 < a and a < 2`.

## E0107

Unexpected tokens in an interpolation. A `#{...}` in a string holds more than one expression.

```cx
puts("#{1 2}")
```

Put a single expression inside `#{...}`; use separate interpolations for several values.

## E0108

Empty tuple. `()` is not a value.

```cx
nothing = ()
```

Use `nil` for "no value".

## E0109

Block after a call in a statement header. In the header of an `if`, `while`, `for` or `match`, a `{`
starts the body, so a call there cannot take a block argument.

```cx
xs = [1, 2]
for x in xs.map { |v| v * 2 } { puts(x) }
```

Wrap the call in parentheses, `for x in (xs.map { |v| v * 2 }) { ... }`, or assign it to a variable
first.

## E0110

Invalid format spec. The text after `:` in `#{value:spec}` is not a valid format spec (reference,
§4.7).

```cx
puts("#{42:q}")
```

Fix the spec, for example `#{42:08}` or `#{x:.2f}`. To print a literal `:`, put it outside the
braces.

## E0201

Duplicate declaration. Two items in one module (functions, classes, enums, interfaces, constants or
module variables) have the same name. There is no overloading.

```cx
fn area(r: Float) -> Float { r * r }
fn area(w: Float, h: Float) -> Float { w * h }
```

Rename one of them, or merge them into one function with default or named parameters.

## E0202

Private item. An import or a `module.item` reference names an item that is not `pub`.

```cx
import lib.secret.{hidden}
```

```cx lib/secret.cx
fn hidden() -> Int { 1 }
pub fn shown() -> Int { 2 }
```

Mark the item `pub` in its module, or use one that is public.

## E0203

No such item in a module. An import or a `module.item` reference names an item the module does not
define.

```cx
import lib.secret.{shown, nothing}
```

```cx lib/secret.cx
pub fn shown() -> Int { 2 }
```

Check the spelling, or add the item to the module.

## E0204

Module not found. An `import` names a module with no file. `import a.b` looks for `a/b.cx` next to the
importing file, then next to the entry file.

```cx
import nowhere.at.all
```

Check the path and file name; module paths use `.` between directories.

## E0205

Cyclic import. Modules import each other, directly or through others.

```cx
import lib.cycle_a
puts(cycle_a.a())
```

```cx lib/cycle_a.cx
import cycle_b
pub fn a() -> Int { cycle_b.b() }
```

```cx lib/cycle_b.cx
import cycle_a
pub fn b() -> Int { 1 }
```

Move what both modules need into a third module that both import.

## E0206

Name already imported. Two imports bind the same name to different modules or items.

```cx
import lib.secret.{shown}
import lib.secret as shown
```

```cx lib/secret.cx
pub fn shown() -> Int { 2 }
```

Rename one import with `as`: `import b.util as butil`.

## E0210

`Self` outside a class or interface. `Self` means "the type of the receiver" and only exists in class
and interface members.

```cx
fn same(a: Self, b: Self) -> Bool { true }
```

Name the type, or use a type parameter: `fn same[T](a: T, b: T) -> Bool`.

## E0211

`&` on a type that is not an interface. An intersection type `A & B` combines interfaces only.

```cx
fn f(x: Int & Comparable) -> Int { 1 }
```

Use interfaces on both sides of `&`, or use the concrete type alone.

## E0212

Type arguments on a type parameter. A type parameter such as `T` stands for one type and takes no
`[...]`.

```cx
fn first[T](xs: T[Int]) -> Int { 1 }
```

Write the type parameter alone (`T`), or name a generic type (`List[T]`).

## E0213

Unknown type. A type annotation names a type that is not defined or imported.

```cx
x: Strng = "a"
```

Fix the spelling (`catx check` suggests the closest name), define the type, or import it.

## E0214

Wrong number of type arguments. A generic type is used without its type arguments, or with the wrong
number of them.

```cx
x: List = []
```

Give every type argument: `List[Int]`, `Dict[String, Int]`.

## E0215

Method parameter without a type. Only plain non-`pub` functions may leave parameter types off (implicit
generics, reference §6.3); methods must annotate every parameter after `self`.

```cx
class Box {
  fn put(self, item) { }
}
```

Annotate the parameter: `fn put(self, item: Int)`.

## E0216

Untyped parameter on a `pub` function. A `pub` function is part of a module's interface, so its
parameters need types.

```cx
pub fn twice(x) -> Int { x * 2 }
```

Annotate the parameter (`pub fn twice(x: Int) -> Int`), or drop `pub`.

## E0217

`native fn` with a body. A native function is implemented in Rust; it is declared without a body.

```cx
native fn now() -> Int { 1 }
```

Remove the body, or remove `native` to define the function in Catallaxy.

## E0218

Function without a body. A function declaration with no `{ ... }` must be marked `native`.

```cx
fn later() -> Int
```

Add a body, or mark it `native fn` and register it from Rust.

## E0219

Class inherits from itself, directly or through its superclasses.

```cx
class Node < Node { v: Int }
```

Name a different superclass, or none.

## E0220

Subclass with a different modifier. A subclass must be `frozen` or `shared` exactly when its
superclass is.

```cx
frozen class Point { x: Int }
class Point3 < Point { z: Int }
```

Give the subclass the same modifier: `frozen class Point3 < Point`.

## E0221

Cannot inherit from this type. Only user-defined classes can be superclasses; built-in types such as
`String` and `List`, interfaces and enums cannot.

```cx
class Name < String { }
```

Hold the value in a field instead (`class Name { text: String }`), or implement an interface.

## E0222

Duplicate field. A class declares a field twice, or redeclares a field of its superclass.

```cx
class Point {
  x: Int
  y: Int
  x: Float
}
```

Remove or rename one of the fields.

## E0223

Duplicate method. A class (or an `extend` block) defines a method twice.

```cx
class Dog {
  fn speak(self) -> String { "woof" }
  fn speak(self) -> String { "arf" }
}
```

Remove or rename one of the methods. There is no overloading by parameter types.

## E0224

`init` without `self`. A constructor sets up a new instance, so it must take `self`.

```cx
class Acct {
  balance: Int = 0
  fn init(start: Int) { }
}
```

Write `fn init(self, start: Int) { self.balance = start }`.

## E0225

Interface method without `self`. Interfaces describe what a value can do, so every method takes
`self`.

```cx
interface Shape {
  fn area() -> Float
}
```

Add `self`: `fn area(self) -> Float`.

## E0226

Override with a different number of parameters. A method that overrides a superclass method must take
the same parameters.

```cx
class Shape {
  fn scale(self, k: Float) -> Float { k }
}
class Square < Shape {
  fn scale(self) -> Float { 1.0 }
}
```

Match the superclass signature, or give the new method a different name.

## E0227

Override with an incompatible signature. A call through the superclass type is checked against the
superclass method but runs the override, so the override must accept every argument the overridden
method accepts and return only what it promises:

- each parameter type must accept the overridden parameter's type (it may be wider);
- the result type must fit the overridden result type (it may be narrower);
- a parameter with a default in the overridden method needs a default in the override;
- a parameter name may change, but not to the name of another of the overridden method's parameters
  (named arguments are matched by the overridden method's names, then passed by position);
- the override declares as many type parameters, and bounds no stronger than the overridden ones.

```cx
class Animal {
  fn feed(self, food: Int) -> Int { food }
}
class Cat < Animal {
  fn feed(self, food: String) -> Int { food.length }
}
```

Use the overridden method's types (`food: Int`), or give the new method a different name. An override
that mutates `self` is allowed; it makes the overridden method mutating too, so calling it on a frozen
value is E0614.

## E0228

A name declared twice in one list: the parameters of a function, the parameters of a block, the type
parameters of a function, class, enum or interface, the named fields of an enum variant, or the
variables bound by one pattern. The second declaration would silently hide the first.

```cx
fn f(a: Int, a: Int) -> Int { a - a }
```

Rename one of them. Use `_` for a parameter or a pattern position you don't need; `_` may repeat.

## E0230

Recursive function without a return type. A function that calls itself must annotate its result
type, because the checker needs it while checking the body.

```cx
fn count(n: Int) { if n == 0 { 0 } else { 1 + count(n - 1) } }
count(3)
```

Annotate the result: `fn count(n: Int) -> Int`.

## E0231

Constant defined in terms of itself. A constant's or module variable's initializer depends on its own
value, directly or through others.

```cx
const A = B + 1
const B = A + 1
```

Break the cycle by giving one of them a value that does not depend on the other.

## E0232

Field not set by `init`. A class with an `init` must assign every field that has no default value.

```cx
class Acct {
  owner: String
  balance: Int
  fn init(self, owner: String) { self.owner = owner }
}
```

Assign the field in `init` (`self.balance = 0`), or give it a default (`balance: Int = 0`).

## E0300

Mismatched types. A value of one type is used where another is expected: an argument, an annotated
variable, a condition, an element. The message names both types.

```cx
fn f(s: String) -> Int { s.length }
f(42)
```

Pass a value of the expected type, convert it (`42.to_s`), or change the annotation.

## E0301

Wrong return type. The function's body produces a value of a type other than its declared result.

```cx
fn name() -> String { 42 }
```

Return a value of the declared type, or change the declared type.

## E0303

Unknown name. A name is not a local, parameter, function, constant or import in scope.

```cx
puts(lenght)
```

Fix the spelling (`catx check` suggests the closest name), or define the name before using it. Locals
of the main program are not visible inside functions; use `const` or `var` for module-wide values.

## E0304

Interpolation that never produces a value. The expression inside `#{...}` always panics, returns or
breaks, so the string can never be built.

```cx
puts("value: #{panic("no")}")
```

Move the `panic`, `return` or `break` out of the string.

## E0305

`self` outside a method. Only methods (functions declared inside a class, interface or `extend`
block with a `self` parameter) have `self`.

```cx
fn helper() -> Int { self.count }
```

Make the function a method of the class, or pass the value as a parameter.

## E0306

Variant used without its values. An enum variant that carries values was named without arguments.

```cx
r = Ok
```

Pass the values: `Ok(1)`, `Shape.Circle(2.0)`.

## E0307

Type used as a value. A class, enum or interface name appears where a value is expected.

```cx
class Point { x: Int }
p = Point
```

Construct a value (`Point.new(1)`) or call a static function of the type.

## E0308

Module used as a value. An imported module is not itself a value.

```cx
import lib.util
u = util
```

```cx lib/util.cx
pub fn hi() -> Int { 1 }
```

Use one of its items: `util.hi()`.

## E0309

Untyped function used as a value. A function with untyped parameters (an implicit-generic function)
can be called but not passed around, because it has no single function type.

```cx
fn twice(x) { x * 2 }
f = twice
```

Annotate its parameters (`fn twice(x: Int) -> Int`), or pass a closure: `{ |x: Int| twice(x) }`.

## E0310

Value may be nil. A member, operator or index is used on an optional value (`T?`) without checking
for `nil`. A call between a `!= nil` test on a field and its use also counts, because the call may
have changed the field; so does a call between a test on a variable that a closure assigns and its
use, because the call may have run the closure.

```cx
x: String? = nil
puts(x.length)
```

Check first (`if x != nil { ... }` or `if let s = x { ... }`), use `x?.length` or `x ?? ""`, or
assert with `x!.length` if `nil` is impossible.

## E0311

No such method or field. The type has no member with this name.

```cx
puts(5.upcase())
```

Fix the spelling (`catx check` suggests close names), convert the value first (`5.to_s.upcase()`), or
add the method to the class (or with `extend`).

## E0312

Unary operator on the wrong type. `-` applies to numbers and `~` to `Int`.

```cx
puts(-"five")
```

Apply the operator to a number, or convert the value first.

## E0313

Comparison that is always false. `==` (or `!=`) compares values of unrelated types, which can never be
equal.

```cx
puts(1 == "1")
```

Convert one side so both have the same type: `1.to_s == "1"`.

## E0314

`in` on something that is not a collection. The right side of `in` must have a `contains` method.

```cx
puts(3 in 5)
```

Use a list, set, dict, string or range on the right: `3 in [1, 3, 5]`.

## E0315

Wrong operand type for an operator. The left side defines the operator, but the right side has a type
it does not accept.

```cx
a = DateTime.of(2024, 1, 1)
b = DateTime.of(2024, 1, 2)
d = b - a
```

Pass the operand type the operator takes; the message names it. Here, `b.since(a)` is the duration
between two times.

## E0316

Operator not defined. No operator method (`+`, `-`, `<`, ...) exists for these operand types, or a
comparison needs `Comparable` elements that the type does not have.

```cx
puts([1, 2] - 2)
```

Use a method that does what was meant (`xs.reject { |x| x == 2 }`), or define the operator on your
class: `fn +(self, o: Money) -> Money`.

## E0317

`if let` on a value that is never nil. `if let` unwraps an optional; the value here is not one.

```cx
n = 5
if let v = n { puts(v) }
```

Use the value directly, or `if` with a condition.

## E0318

`return` without a value. The function declares a result type that does not include `nil`, but a
`return` gives no value.

```cx
fn pick(xs: List[Int]) -> Int {
  if xs.is_empty() { return }
  xs[0]
}
```

Return a value (`return 0`), or make the result optional (`-> Int?`).

## E0319

`break` or `continue` outside a loop.

```cx
break
```

Use them only inside `while`, `for`, `loop` or an iteration block; use `return` to leave a function.

## E0320

`?` with a different error type. `?` on a `Result[T, E]` returns the error early, so `E` must match
the error type of the function's own `Result`.

```cx
fn parse(s: String) -> Result[Int, Int] { Ok(1) }
fn run() -> Result[Int, String] {
  n = parse("1")?
  Ok(n)
}
```

Convert the error first: `parse("1").map_err { |e| e.to_s }?`, or make the error types agree.

## E0321

`?` in a function that cannot pass the error on. `?` on a `Result` needs the function to return a
`Result`; `?` on an optional needs it to return an optional.

```cx
fn parse(s: String) -> Result[Int, String] { Ok(1) }
fn main() -> Int { parse("1")? }
```

Change the function's result type, or handle the case in place: `match`, `unwrap_or`, `??`.

## E0322

`?` on a value that is neither a `Result` nor optional.

```cx
fn twice(n: Int) -> Int? {
  m = n?
  m * 2
}
```

Remove the `?`.

## E0323

Variable declared twice. An annotated assignment (`x: T = ...`) declares a new variable, and the
name already exists in this scope.

```cx
x: Int = 1
x: Int = 2
```

Drop the annotation to assign the existing variable (`x = 2`), or use another name.

## E0324

Assignment of the wrong type. A variable keeps the type of its first assignment (or its annotation).

```cx
x = 5
x = "five"
```

Assign a value of the same type, use a new variable, or declare the variable with a wider type
(`x: Int | String = 5`).

## E0325

Assignment to a constant.

```cx
const LIMIT = 10
fn bump() { LIMIT = 11 }
```

Use a module variable (`var limit: Int = 10`) for a value that changes.

## E0326

Variable assigned a value that never exists. The right side always panics, returns or breaks, so the
variable could never be set.

```cx
x = panic("stop")
```

Call the expression as a statement on its own, without the assignment.

## E0327

Assignment to a field that does not exist. Classes have a fixed set of fields.

```cx
class Point { x: Int }
p = Point.new(1)
p.z = 3
```

Fix the field name, or declare the field in the class.

## E0328

Wrong number of indices. Indexing (`a[i]`) and index assignment (`a[i] = v`) take exactly one index.

```cx
xs = [1, 2]
puts(xs[0, 1])
```

Use one index, or a range for a slice: `xs[0..2]`.

## E0329

Index assignment not supported. The type has no `[]=` method.

```cx
n = 5
n[0] = 1
```

Assign elements only on lists, dicts and classes that define `fn []=(mut self, i: K, v: V)`.

## E0330

Expected a type. A type argument in `Type[...]` written in an expression is not a type.

```cx
xs = List[1].new()
```

Write a type in the brackets: `List[Int].new()`.

## E0331

Class called like a function. Instances are constructed with `.new`.

```cx
class Point { x: Int }
p = Point(1)
```

Write `Point.new(1)`. `catx fix` applies this.

## E0332

Too many arguments. A call passes more arguments than the function takes (or `freeze` gets other than
one argument).

```cx
fn add(a: Int, b: Int) -> Int { a + b }
puts(add(1, 2, 3))
```

Remove the extra arguments, or add parameters to the function.

## E0333

Named argument to a closure. Closures and function values take positional arguments only.

```cx
add = { |a: Int, b: Int| a + b }
puts(add(a: 1, b: 2))
```

Pass the arguments by position: `add(1, 2)`.

## E0334

Call of something that is not a function.

```cx
limit = 10
puts(limit(3))
```

Call a function or closure. To multiply, write `limit * 3`.

## E0335

Argument given twice. A named argument repeats a parameter that already has a value, by name or by
position.

```cx
fn area(w: Int, h: Int) -> Int { w * h }
puts(area(w: 1, w: 2))
```

Give each parameter once: `area(w: 1, h: 2)`.

## E0336

Unknown named argument. The function has no parameter with this name, or the callee is a function
value, which takes positional arguments only.

```cx
fn add(a: Int, b: Int) -> Int { a + b }
add(a: 1, c: 2)
```

Use the parameter's name (`catx check` suggests close names), or pass the argument by position.

## E0337

Unexpected block. A block was passed to a function that takes no function parameter for it.

```cx
fn twice(n: Int) -> Int { n * 2 }
puts(twice(3) { |x| x })
```

Remove the block, or add a final function parameter (`f: fn(Int) -> Int`) to receive it.

## E0338

Missing argument. A parameter without a default value got no argument.

```cx
fn add(a: Int, b: Int) -> Int { a + b }
add(1)
```

Pass the argument, or give the parameter a default (`b: Int = 0`).

## E0339

Wrong number of values for a variant.

```cx
enum E { A(Int) }
e = E.A(1, 2)
```

Pass as many values as the variant declares.

## E0340

Recursive call to an untyped function. An implicit-generic function cannot call itself, because its
requirements are not known until its body has been checked.

```cx
fn count(n) -> Int { if n == 0 { 0 } else { 1 + count(n - 1) } }
count(3)
```

Annotate its parameters: `fn count(n: Int) -> Int`.

## E0341

Argument flows into a typed place it does not fit. In an implicit-generic function, a value derived
from an untyped parameter reaches an annotated variable, argument or result; the argument at this call
makes it a type that does not fit there. The note points at the place in the body.

```cx
fn f(x) -> Int { x }
f("s")
```

Pass an argument of the type the body needs, or annotate the parameter so the mismatch is reported
inside the function.

## E0342

Argument does not meet a requirement of an untyped parameter. The body of an implicit-generic function
uses its parameter in a way (a method, a field, an operator, an argument type) that the argument at
this call does not support. The note points at the use (reference, §6.3).

```cx
fn shout(x) { x.name.upcase }
shout(42)
```

Pass an argument that supports the use, or annotate the parameter. Some uses work only with an
annotated parameter: interface default methods, extensions of interfaces, named arguments out of
order, `Int` passed where a `Float` is expected, and comparisons on a type without `<=>`.

## E0343

Unknown variant. The enum has no variant with this name.

```cx
enum Color { Red, Green }
c = Color.Blue
```

Fix the spelling (`catx check` suggests close names), or add the variant to the enum.

## E0344

Tuple field out of range. `t.N` names an element past the end of the tuple.

```cx
pair = (1, "a")
puts(pair.2)
```

Use an index from `0` to the tuple's length minus one.

## E0345

Method called on a value whose type is not known yet. The checker infers types in order, and nothing
so far says what this value is.

```cx
xs = []
puts(xs[0].length)
```

Annotate the variable (`xs: List[String] = []`), or add an element before the call.

## E0346

Static function called on an instance. Functions without `self` belong to the type.

```cx
class Circle {
  r: Float
  fn unit() -> Circle { Circle.new(1.0) }
}
c = Circle.new(2.0)
d = c.unit()
```

Call it on the type: `Circle.unit()`.

## E0347

Instance method called on the type. Methods with `self` need a value.

```cx
class Circle {
  r: Float
  fn area(self) -> Float { 3.14 * self.r * self.r }
}
puts(Circle.area())
```

Call it on an instance: `Circle.new(1.0).area()`.

## E0348

No such static function. The type has no function of this name that takes no `self`.

```cx
class Circle { r: Float }
c = Circle.unit()
```

Fix the name (`catx check` suggests close names), use `Circle.new(...)`, or define
`fn unit() -> Circle` in the class.

## E0349

Variant without its enum. A leading-dot variant (`.Red`) needs an expected type that says which enum
it belongs to.

```cx
c = .Red
```

Name the enum (`Color.Red`), or annotate the target: `c: Color = .Red`.

## E0350

`super` with no superclass method. `super` works only in methods of a class that has a superclass.

```cx
class Base {
  fn hi(self) -> String { super.hi() }
}
```

Remove the `super` call, or give the class a superclass that has the method.

## E0351

Type indexed like a value. `List[Int]` names a type; it is not a value.

```cx
xs = List[Int]
```

Call a static function on it: `List[Int].new()`, or write a literal: `xs: List[Int] = []`.

## E0352

Tuple indexed with a non-literal. Tuple elements can have different types, so the index must be a
literal.

```cx
t = (1, 2, 3)
i = 1
puts(t[i])
```

Use a literal index (`t[1]` or `t.1`), or use a `List` if every element has the same type.

## E0353

Wrong index type. The index does not have the type the collection is indexed by: `Int` (or a range)
for lists and strings, the key type for dicts.

```cx
xs = [1, 2]
puts(xs["one"])
```

Pass an index of the right type, or convert it (`s.to_i`).

## E0354

Type cannot be indexed. The type has no `[]` method.

```cx
n = 42
puts(n[0])
```

Index lists, dicts, strings, tuples, or a class that defines `fn [](self, i: K) -> V`.

## E0355

Block takes the wrong number of parameters.

```cx
[1, 2].map { |a, b| a }
```

Give the block as many parameters as the method passes. A block over tuples may take one parameter
per element.

## E0356

Block parameter type cannot be inferred. A block that is not passed directly to a call has no
expected type for its parameters.

```cx
f = { |x| x + 1 }
```

Annotate the parameter: `f = { |x: Int| x + 1 }`.

## E0357

Block parameter annotated with the wrong type. The annotation differs from the type the method passes.

```cx
puts([1, 2].map { |s: String| s.length })
```

Fix the annotation or drop it; the method's element type is inferred.

## E0358

Block returns the wrong type. The method expects the block to produce a different type, such as `Bool`
for `select`.

```cx
puts([1, 2].select { |x| x + 1 })
```

Return the expected type: `select { |x| x > 1 }`.

## E0359

Not iterable. `for` needs a value with an `each` method.

```cx
for x in 42 { puts(x) }
```

Iterate a collection or a range: `for x in 0..42`.

## E0360

Non-exhaustive `match`. Some values of the subject match no arm. The message lists them.

```cx
enum Color { Red, Green, Blue }
fn name(c: Color) -> String {
  match c {
    .Red => "red"
    .Green => "green"
  }
}
```

Add arms for the missing cases, or a catch-all `_ => ...` arm. `catx check` offers both as fixes.

## E0361

Refutable pattern where it must always match. Destructuring assignments, `for` variables and `let`
take patterns that cannot fail, such as names and tuples.

```cx
enum Shape { Circle(Float), Square(Float) }
s = Shape.Circle(1.0)
let .Circle(r) = s
```

Use `match` or `if let`, which handle the case where the pattern does not match.

## E0362

Pattern can never match. The pattern's type has no values in common with the subject's type.

```cx
n = 5
match n {
  "five" => puts("5")
  _ => puts("?")
}
```

Use a pattern of the subject's type, or fix the subject.

## E0363

Binding in an `|` pattern. Alternatives joined with `|` cannot bind variables.

```cx
enum Msg { Ping(Int), Pong(Int) }
fn id(m: Msg) -> Int {
  match m {
    .Ping(n) | .Pong(n) => n
  }
}
```

Write one arm per alternative.

## E0364

Variant pattern with the wrong number of fields.

```cx
enum Shape { Circle(Float), Rect(Float, Float) }
fn area(s: Shape) -> Float {
  match s {
    .Circle(r) => r * r
    .Rect(w) => w
  }
}
```

Give one sub-pattern per field; use `_` for fields you do not need: `.Rect(w, _)`.

## E0365

Type pattern with arguments. A type pattern matches by type only.

```cx
fn f(x: Int | String) -> Int {
  match x {
    Int(n) => 1
    _ => 0
  }
}
```

Bind the value with `name: Type`: `n: Int => n`.

## E0366

Unknown pattern. A pattern names no variant of the subject's enum and no type.

```cx
enum Color { Red, Green }
fn f(c: Color) -> Int {
  match c {
    .Blue => 1
    _ => 0
  }
}
```

Fix the name (`catx check` suggests close names), or add the variant to the enum.

## E0367

`values()` on an enum with payloads. `Enum.values()` lists every variant, which works only when no
variant carries values.

```cx
enum Shape {
  Circle(r: Float)
  Empty
}

puts(Shape.values())
```

Build the list yourself, or move the payloads out of the enum.

## E0368

Type argument does not meet a bound. A type parameter of a generic function, class, enum or
interface is declared `T: Interface`, and the type it gets does not implement the interface. The
message lists the missing methods. For a function the type is the one at this call; for a class,
enum or interface it is every type written as an argument (`SortedList[Blob]` in an annotation, a
signature, a field or a superclass) and every type inferred for a construction (`SortedList.new()`
whose elements later turn out to be `Blob`s, `Heap.Node(Blob.new(1), .Leaf)`). A type parameter
passed on (`class Wrap[T] { s: SortedList[T] }`) needs the bound too.

```cx
fn biggest[T: Comparable](xs: List[T]) -> T? { xs.max() }
class Blob { v: Int }
biggest([Blob.new(1)])
```

```
class SortedList[T: Comparable] { items: List[T] = [] }
class Blob { v: Int }
s: SortedList[Blob] = SortedList.new()
```

Add the missing methods to the type (for `Comparable`, `fn <=>(self, o: Blob) -> Int`), or pass a
type that has them. For a type parameter passed on, bound it the same way: `class Wrap[T: Comparable]`.

## E0369

Use not supported through an untyped parameter. Assigning a field, `?`, and members of `freeze(x)`
need to know the parameter's type (reference, §6.3).

```cx
fn reset(x) { x.n = 0 }
```

Annotate the parameter: `fn reset(x: Counter)`.

## E0372

Block that runs now assigned to a variable. A bare `{ ... }` in value position runs immediately; its
value here is `nil`, so the variable would not hold code.

```cx
count = 0
inc = { count += 1 }
```

Write a closure, `inc = { || count += 1 }` or `fn() { count += 1 }`, to run the code later.

## E0373

Mutable dict or set key. A `Dict` or `Set` whose keys are owned lists, dicts or sets could lose
entries when a key changes after insertion.

```cx
seen = Set.new()
key = [1, 2]
seen.add(key)
key.push(3)
```

Use frozen keys (`Set[frozen List[Int]]`, filled with `freeze(k)`) or tuples.

## E0374

Type test that cannot be checked at run time. Type arguments of generic classes, enums and interfaces
are erased, so `is`, `as?` and type patterns check only the class (`Box`), not `Box[String]`; a
function type is checked only as "a function", and a type parameter not at all. A test is allowed
when the value's static type already determines what is not checked (`x: Box[Int] | Foo` tested for
`Box[Int]`), but not when a value could pass the test without having the tested type. Lists, sets,
dictionaries and tuples are checked element by element, so `is List[Int]` is allowed.

An interface test checks that the value's class satisfies the interface. A generic class may satisfy it
only for some type arguments (`class Box[T] { fn area(self) -> T }` is a `Shape` with
`fn area(self) -> Float` only as a `Box[Float]`), so a test for that interface is an error on a
`Box[Int]`, and on any value that may be of that class (a `Dyn`, an interface value, a type parameter):
the note names the class.

```cx
class Box[T] {
  v: T
}
fn h(b: Box[Int] | Box[String]) -> Int {
  if b is Box[String] { return b.v.length }
  0
}
```

Test for the class alone (`is Box`, `as? Box`, `b: Box =>`), which keeps the type arguments the static
type implies (`Box[Int] | Box[String]` here, `Box[Dyn]` for a `Dyn` value), and check the contents; or
give each case its own class or enum variant and test for that. For an interface test, test for the
classes themselves, or change the generic class's method so it satisfies the interface for every type
argument, or not at all.

## E0375

Interface with `Self` parameters used as a value type. In an interface, `Self` is the implementing
type, so `fn merge(self, other: Self)` means "merge with another value of my own type". Two values of
the interface type may be different implementors, and a call like `x.merge(y)` would pass one
implementor's value to another's method. Such an interface (the prelude's `Comparable` is one) can
only be a bound on a type parameter, not the type of a parameter, field, variable, result, list
element or union member. Interfaces that use `Self` only as a result (`fn copy(self) -> Self`) can be
value types.

```cx
interface Merge {
  fn merge(self, other: Self) -> Int
}
fn go(x: Merge, y: Merge) -> Int { x.merge(y) }
```

Introduce a type parameter bounded by the interface, so both values have the same type:
`fn go[T: Merge](x: T, y: T) -> Int`. For a collection, make the element type a type parameter
(`fn top[T: Comparable](xs: List[T])`).

## E0380

Format spec does not apply to the type. The spec is valid but not for this value: `.2f` on a
`String`, `x` on a `Float`, a precision on an `Int`, `+` on a `String`.

```cx
name = "Ada"
puts("#{name:.2f}")
```

Use a spec for the value's type, or convert the value first (`#{n.to_f:.2f}`).

## E0601

Module variable of a type that cannot be shared. Module variables are visible to every thread, so
their type must be frozen or `shared`.

```cx
var cache: List[Int] = []
```

Use a frozen type (`var cache: frozen List[Int] = freeze([])`), a `shared class`, or a `Mutex`.

## E0602

Mutable field in a frozen class. Every field of a `frozen class` must have a frozen type.

```cx
frozen class Bag { items: List[Int] }
```

Make the field type frozen: `items: frozen List[Int]`. `catx fix` applies this.

## E0603

Owned field in a shared class. Fields of a `shared class` must be frozen or shareable, so that other
threads can reach them safely.

```cx
shared class Registry { names: List[String] }
```

Use a frozen field type (`frozen List[String]`), or another `shared class`.

## E0604

Owned value captured by a parallel block. A `spawn` block (or `par_*` block) runs on another thread
and cannot share a value that this thread owns.

```cx
items = [1, 2, 3]
t = spawn { items.push(4) }
```

Move the value into the block (`spawn move { ... }`), freeze it first (`freeze(items)`), or use a
`shared class`.

## E0606

`move` of something other than a local. Only a local variable (or a fresh literal) can be moved,
because the checker can only forget a variable.

```cx
xs = [[1], [2]]
ch: Channel[List[Int]] = Channel.new()
ch.send(move xs[0])
```

Copy the value into a local first, or send a copy: `ch.send(xs[0].dup())`.

## E0607

`with lock` on a value that is not shared. Only instances of a `shared class` have a lock.

```cx
class Counter { n: Int = 0 }
c = Counter.new()
with lock(c) { c.n += 1 }
```

Declare the class `shared class Counter`.

## E0608

Use after move. The variable's value was moved (with `move`, `spawn move` or a send), here or
through another variable that referred to the same object.

```cx
items = [1, 2, 3]
t = spawn move { items.sum() }
puts(items)
```

Use the value before moving it, send a copy (`.dup()`), or assign the variable a new value.

## E0609

Assignment to a captured variable in a parallel block. A `spawn` block gets copies of the variables
it captures, so assigning one would have no effect outside.

```cx
n = 0
t = spawn { n = 1 }
```

Return the value from the block and read it with `t.join()`, or use a `shared class` or `Atomic`.

## E0610

Assignment into a frozen value. Frozen lists, dicts and objects are deeply immutable.

```cx
xs = freeze([1, 2, 3])
xs[0] = 10
```

Change a copy (`ys = xs.dup()`), or do not freeze the value.

## E0611

Field assignment on a frozen class. Instances of a `frozen class` (including built-in value types such
as `DateTime`) cannot change after construction.

```cx
frozen class P { x: Int }
p = P.new(1)
p.x = 2
```

Build a new instance (`p = P.new(2)`), or drop `frozen` from the class.

## E0612

Owned value sent without `move`. Sending an owned value to another thread transfers it.

```cx
ch: Channel[List[Int]] = Channel.new()
batch = [1, 2]
ch.send(batch)
```

Write `ch.send(move batch)`, or send `freeze(batch)` to share it read-only. `catx fix` adds `move`.

## E0613

Owned value sent from a field or element. Only a local variable can be moved, so a value reachable
from another object cannot be sent.

```cx
class Job { items: List[Int] }
j = Job.new([1])
ch: Channel[List[Int]] = Channel.new()
ch.send(j.items)
```

Send a copy (`j.items.dup()`), or restructure so the value is held in a local and moved.

## E0614

Mutating method on a frozen value. `freeze(x)` freezes the object itself, so every variable that
refers to it sees a frozen value afterwards.

```cx
xs = freeze([1, 2])
xs.push(3)
```

Mutate a copy (`ys = xs.dup()`), or make the change before freezing.

## E0615

Parallel method over owned elements. `par_map` and the other `par_*` methods run their block on other
threads, so the elements must be frozen or shared.

```cx
class Cell { v: Int }
cells = [Cell.new(1)]
cells.par_map { |c| c.v }
```

Freeze the list first (`freeze(cells).par_map { ... }`), or use frozen or shared element types.

## E0801

Internal formatter error. `catx fmt` produced output that would change the program, so it left the
file unchanged. This is a bug in the formatter; there is no example program.

Please report it with the file that triggered it. Until it is fixed, format that part of the file by
hand.

## E0900

Function too large. A function needs more than 250 registers (locals and temporaries live at once).
An example would be several hundred lines long, so there is none here.

Split the function into smaller functions.

## W0001

`!` on a value that is never nil. The non-nil assertion has no effect.

```cx
n = 5
puts(n!)
```

Remove the `!`. `catx fix` applies this.

## W0002

`??` on a value that is never nil. The right side is never used.

```cx
n = 5
puts(n ?? 0)
```

Remove `?? 0`.

## W0003

`?.` on a value that is never nil. Safe navigation has no effect.

```cx
name = "ada"
puts(name?.length)
```

Use `.` instead of `?.`.

## W0004

String built by appending in a loop. A `String` local declared outside a loop grows by `s += x` or
`s = s + x` inside a `for`, `while` or `loop` body, or inside the block of `each*`, `times`, `upto`,
`downto`, `map`, `flat_map`, `filter`, `filter_map`, `reject` or `select`. Each append copies the whole
string, so the loop takes time quadratic in the string's length. Loops over a literal range of at most
16 elements (`for i in 0..3`, `4.times`) are not reported.

```cx
fn join(xs: List[String]) -> String {
  s = ""
  for x in xs {
    s += x
  }
  s
}
```

Append to a `StringBuilder`, which grows in place, and convert once after the loop:
`sb = StringBuilder.new()` before it, `sb.push(x)` inside, `s = sb.to_s` after. `catx fix` makes this
rewrite when it is mechanical (reference, §10). To keep the code as it is, add `# allow(W0004)` on
the line.
