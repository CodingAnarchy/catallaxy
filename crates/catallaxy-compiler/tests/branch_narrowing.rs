//! Narrowing across the branches of `if`, `if let` and `match`: each branch starts from the
//! narrowings before it, and after it a narrowing holds only if it holds at the end of every
//! branch that falls through.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

/// `(code, message)` of each error in `src`.
fn errors(src: &str) -> Vec<(String, String)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| (d.code.to_string(), d.message.clone())).collect()
}

/// `body` inside `fn g(c: Bool, n: Int, o: Int?)`, after `x` is narrowed to `Int`.
fn check(body: &str) -> Vec<(String, String)> {
    errors(&format!(
        "fn other() {{}}\nfn g(c: Bool, n: Int, o: Int?) {{\n  x: Int? = 1\n  if x == nil {{ return }}\n{body}\n}}\n"
    ))
}

#[test]
fn an_assignment_in_one_branch_does_not_reach_the_others() {
    let ok = [
        // The issue's program.
        "y = if c {\n  x = nil\n  0\n} else {\n  x + 1\n}",
        "if c { x = nil } else { puts(x + 1) }",
        "if c { x = nil } else if n > 0 { puts(x + 1) } else { puts(x + 2) }",
        "if n > 0 { puts(x) } else if n < 0 { x = nil } else { puts(x + 1) }",
        "match n {\n  0 => { x = nil }\n  1 => puts(x + 1)\n  _ => puts(x + 2)\n}",
        "if let v = o { x = nil } else { puts(x + 1) }",
        // A branch that leaves does not reach the code after the `if`.
        "if c {\n  x = nil\n  return\n}\nputs(x + 1)",
        "if c { puts(x) } else {\n  x = nil\n  return\n}\nputs(x + 1)",
        "match n {\n  0 => {\n    x = nil\n    return\n  }\n  _ => {}\n}\nputs(x + 1)",
        // Assignments that keep it non-nil.
        "if c { x = 2 }\nputs(x + 1)",
        "if c { x += 2 } else { x = 3 }\nputs(x + 1)",
        // Every branch that falls through narrows it.
        "y = o\nif y == nil { y = 5 }\nputs(y + 1)",
        "y = o\nif c { y = 1 } else { y = 2 }\nputs(y + 1)",
        "y = o\nif y != nil { puts(y) } else { y = 0 }\nputs(y + 1)",
        "y = o\nmatch n {\n  0 => { y = 1 }\n  1 => return\n  _ => { y = 2 }\n}\nputs(y + 1)",
    ];
    for src in ok {
        assert!(check(src).is_empty(), "{src}\n{:?}", check(src));
    }
}

#[test]
fn after_the_branches_a_narrowing_holds_only_if_every_branch_keeps_it() {
    let bad = [
        "if c { x = nil }\nputs(x + 1)",
        "if c { puts(x) } else { x = nil }\nputs(x + 1)",
        "if c { puts(x) } else if n > 0 { x = nil } else { puts(x) }\nputs(x + 1)",
        "match n {\n  0 => { x = nil }\n  _ => {}\n}\nputs(x + 1)",
        "if let v = o { x = nil }\nputs(x + 1)",
        // The other branch of a guard clause reassigns it.
        "y = o\nif y == nil { return } else { y = nil }\nputs(y + 1)",
        "y = o\nif y == nil { y = 5 } else { y = nil }\nputs(y + 1)",
        // A guard that fails has run before the next arm.
        "match n {\n  0 if { x = nil\n false } => {}\n  _ => puts(x + 1)\n}",
        // So has the guard of the arm that is taken.
        "match n {\n  0 if { x = nil\n true } => puts(x + 1)\n  _ => {}\n}",
    ];
    for src in bad {
        let errs = check(src);
        assert_eq!(errs.len(), 1, "{src}\n{errs:?}");
        assert_eq!(errs[0].0, "E0310", "{src}\n{errs:?}");
    }
}

#[test]
fn field_paths_follow_the_same_rule() {
    let src = |body: &str| {
        errors(&format!(
            "class N {{\n  next: N?\n  v: Int = 0\n}}\nfn other() {{}}\n\
             fn g(c: Bool, n: N) {{\n  if n.next == nil {{ return }}\n{body}\n}}\n"
        ))
    };
    for ok in ["if c { other() } else { puts(n.next.v) }", "if c { n.next = nil } else { puts(n.next.v) }"] {
        assert!(src(ok).is_empty(), "{ok}\n{:?}", src(ok));
    }
    for bad in ["if c { other() }\nputs(n.next.v)", "if c { puts(1) } else { n.next = nil }\nputs(n.next.v)"] {
        let errs = src(bad);
        assert_eq!(errs.len(), 1, "{bad}\n{errs:?}");
        assert_eq!(errs[0].0, "E0310", "{bad}\n{errs:?}");
    }
}
