//! Narrowing of field paths and `??=`, and `T?.ok_or` for `?` in functions returning `Result`.

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

const NODE: &str = "class Node {\n  v: Int\n  next: Node?\n  fn cut(mut self) { self.next = nil }\n}\n\
                    frozen class Pair {\n  a: Int\n  b: Pair?\n}\nn = Node.new(1, Node.new(2, nil))\n";

fn check(body: &str) -> Vec<(String, String, String)> {
    errors(&format!("{NODE}{body}"))
}

#[test]
fn field_tests_narrow_field_paths() {
    let ok = [
        "if n.next != nil { puts(n.next.v) }",
        "if n.next != nil and n.next.v > 1 { puts(1) }",
        "if n.next == nil { puts(0) } else { puts(n.next.v) }",
        "fn f(n: Node) -> Int {\n  if n.next == nil { return 0 }\n  n.next.v\n}",
        "if n.next != nil and n.next.next != nil { puts(n.next.next.v) }",
        "n.next = Node.new(3, nil)\nputs(n.next.v)",
        "m = Node.new(1, nil)\nm.next ??= Node.new(2, nil)\nputs(m.next.v)",
        // Frozen classes cannot change: calls keep the narrowing.
        "p = Pair.new(1, Pair.new(2, nil))\nif p.b != nil {\n  puts(\"x\")\n  puts(p.b.a)\n}",
    ];
    for src in ok {
        assert!(check(src).is_empty(), "{src}\n{:?}", check(src));
    }
}

#[test]
fn field_narrowing_ends_when_the_field_may_change() {
    let bad = [
        "fn poke(n: Node) { n.next = nil }\nif n.next != nil {\n  poke(n)\n  puts(n.next.v)\n}",
        "if n.next != nil {\n  n.cut()\n  puts(n.next.v)\n}",
        "if n.next != nil {\n  m = n\n  m.next = nil\n  puts(n.next.v)\n}",
        "if n.next != nil {\n  n = Node.new(5, nil)\n  puts(n.next.v)\n}",
        "if n.next != nil {\n  f = { || n.next.v }\n}",
        "if n.next != nil {\n  i = 0\n  while i < 2 {\n    puts(n.next.v)\n    i += 1\n  }\n}",
        "if n.next != nil and n.cut() == nil { puts(n.next.v) }",
    ];
    for src in bad {
        let errs = check(src);
        assert_eq!(errs.len(), 1, "{src}\n{errs:?}");
        assert_eq!(errs[0].0, "E0310", "{src}\n{errs:?}");
    }
    let errs = check("puts(n.next.v)");
    assert!(errs[0].2.contains("`if let next = n.next { ... }`"), "{errs:?}");
}

#[test]
fn coalesce_assign_narrows() {
    let src = "x: Int? = nil\nx ??= 5\ny = x + 1\nfn f(d: Dict[String, Int]) -> Int {\n  c = d[\"a\"]\n  c ??= 0\n  c + 1\n}\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}

#[test]
fn ok_or_converts_optionals_for_question_mark() {
    let src =
        "fn port(env: Dict[String, String]) -> Result[Int, String] {\n  raw = env[\"PORT\"].ok_or(\"unset\")?\n  \
               n = raw.to_i().ok_or_else { \"bad #{raw}\" }?\n  Ok(n)\n}\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
    let errs = errors("fn f(d: Dict[String, Int]) -> Result[Int, String] {\n  v = d[\"k\"]?\n  Ok(v)\n}\n");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0321");
    assert!(errs[0].2.contains(".ok_or(error)?"), "{errs:?}");
    // Calling a method of the value on an optional still needs unwrapping.
    let errs = errors("x: String? = nil\ny = x.upcase\n");
    assert_eq!(errs[0].0, "E0310", "{errs:?}");
}
