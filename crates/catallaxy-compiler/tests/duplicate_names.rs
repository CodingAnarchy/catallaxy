//! Names declared twice in one parameter list, type-parameter list or pattern (#171).

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

/// `(code, message, note count)` of each error in `src`.
fn errors(src: &str) -> Vec<(String, String, usize)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| (d.code.to_string(), d.message.clone(), d.notes.len())).collect()
}

fn one_dup(src: &str, msg: &str) {
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{src}: {errs:?}");
    assert_eq!(errs[0].0, "E0228", "{errs:?}");
    assert_eq!(errs[0].1, msg, "{errs:?}");
    assert_eq!(errs[0].2, 1, "a note points at the first declaration: {errs:?}");
}

#[test]
fn duplicate_parameters() {
    one_dup("fn f(a: Int, a: Int) -> Int { a - a }\n", "parameter `a` is declared more than once");
    one_dup("class C {\n  fn m(self, x: Int, x: String) { nil }\n}\n", "parameter `x` is declared more than once");
    one_dup("interface I { fn m(self, x: Int, x: Int) -> Int }\n", "parameter `x` is declared more than once");
}

#[test]
fn duplicate_block_parameters() {
    one_dup("f = { |b: Int, b: Int| b }\n", "block parameter `b` is declared more than once");
    one_dup("[(1, 2)].each { |a, a| puts(a) }\n", "block parameter `a` is declared more than once");
    // `_` may repeat.
    assert!(errors("f = { |_: Int, _: Int| 1 }\nputs(f(1, 2))\n").is_empty());
}

#[test]
fn duplicate_pattern_bindings() {
    one_dup("match (1, 2) {\n  (a, a) => puts(a)\n}\n", "variable `a` is declared more than once");
    one_dup("let (b, b) = (1, 2)\n", "variable `b` is declared more than once");
    one_dup("for (c, c) in [(1, 2)] { puts(c) }\n", "variable `c` is declared more than once");
    one_dup(
        "x: Dyn = (1, \"s\")\nmatch x {\n  (n: Int, n: String) => puts(n)\n  _ => nil\n}\n",
        "variable `n` is declared more than once",
    );
    assert!(errors("match (1, 2) {\n  (_, _) => puts(0)\n}\nlet (a, b) = (1, 2)\nputs(a + b)\n").is_empty());
}

#[test]
fn duplicate_type_parameters_and_fields() {
    one_dup("fn g[T, T](x: T) -> T { x }\n", "type parameter `T` is declared more than once");
    one_dup("class C[T, T] { v: Int }\n", "type parameter `T` is declared more than once");
    one_dup("enum E[T, T] { A }\n", "type parameter `T` is declared more than once");
    one_dup("interface I[T, T] { fn m(self) -> Int }\n", "type parameter `T` is declared more than once");
    one_dup("class K {\n  fn m[U, U](self) { nil }\n}\n", "type parameter `U` is declared more than once");
    one_dup("enum E { V(a: Int, a: Int) }\n", "field `a` is declared more than once");
}
