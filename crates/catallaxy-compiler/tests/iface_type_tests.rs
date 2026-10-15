//! Interface type tests check method signatures, with the rules of static conformance (#196).

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

fn errors(src: &str) -> Vec<(String, String)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| (d.code.to_string(), d.message.clone())).collect()
}

const SHAPES: &str = "interface Shape {\n  fn area(self) -> Float\n}\n\
                      class Circle {\n  r: Float\n  fn area(self) -> Float { self.r }\n}\n\
                      class Scaled {\n  fn area(self, k: Int) -> Float { 1.0 * k }\n}\n\
                      class Box[T] {\n  v: T\n  fn area(self) -> T { self.v }\n}\n";

#[test]
fn incompatible_class_never_passes() {
    // `Scaled` has an `area`, but not `Shape`'s: the arm for `Shape` cannot match it, so the
    // `Scaled` arm is still needed, and the narrowed `w` is a `Scaled`.
    let src = format!(
        "{SHAPES}fn f(u: Circle | Scaled) -> Float {{\n  match u {{\n    s: Shape => s.area()\n    w: Scaled => w.area(2)\n  }}\n}}\n"
    );
    assert!(errors(&src).is_empty(), "{:?}", errors(&src));
    let missing =
        format!("{SHAPES}fn f(u: Circle | Scaled) -> Float {{\n  match u {{\n    s: Shape => s.area()\n  }}\n}}\n");
    let errs = errors(&missing);
    assert!(errs.iter().any(|e| e.1.contains("does not cover")), "{errs:?}");
}

#[test]
fn class_that_satisfies_only_for_some_type_arguments() {
    // A `Box[Float]` is a `Shape`, and the test passes for it.
    let ok = format!("{SHAPES}b: Box[Float] | Circle = Box.new(2.0)\nputs(b is Shape)\n");
    assert!(errors(&ok).is_empty(), "{:?}", errors(&ok));
    // A `Box[Int]` would pass the run-time test without being a `Shape`.
    let bad = format!("{SHAPES}c: Box[Int] | Circle = Box.new(2)\nputs(c is Shape)\n");
    let errs = errors(&bad);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0374");
    assert!(errs[0].1.contains("a `Box[Int]` value could pass"), "{errs:?}");
    // So would one inside a `Dyn`, an interface value or a type parameter.
    for subject in ["d: Dyn = 1\nputs(d is Shape)\n", "fn g[T](x: T) -> Bool { x is Shape }\n"] {
        let errs = errors(&format!("{SHAPES}{subject}"));
        assert_eq!(errs.len(), 1, "{subject}: {errs:?}");
        assert_eq!(errs[0].0, "E0374", "{errs:?}");
    }
}

#[test]
fn dyn_tests_are_fine_when_every_class_decides() {
    let src = "interface Shape {\n  fn area(self) -> Float\n}\n\
               class Circle {\n  r: Float\n  fn area(self) -> Float { self.r }\n}\n\
               class Wrong {\n  fn area(self) -> String { \"x\" }\n}\n\
               d: Dyn = Wrong.new()\nputs(d is Shape)\nif let s = d as? Shape { puts(s.area()) }\n\
               match d {\n  s: Shape => puts(s.area())\n  _ => nil\n}\n\
               s: Shape = Circle.new(1.0)\nputs(s is Shape)\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}
