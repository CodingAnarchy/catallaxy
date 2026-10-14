//! An override must accept every argument the overridden method accepts and return a result it
//! promises, since calls through the superclass type run it (E0227). An override that mutates
//! `self` makes the overridden method mutating.

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

fn one_error(src: &str, code: &str, text: &str) {
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, code, "{errs:?}");
    assert!(errs[0].1.contains(text) || errs[0].2.contains(text), "{errs:?}");
}

#[test]
fn narrower_parameter_type_is_rejected() {
    let src = "class A { fn feed(self, food: Int) -> Int { food } }\n\
               class C < A { fn feed(self, food: String) -> Int { food.length } }\n";
    one_error(src, "E0227", "parameter `food` takes `String`, which does not accept every `Int`");
}

#[test]
fn wider_result_type_is_rejected() {
    let src = "class A { fn get(self) -> Int { 1 } }\nclass B < A { fn get(self) -> Int? { nil } }\n";
    one_error(src, "E0227", "returns `Int?` where the overridden method promises `Int`");
    let src = "class A { fn get(self) -> String { \"a\" } }\nclass B < A { fn get(self) -> Int { 1 } }\n";
    one_error(src, "E0227", "promises `String`");
}

#[test]
fn inferred_results_are_compared() {
    let src = "class A { fn get(self) { 1 } }\nclass B < A { fn get(self) { \"b\" } }\n";
    one_error(src, "E0227", "returns `String` where the overridden method promises `Int`");
}

#[test]
fn dropped_default_is_rejected() {
    let src = "class A { fn f(self, x: Int = 5) -> Int { x } }\nclass B < A { fn f(self, x: Int) -> Int { x } }\n";
    one_error(src, "E0227", "parameter `x` has no default; the overridden one does");
}

#[test]
fn reordered_parameter_names_are_rejected() {
    let src = "class A { fn f(self, width: Int, height: Int) -> Int { width } }\n\
               class B < A { fn f(self, height: Int, width: Int) -> Int { width } }\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert!(errs.iter().all(|e| e.0 == "E0227"), "{errs:?}");
    assert!(errs[0].1.contains("its parameter 1 is `height`, which is parameter 2"), "{errs:?}");
}

#[test]
fn compatible_overrides_check() {
    let src = "class Shape {\n\
                 fn area(self) -> Float { 0.0 }\n\
                 fn name(self, prefix: String = \"\") -> String? { nil }\n\
                 fn scale(self, k: Int) -> Shape { self }\n\
                 fn pick[T](self, x: T) -> T { x }\n\
                 fn lt[T: Comparable](self, x: T, y: T) -> Bool { x < y }\n\
                 fn me(self) -> Self { self }\n\
               }\n\
               class Sq < Shape {\n\
                 fn area(self) -> Float { 4.0 }\n\
                 fn name(self, p: String = \"x\") -> String { \"sq\" }\n\
                 fn scale(self, k: Int | String) -> Sq { self }\n\
                 fn pick[U](self, x: U) -> U { x }\n\
                 fn lt[T](self, x: T, y: T) -> Bool { true }\n\
                 fn me(self) -> Self { self }\n\
               }\n\
               class G[T] { fn get(self, x: T) -> T { x } }\n\
               class H < G[Int] { fn get(self, x: Int) -> Int { x + 1 } }\n\
               class K[U] < G[U] { fn get(self, x: U) -> U { x } }\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}

#[test]
fn generic_superclass_arguments_are_substituted() {
    let src =
        "class G[T] { fn get(self, x: T) -> T { x } }\nclass H < G[Int] { fn get(self, x: String) -> Int { 1 } }\n";
    one_error(src, "E0227", "does not accept every `Int`");
}

#[test]
fn stronger_bound_is_rejected() {
    let src =
        "class A { fn f[T](self, x: T) -> Int { 1 } }\nclass B < A { fn f[T: Comparable](self, x: T) -> Int { 1 } }\n";
    one_error(src, "E0227", "requires type parameter `T` to be `Comparable`");
    let src = "class A { fn f[T](self, x: T) -> Int { 1 } }\nclass B < A { fn f(self, x: Int) -> Int { 1 } }\n";
    one_error(src, "E0227", "takes 0 type parameter(s) instead of 1");
}

#[test]
fn mutating_override_makes_the_overridden_method_mutating() {
    let src = "class A {\n  n: Int = 0\n  fn bump(self) -> Int { 1 }\n}\n\
               class B < A { fn bump(self) -> Int { self.n += 1; self.n } }\n\
               a: A = B.new()\nf = freeze(a)\nputs(f.bump())\n";
    one_error(src, "E0614", "the call may run this override, which mutates `self`");
    // Without a frozen receiver the program is fine.
    let src = "class A {\n  n: Int = 0\n  fn bump(self) -> Int { 1 }\n}\n\
               class B < A { fn bump(self) -> Int { self.n += 1; self.n } }\n\
               a: A = B.new()\nputs(a.bump())\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}
