//! A recursive function without a result annotation is E0230, and its result is unknown from
//! then on: uses of it do not pin it to a type, so no follow-on error about inferring it appears.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

fn error_codes(src: &str) -> Vec<String> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| d.code.to_string()).collect()
}

#[test]
fn a_recursive_use_of_an_unannotated_result_reports_only_e0230() {
    let progs = [
        // Through a typed variable.
        "fn f(c: Bool) {\n  g: fn(Bool) -> Int = f\n  \"s\"\n}\nputs(f(true))\n",
        // A call whose result is used as an `Int`.
        "fn f(c: Bool) {\n  if c { return 1 }\n  x: Int = f(false)\n  \"s\"\n}\nputs(f(true))\n",
        // A method.
        "class A {\n  n: Int\n  fn m(self, c: Bool) {\n    if c { return 1 }\n    x: Int = self.m(false)\n    \"s\"\n  }\n}\nputs(A.new(1).m(true))\n",
        // A generic function.
        "fn gen[T](x: T) {\n  y: Int = gen(1)\n  \"s\"\n}\nputs(gen(2))\n",
        // From a closure in the body.
        "fn cb(c: Bool) {\n  h = fn() -> Int { cb(true) }\n  \"s\"\n}\nputs(cb(true))\n",
    ];
    for src in progs {
        let codes = error_codes(src);
        assert!(!codes.is_empty() && codes.iter().all(|c| c == "E0230"), "{src}\n{codes:?}");
    }
}

#[test]
fn branches_returning_different_types_join_into_a_union() {
    let src = "fn f(c: Bool) {\n  if c { return 1 }\n  \"s\"\n}\nx: Int | String = f(true)\nputs(x)\n";
    assert!(error_codes(src).is_empty(), "{:?}", error_codes(src));
}

/// E0231 makes the global's type unknown, so a later typed use of it, or an operation on it in
/// the initializer, does not report a second error.
#[test]
fn a_global_defined_in_terms_of_itself_reports_only_e0231() {
    let progs = [
        "const A = A + 1\nx: String = A\nputs(x)\n",
        "const A = B\nconst B = A\nx: String = A\ny: Int = B\nputs(x)\n",
        "const A = [B]\nconst B = A[0]\nx: String = B\nputs(x)\n",
        "const A = fn() -> Int { x: String = A()\n  1 }\nputs(A())\n",
        "class K {\n  const C = K.C.size\n}\nx: Bool = K.C\nputs(x)\n",
    ];
    for src in progs {
        let codes = error_codes(src);
        assert_eq!(codes, ["E0231"], "{src}");
    }
}
