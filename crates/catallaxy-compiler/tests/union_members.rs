//! Members of a union value: every member type must have the member, a field's type is the union
//! of the members' field types, and a method call is checked against every member's signature.

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

const CLASSES: &str = "class C {\n  n: Int\n  fn m(self, x: Int) -> Int { x }\n}\n\
                       class D {\n  n: String\n  fn m(self, x: String) -> String { x }\n}\n";

#[test]
fn field_type_is_the_union_of_the_members() {
    let errs = errors(&format!("{CLASSES}fn f(c: C | D) -> Int {{ c.n }}\n"));
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].1.contains("produces `Int | String`"), "{errs:?}");
    assert!(errors(&format!("{CLASSES}fn f(c: C | D) -> Int | String {{ c.n }}\n")).is_empty());
}

#[test]
fn arguments_must_suit_every_member() {
    let errs = errors(&format!("{CLASSES}fn f(c: C | D) {{ c.m(1) }}\n"));
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0300");
    assert!(errs[0].1.contains("`m` on `D` expects `String`, found `Int`"), "{errs:?}");
    assert!(errs[0].2.contains("the receiver is `C | D`"), "{errs:?}");
}

#[test]
fn a_missing_member_is_named() {
    let src = "class C { n: Int }\nclass D { k: Int }\nclass E { n: Int }\nfn f(c: C | D | E) -> Int { c.n }\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0311");
    assert!(errs[0].1.contains("no method or field `n` on `D`, so not on every member of `C | D | E`"), "{errs:?}");
}

#[test]
fn result_is_the_union_of_the_members() {
    let src =
        "class C {\n  fn m(self, x: Int) -> Int { x }\n}\nclass D {\n  fn m(self, x: Int) -> String { \"s\" }\n}\n\
               fn f(c: C | D) -> Int | String { c.m(1) }\nfn g(c: C | D) -> String { c.m(1) }\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0301");
    assert!(errs[0].1.contains("`g` should return `String`"), "{errs:?}");
}

#[test]
fn parameters_must_match_by_name_and_default() {
    let src = "class C {\n  fn m(self, a: Int, b: Int = 1) -> Int { a }\n}\nclass D {\n  fn m(self, a: Int, b: Int) -> Int { a }\n}\n\
               fn f(c: C | D) -> Int { c.m(1, 2) }\nfn g(c: C | D) -> Int { c.m(1) }\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0332");
    assert!(errs[0].1.contains("`(a, b = ...)` on `C`, `(a, b)` on `D`"), "{errs:?}");
}

#[test]
fn mutating_a_frozen_member_is_rejected() {
    let src = "class C {\n  n: Int\n  fn bump(self) { self.n += 1 }\n}\nclass D {\n  fn bump(self) { }\n}\n\
               fn f(c: frozen C | D) { c.bump() }\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0614");
}
