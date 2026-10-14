//! Narrowing of locals across loop iterations: a narrowing from before a loop holds in the body
//! only if every assignment to the local in the loop keeps it.

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

/// `body` inside `fn g(x: Int?, i: Int)`, after `if x == nil { return }`.
fn check(body: &str) -> Vec<(String, String, String)> {
    errors(&format!(
        "fn maybe(i: Int) -> Int? {{ if i > 0 {{ i }} else {{ nil }} }}\nfn int(i: Int) -> Int {{ i }}\n\
         fn g(x: Int?, i: Int) {{\n  if x == nil {{ return }}\n{body}\n}}\n"
    ))
}

#[test]
fn assigning_a_possibly_nil_value_in_a_loop_drops_the_narrowing() {
    let bad = [
        // The issue's program: the second iteration sees `x = nil`.
        "while i < 2 {\n  y = x + 1\n  x = nil\n  i += 1\n}",
        "loop {\n  puts(x + 1)\n  x = maybe(i)\n}",
        "for j in [1, 2] {\n  puts(x + j)\n  x = maybe(j)\n}",
        // The condition runs again after the body.
        "while x > 0 {\n  x = maybe(1)\n}",
        // An assignment in a closure may run at any point.
        "while i < 2 {\n  puts(x + 1)\n  f = { || x = nil }\n  f()\n}",
        // After the loop `x` may be nil too.
        "for j in [1, 2] { x = nil }\nputs(x + 1)",
    ];
    for src in bad {
        let errs = check(src);
        assert_eq!(errs.len(), 1, "{src}\n{errs:?}");
        assert_eq!(errs[0].0, "E0310", "{src}\n{errs:?}");
    }
    let errs = check("while i < 2 {\n  y = x + 1\n  x = nil\n}");
    assert!(errs[0].2.contains("the loop assigns it a value that may be nil"), "{errs:?}");
}

#[test]
fn loops_that_keep_the_narrowing_check() {
    let ok = [
        "while x > 0 { x -= 1 }",
        "while x > 0 { x = x - 1 }",
        "while x < 100 { x = int(x) * 2 }",
        "loop {\n  x += 1\n  if x > 5 { break }\n}",
        "for j in [1, 2] { x = x * j + 1 }",
        "while x > 0 {\n  if x % 2 == 0 { x -= 1 } else { x -= 3 }\n}",
        "while i < 3 {\n  x = 7\n  i += 1\n}",
        // Nested loops.
        "while i < 3 {\n  while x > 0 { x -= 1 }\n  x += i\n}",
        // The narrowing holds after the loop too.
        "while x > 0 { x -= 1 }\nputs(x + 1)",
        // A narrowing that the loop drops can still be checked inside it.
        "while i < 2 {\n  if x != nil { puts(x + 1) }\n  x = maybe(i)\n}",
        // A use that does not need the narrowing.
        "while i < 2 {\n  puts(x)\n  x = maybe(i)\n}",
    ];
    for src in ok {
        assert!(check(src).is_empty(), "{src}\n{:?}", check(src));
    }
}

#[test]
fn an_assignment_that_breaks_a_kept_narrowing_is_an_error() {
    // `+` on `V` returns `V?`, which only the checked assignment reveals.
    let src = "class V {\n  n: Int\n  fn +(self, o: V) -> V? { nil }\n}\n\
               fn g(u: V?) {\n  if u == nil { return }\n  i = 0\n  while i < 2 {\n    u += V.new(i)\n    i += 1\n  }\n}\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0324");
    assert!(errs[0].1.contains("the enclosing loop treats `u` as `V`"), "{errs:?}");
}
