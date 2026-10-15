//! Diagnostics that explain themselves: why a type does not satisfy an interface, and fixes
//! that work (no `freeze` suggested for a closure, which a frozen value could not call).

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

fn notes_of(src: &str) -> String {
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{src}\n{errs:?}");
    errs[0].2.clone()
}

#[test]
fn a_conformance_note_says_what_does_not_fit() {
    let shape = "interface Shape {\n  fn area(self) -> Float\n}\n";
    let n = notes_of(&format!(
        "{shape}class Sq {{\n  s: Int\n  fn area(self) -> Int {{ self.s }}\n}}\nx: Shape = Sq.new(2)\n"
    ));
    assert_eq!(n, "`area` returns `Int`, but `Shape` expects `Float`");
    let n = notes_of(&format!("{shape}class Sq {{\n  s: Int\n}}\nx: Shape = Sq.new(2)\n"));
    assert_eq!(n, "`Sq` has no method `area`, which `Shape` requires");
    let sink = "interface Sink[T] {\n  fn put(self, v: T)\n}\n";
    let n = notes_of(&format!("{sink}class S {{\n  fn put(self, v: Int) {{}}\n}}\nk: Sink[Int?] = S.new()\n"));
    assert_eq!(n, "`put` takes `Int` as `v`, but `Sink[Int?]` may pass `Int?`");
    let n = notes_of(&format!("{sink}class S {{\n  fn put(self, v: Int, w: Int) {{}}\n}}\nk: Sink[Int] = S.new()\n"));
    assert_eq!(n, "`put` also takes `w`, which `Sink[Int]` does not pass (give it a default value)");
    let n = notes_of(&format!("{sink}class S {{\n  fn put(self) {{}}\n}}\nk: Sink[Int] = S.new()\n"));
    assert_eq!(n, "`put` takes 0 arguments, but `Sink[Int]` passes 1");
    // An unmet bound says the same.
    let n = notes_of(&format!(
        "{shape}class Sq {{\n  s: Int\n  fn area(self) -> Int {{ self.s }}\n}}\nfn a[T: Shape](t: T) -> Float {{ t.area() }}\nputs(a(Sq.new(2)))\n"
    ));
    assert_eq!(n, "`area` returns `Int`, but `Shape` expects `Float`");
}

#[test]
fn freezing_is_not_suggested_for_a_closure() {
    let errs = errors("xs = [1]\nf = fn() -> Int { xs.size }\nt = spawn { f() }\n");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0604");
    assert!(!errs[0].2.contains("freeze"), "{errs:?}");
    assert!(errs[0].2.contains("spawn move"), "{errs:?}");
    // Other owned values can still be shared frozen.
    let errs = errors("xs = [1]\nt = spawn { xs.size }\n");
    assert!(errs[0].2.contains("freeze(xs)"), "{errs:?}");
    let errs = errors("ch: Channel[fn() -> Int] = Channel.new()\nf = fn() -> Int { 1 }\nch.send(f)\n");
    assert_eq!(errs[0].0, "E0612");
    assert!(!errs[0].1.contains("freeze"), "{errs:?}");
}
