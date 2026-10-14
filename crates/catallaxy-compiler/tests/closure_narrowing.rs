//! Narrowing of locals that closures assign: a closure may run during any call, so a nil test on
//! such a local holds only until the next call. Inside a closure, a captured local that is
//! assigned elsewhere keeps no narrowing from outside, and its own narrowing ends at a call.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

/// `(code, message, notes)` of each error in `src`.
fn errors(src: &str) -> Vec<(String, String, String)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| {
            let notes: Vec<&str> = d.notes.iter().map(|(n, _)| n.as_str()).collect();
            (d.code.to_string(), d.message.clone(), notes.join("\n"))
        })
        .collect()
}

/// `body` inside `fn g(c: Bool)`, after `x: Int? = 1`, with a few helpers around.
fn check(body: &str) -> Vec<(String, String, String)> {
    errors(&format!(
        "fn other() {{}}\nfn run(f: fn()) {{ f() }}\n\
         class Holder {{\n  cb: fn() = fn() {{}}\n  fn fire(self) {{ (self.cb)() }}\n}}\n\
         class Stash {{\n  cb: fn(Int)? = nil\n  fn each(self, f: fn(Int)) {{ self.cb = f }}\n  \
         fn fire(self) {{\n    if let f = self.cb {{ f(1) }}\n  }}\n}}\n\
         fn g(c: Bool) {{\n  x: Int? = 1\n{body}\n}}\n"
    ))
}

#[test]
fn a_call_ends_the_narrowing_of_a_local_a_closure_assigns() {
    let bad = [
        // The issue's program.
        "f = fn() { x = nil }\nif x != nil {\n  f()\n  puts(x + 1)\n}",
        // Any call may run the closure, not just a call of it.
        "f = fn() { x = nil }\nif x != nil {\n  run(f)\n  puts(x + 1)\n}",
        "f = fn() { x = nil }\nif x != nil {\n  other()\n  puts(x + 1)\n}",
        // A block passed to a method.
        "fs: List[fn()] = []\nfs.push { x = nil }\nif x != nil {\n  fs[0]()\n  puts(x + 1)\n}",
        // A closure stored in a field.
        "h = Holder.new()\nh.cb = fn() { x = nil }\nif x != nil {\n  h.fire()\n  puts(x + 1)\n}",
        // A guard clause, and an assignment, narrow until the next call too.
        "f = { || x = nil }\nif x == nil { return }\nf()\nputs(x + 1)",
        "f = { || x = nil }\nx = 2\nf()\nputs(x + 1)",
        // A call in the other branch of a guard clause.
        "f = { || x = nil }\nif x == nil { return } else { f() }\nputs(x + 1)",
        // A call on the right of `&&`.
        "f = { || x = nil }\nif x != nil && run(f) == nil { puts(x + 1) }",
        // A closure created later in a loop runs on the next iteration.
        "f: fn()? = nil\ni = 0\nwhile i < 2 {\n  if x != nil {\n    if let h = f { h() }\n    puts(x + 1)\n  }\n  f = fn() { x = nil }\n  i += 1\n}",
        // A loop that keeps the narrowing over its own assignments cannot keep it over a call.
        "f = { || x = nil }\nif x == nil { return }\ni = 0\nwhile i < 2 {\n  f()\n  puts(x + 1)\n  x = 3\n  i += 1\n}",
        // The body of a `for` over a user `Enumerable` is a block its `each` may keep.
        "s = Stash.new()\nfor v in s { x = nil }\nif x != nil {\n  s.fire()\n  puts(x + 1)\n}",
    ];
    for src in bad {
        let errs = check(src);
        assert_eq!(errs.len(), 1, "{src}\n{errs:?}");
        assert_eq!(errs[0].0, "E0310", "{src}\n{errs:?}");
    }
    let errs = check("f = fn() { x = nil }\nif x != nil {\n  f()\n  puts(x + 1)\n}");
    assert!(errs[0].2.contains("a closure assigns `x`"), "{errs:?}");
}

#[test]
fn a_closure_cannot_rely_on_narrowing_a_local_assigned_elsewhere() {
    let bad = [
        // The narrowing from outside does not reach a closure that runs after `x = nil`.
        "if x != nil {\n  f = fn() { puts(x + 1) }\n  x = nil\n  f()\n}",
        // Inside the closure, a call may let the function (on another fiber) assign it.
        "fib = Fiber.start {\n  if x != nil {\n    Fiber.yield()\n    puts(x + 1)\n  }\n}\nFiber.yield()\nx = nil\nfib.join()",
        // Another closure may assign it.
        "f = fn() {\n  if x != nil {\n    other()\n    puts(x + 1)\n  }\n}\nh = fn() { x = nil }",
    ];
    for src in bad {
        let errs = check(src);
        assert_eq!(errs.len(), 1, "{src}\n{errs:?}");
        assert_eq!(errs[0].0, "E0310", "{src}\n{errs:?}");
    }
    let errs = check("if x != nil {\n  f = fn() { puts(x + 1) }\n  x = nil\n  f()\n}");
    assert!(errs[0].2.contains("`x` is assigned elsewhere"), "{errs:?}");
}

#[test]
fn narrowings_that_no_call_can_break_still_hold() {
    let ok = [
        // No call between the test and the use.
        "f = fn() { x = nil }\nif x != nil { puts(x + 1) }",
        "f = fn() { x = nil }\nif x != nil {\n  y = x + 1\n  f()\n  puts(y)\n}",
        // Test again after the call, or bind the value.
        "f = fn() { x = nil }\nf()\nif x != nil { puts(x + 1) }",
        "f = fn() { x = nil }\nif let v = x {\n  f()\n  puts(v + 1)\n}",
        // A closure that only reads `x`.
        "f = fn() { puts(x) }\nif x != nil {\n  f()\n  puts(x + 1)\n}",
        // Assignments outside closures do not end narrowings at calls.
        "x = 2\nother()\nputs(x + 1)",
        // A closure's own test of a captured local that nothing reassigns.
        "f = fn() {\n  if x != nil {\n    other()\n    puts(x + 1)\n  }\n}",
        // A closure's own local, and the narrowing from outside of one never reassigned.
        "if x != nil {\n  f = fn() {\n    y: Int? = 2\n    if y != nil {\n      other()\n      puts(x + y)\n    }\n  }\n}",
        // `for` over a list runs its body in place.
        "for v in [1, 2] { x = v }\nif x != nil {\n  other()\n  puts(x + 1)\n}",
        // The guard's branch runs only when it leaves the function.
        "f = { || x = nil }\nif x != nil { puts(x + 1) } else { f() }",
    ];
    for src in ok {
        assert!(check(src).is_empty(), "{src}\n{:?}", check(src));
    }
}
