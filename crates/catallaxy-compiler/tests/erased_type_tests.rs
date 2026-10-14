//! Type tests (`is`, `as?`, type patterns) cannot check erased type arguments at run time, so a
//! test whose type arguments the static type does not determine is E0374. The erased form
//! (`is Box`) keeps the type arguments the static type implies. Variant patterns bind payloads
//! with every instantiation the subject may have.

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

fn codes(src: &str) -> Vec<String> {
    errors(src).into_iter().map(|e| e.0).collect()
}

const BOX: &str = "class Box[T] { v: T }\nclass Foo { n: Int = 1 }\n";

#[test]
fn is_with_undetermined_type_arguments_is_rejected() {
    let src = format!(
        "{BOX}fn h(b: Box[Int] | Box[String]) -> Int {{\n  if b is Box[String] {{ return b.v.length }}\n  0\n}}\n"
    );
    let errs = errors(&src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0374");
    assert!(errs[0].1.contains("cannot test for `Box[String]` at run time: a `Box[Int]` value"), "{errs:?}");
    assert!(errs[0].2.contains("type arguments are erased"), "{errs:?}");
}

#[test]
fn as_cast_from_dyn_with_type_arguments_is_rejected() {
    let src = format!("{BOX}x: Dyn = Box.new(5)\nif let b = x as? Box[String] {{ puts(b.v.length) }}\n");
    assert_eq!(codes(&src), ["E0374"]);
}

#[test]
fn typed_pattern_with_undetermined_type_arguments_is_rejected() {
    let src = "fn m(r: Result[Int, String] | Result[String, Int]) -> String {\n  match r {\n    a: Result[String, Int] => \"si\"\n    _ => \"other\"\n  }\n}\n";
    assert_eq!(codes(src), ["E0374"]);
}

#[test]
fn function_types_and_type_parameters_are_not_tested() {
    let src = "f: Dyn = fn(a: String) -> Int { a.length }\nif f is fn(Int) -> Int { puts(f(3)) }\n";
    assert_eq!(codes(src), ["E0374"]);
    let src = "fn g[T](x: Dyn) -> Bool { x is T }\n";
    assert_eq!(codes(src), ["E0374"]);
}

#[test]
fn determined_and_checked_type_tests_are_allowed() {
    let src = format!(
        "{BOX}fn a(x: Box[Int] | Foo) -> Int {{\n  if x is Box[Int] {{ return x.v + 1 }}\n  x.n\n}}\n\
         fn f(x: Dyn) -> Int {{\n  if x is List[Int] {{ return x[0] + 1 }}\n  if x is (Int, String) {{ return 3 }}\n  \
         if let l = x as? List[Dyn] {{ return l.length }}\n  0\n}}\n"
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
}

#[test]
fn erased_form_keeps_the_static_type_arguments() {
    let src = format!(
        "{BOX}fn b(x: Box[Int] | Foo) -> Int {{\n  if x is Box {{ return x.v + 2 }}\n  x.n\n}}\n\
         fn c(x: Box[Int] | String) -> Int {{\n  if let y = x as? Box {{ return y.v }}\n  0\n}}\n\
         fn d(x: Box[Int] | Foo) -> Int {{\n  match x {{\n    Box => 1\n    Foo => 2\n  }}\n}}\n\
         fn e(x: Box[Int] | Box[String]) -> Int {{\n  match x {{\n    y: Box => 1\n  }}\n}}\n"
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
    // From `Dyn`, the erased form gives `Dyn` arguments.
    let src = format!("{BOX}x: Dyn = Box.new(5)\nif let b = x as? Box {{ n: Int = b.v }}\n");
    assert_eq!(codes(&src), ["E0300"]);
}

#[test]
fn variant_patterns_bind_every_instantiation() {
    let src = "fn t(n: Int) -> Int { n }\nfn v(r: Result[Int, String] | Result[String, Int]) -> Int {\n  match r {\n    Ok(x) => t(x)\n    Err(e) => 0\n  }\n}\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].1.contains("found `Int | String`"), "{errs:?}");
    // A `Dyn` subject gives `Dyn` payloads (not types inferred from their uses).
    let src = "fn t(n: Int) -> Int { n }\nd: Dyn = Ok(\"s\")\nmatch d {\n  Ok(x) => puts(t(x))\n  _ => puts(0)\n}\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].1.contains("found `Dyn`"), "{errs:?}");
}
