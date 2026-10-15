//! Narrowing of locals that closures assign, made precise: the pre-scan resolves assignments by
//! binding, a closure that is only called directly through its local runs only at those calls,
//! and a loop drops such a narrowing at entry only if an iteration may run the closure.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

fn error_codes(src: &str) -> Vec<String> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| d.code.to_string()).collect()
}

/// `body` inside `fn g(stash: Stash)`, after `x: Int? = 1`, with helpers around.
fn check(body: &str) -> Vec<String> {
    error_codes(&format!(
        "fn other() {{}}\nfn run(f: fn()) {{ f() }}\n\
         class Holder {{\n  cb: fn() = fn() {{}}\n  fn fire(self) {{ (self.cb)() }}\n}}\n\
         class Stash {{\n  cb: fn(Int)? = nil\n  fn each(self, f: fn(Int)) {{ self.cb = f }}\n  \
         fn fire(self) {{\n    if let f = self.cb {{ f(1) }}\n  }}\n}}\n\
         fn g(stash: Stash) {{\n  x: Int? = 1\n{body}\n}}\n"
    ))
}

#[test]
fn newly_accepted() {
    let ok = [
        // A loop without calls keeps a narrowing that it keeps over its own assignments.
        "f = fn() { x = nil }\nif x != nil {\n  i = 0\n  while i < 3 {\n    x = x + 1\n    i += 1\n  }\n  y: Int = x\n}\nf()",
        "f = fn() { x = nil }\nif x != nil {\n  for i in [1, 2] {\n    x = x + i\n  }\n  y: Int = x\n}\nf()",
        // A closure's own local of the same name is not the outer one.
        "f = fn() {\n  x: Int? = nil\n  x = 3\n}\nif x != nil {\n  f()\n  y: Int = x\n}",
        "f = fn(x: Int?) { x = nil }\nif x != nil {\n  f(nil)\n  y: Int = x\n}",
        "xs = [1].map { |x| x = 2\n  x }\nif x != nil {\n  other()\n  y: Int = x\n}",
        // A closure only called through its local does not run at other calls.
        "f = fn() { x = nil }\nif x != nil {\n  other()\n  n = [1, 2].size\n  y: Int = x\n}\nf()",
        "f: fn() = fn() { x = nil }\nif x != nil {\n  stash.fire()\n  y: Int = x\n}\nf()",
        // ... nor in a loop that does not call it.
        "f = fn() { x = nil }\nif x != nil {\n  i = 0\n  while i < 2 {\n    other()\n    y: Int = x\n    i += 1\n  }\n}\nf()",
    ];
    for body in ok {
        let codes = check(body);
        assert!(codes.is_empty(), "{body}\n{codes:?}");
    }
}

#[test]
fn still_rejected() {
    let bad = [
        // The closure's own call.
        "f = fn() { x = nil }\nif x != nil {\n  f()\n  y: Int = x\n}",
        // Its value escapes: passed, stored, aliased, captured or returned.
        "f = fn() { x = nil }\nif x != nil {\n  run(f)\n  y: Int = x\n}",
        "f = fn() { x = nil }\nh = Holder.new()\nh.cb = f\nif x != nil {\n  other()\n  y: Int = x\n}",
        "f = fn() { x = nil }\nk = f\nif x != nil {\n  other()\n  y: Int = x\n}",
        "f = fn() { x = nil }\nh = Holder.new(cb: fn() { f() })\nif x != nil {\n  h.fire()\n  y: Int = x\n}",
        // Called in a `for` body over a user type, which `each` may keep.
        "f = fn() { x = nil }\nfor v in stash { f() }\nif x != nil {\n  stash.fire()\n  y: Int = x\n}",
        "i = 0\nwhile i < 2 {\n  if x != nil {\n    stash.fire()\n    y: Int = x\n  }\n  for v in stash {\n    f = fn() { x = nil }\n    f()\n  }\n  i += 1\n}",
        // Called in a loop: on the next iteration the narrowing no longer holds.
        "f = fn() { x = nil }\nif x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    f()\n    i += 1\n  }\n}",
        "if x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    h = fn() { x = nil }\n    h()\n    i += 1\n  }\n}",
        // Called on the right of `&&`.
        "f = fn() { x = nil }\nif x != nil && f() == nil {\n  y: Int = x\n}",
        // A loop with a call cannot keep a narrowing the call may end.
        "f = { || x = nil }\nrun(f)\nif x != nil {\n  i = 0\n  while i < 3 {\n    x = x + 1\n    other()\n    i += 1\n  }\n  y: Int = x\n}",
        // A `for` over a user type runs `each` between iterations.
        "f = { || x = nil }\nrun(f)\nif x != nil {\n  for v in stash {\n    x = x + 1\n  }\n  y: Int = x\n}",
    ];
    for body in bad {
        let codes = check(body);
        assert!(!codes.is_empty() && codes.iter().all(|c| c == "E0300" || c == "E0310"), "{body}\n{codes:?}");
    }
}
