//! Checker fixes from the 0.4 survey: exhaustiveness witnesses, blocks assigned to variables,
//! mutable dict/set keys and `move` of literals.

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

const E: &str = "enum E {\n  A(Int)\n  B(E)\n  C\n}\n";

#[test]
fn recursive_enum_witnesses_are_shallow() {
    let src =
        format!("{E}fn f(e: E) -> Int {{\n  match e {{\n    .A(n) => n\n    .B(.A(n)) => n\n    .C => 0\n  }}\n}}\n");
    let errs = errors(&src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].1, "`match` does not cover .B(.B(_)), .B(.C)");
}

#[test]
fn missing_top_level_variants_are_listed() {
    let src = format!(
        "{E}fn f(e: E) -> Int {{\n  match e {{\n    .A(n) => n\n  }}\n}}\n\
         fn g(p: (Bool, Bool)) -> Int {{\n  match p {{\n    (true, _) => 1\n  }}\n}}\n"
    );
    let errs = errors(&src);
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert_eq!(errs[0].1, "`match` does not cover .B(_), .C");
    assert_eq!(errs[1].1, "`match` does not cover (false, _)");
}

#[test]
fn block_assigned_to_a_variable_is_diagnosed() {
    let errs = errors("count = 0\ninc = { count += 1 }\ninc()\n");
    assert_eq!(errs[0].0, "E0372", "{errs:?}");
    assert!(errs[0].2.contains("`{ || ... }` or `fn() { ... }`"), "{errs:?}");
    assert!(errs.iter().all(|e| !e.1.contains("_?")), "{errs:?}");
    // A block with a value is fine, and so is a closure.
    let ok = "x = {\n  a = 1\n  a + 1\n}\ninc = { || x + 1 }\ninc()\n";
    assert!(errors(ok).is_empty(), "{:?}", errors(ok));
}

#[test]
fn mutable_keys_are_rejected() {
    let src = "class C {\n  hits: Dict[List[Int], Int]\n}\nseen = Set.new()\nseen.add([1, 2])\n\
               fn f(g: List[Set[(Int, List[Int])]]) -> Int { g.length }\n";
    let errs = errors(src);
    let codes: Vec<&str> = errs.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(codes, ["E0373", "E0373", "E0373"], "{errs:?}");
    assert!(errs[1].1.contains("`seen` has type `Set[List[Int]]`"), "{errs:?}");
    let ok = "a = Set.new()\na.add(freeze([1, 2]))\nb = [(1, \"x\"): 2]\nc: Dict[String, List[Int]] = [:]\n";
    assert!(errors(ok).is_empty(), "{:?}", errors(ok));
}

#[test]
fn moving_a_fresh_literal_is_accepted() {
    let ok = "m = Mutex.new(move [\"hits\": 0])\nn = Mutex.new(move [1, 2])\n";
    assert!(errors(ok).is_empty(), "{:?}", errors(ok));
    let errs = errors("xs = [[1]]\nch: Channel[List[Int]] = Channel.new()\nch.send(move xs[0])\n");
    assert_eq!(errs[0].0, "E0606", "{errs:?}");
}
