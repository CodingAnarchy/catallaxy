//! Interfaces with `Self` in an input position (a parameter, or inside a generic type) are bounds
//! only: as a value type, two values may be different implementors (E0375).

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

/// `(code, message)` of each error in `src`.
fn errors(src: &str) -> Vec<(String, String)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| (d.code.to_string(), d.message.clone())).collect()
}

fn codes(src: &str) -> Vec<String> {
    errors(src).into_iter().map(|e| e.0).collect()
}

const MERGE: &str = "interface Merge { fn merge(self, other: Self) -> Int }\n\
                     class A {\n  n: Int\n  fn merge(self, other: A) -> Int { self.n + other.n }\n}\n";

#[test]
fn self_parameter_interface_as_parameter_type_is_rejected() {
    let src = format!("{MERGE}fn go(x: Merge, y: Merge) -> Int {{ x.merge(y) }}\n");
    let errs = errors(&src);
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert!(errs.iter().all(|e| e.0 == "E0375"), "{errs:?}");
    assert!(errs[0]
        .1
        .contains("interface `Merge` cannot be used as a value type, because its method `merge` takes `Self`"));
}

#[test]
fn comparable_is_not_a_value_type() {
    assert_eq!(codes("fn bigger(a: Comparable, b: Comparable) -> Bool { a > b }\n"), ["E0375", "E0375"]);
    assert_eq!(codes("xs: List[Comparable] = [1, \"a\"]\n"), ["E0375"]);
    assert_eq!(codes("x: Int | Comparable = 1\n"), ["E0375"]);
    assert_eq!(codes("class K {\n  v: Comparable\n}\n"), ["E0375"]);
    assert_eq!(codes("fn f() -> Comparable { 1 }\n"), ["E0375"]);
    assert_eq!(codes("x: Dyn = 1\nif x is Comparable { puts(1) }\n"), ["E0375"]);
}

#[test]
fn self_inside_generic_types_counts() {
    let src = "interface Pile { fn all(self) -> List[Self] }\nfn f(p: Pile) -> Int { 1 }\n";
    assert_eq!(codes(src), ["E0375"]);
    let src = "interface Fold { fn fold(self, f: fn(Self) -> Int) -> Int }\nfn f(p: Fold) -> Int { 1 }\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}

#[test]
fn bounds_and_result_only_self_are_allowed() {
    let src = format!(
        "{MERGE}interface Copyable {{ fn copy(self) -> Self }}\n\
         fn go[T: Merge](x: T, y: T) -> Int {{ x.merge(y) }}\n\
         fn both[T: Merge & Copyable](x: T) -> Int {{ x.merge(x.copy()) }}\n\
         fn bigger[T: Comparable](a: T, b: T) -> Bool {{ a > b }}\n\
         fn dup(c: Copyable) -> Copyable {{ c.copy() }}\n\
         class Holder[T: Comparable] {{ v: T }}\n\
         puts(go(A.new(1), A.new(2)))\n"
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
}

#[test]
fn inline_interfaces_with_self_parameters_are_rejected() {
    let src = "fn go(x: { fn merge(self, o: Self) -> Int }) -> Int { 1 }\n";
    assert_eq!(codes(src), ["E0375"]);
    let src = "fn go[T: { fn merge(self, o: Self) -> Int }](x: T, y: T) -> Int { x.merge(y) }\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}
