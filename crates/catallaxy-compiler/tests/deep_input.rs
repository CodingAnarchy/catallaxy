//! Deeply nested and very long expressions compile or get a diagnostic (E0901, E0900); they
//! never overflow the compiler's stack. The test thread's own stack is small, so this also
//! checks that compilation runs on a stack of its own.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Compilation, Options};
use catallaxy_compiler::fmt::format_source;
use catallaxy_compiler::parser::{MAX_DEPTH, MAX_NESTING};

/// Generated programs: (name, program for size `n`).
fn programs(n: usize) -> Vec<(&'static str, String)> {
    let rep = |s: &'static str| vec![s; n];
    let mut interp = "1".to_string();
    for _ in 0..n {
        interp = format!("\"#{{{interp}}}\"");
    }
    vec![
        ("sum chain", format!("x = {}\nputs(x)\n", rep("1").join(" + "))),
        ("string chain", format!("x = {}\nputs(x.length)\n", rep("\"ab\"").join(" + "))),
        ("comparison of chains", format!("x = {} == {}\nputs(x)\n", rep("1").join(" * "), rep("1").join(" * "))),
        ("and chain", format!("x = {}\nputs(x)\n", rep("true").join(" and "))),
        ("method chain", format!("x = \"a\"{}\nputs(x)\n", ".upcase()".repeat(n))),
        ("parentheses", format!("x = {}1{}\nputs(x)\n", "(".repeat(n), ")".repeat(n))),
        ("lists", format!("x = {}1{}\nputs(x.length)\n", "[".repeat(n), "]".repeat(n))),
        ("ifs", format!("x = 1\n{}puts(x)\n{}", "if x > 0 {\n".repeat(n), "}\n".repeat(n))),
        ("interpolations", format!("x = {interp}\nputs(x)\n")),
        ("negations", format!("x = {}1\nputs(x)\n", "-".repeat(n))),
        ("nots", format!("x = {}true\nputs(x)\n", "!".repeat(n))),
        ("types", format!("x: {}Int{} = []\nputs(x.length)\n", "List[".repeat(n), "]".repeat(n))),
        ("blocks", format!("f = {}1{}\nputs(f)\n", "{ ".repeat(n), " }".repeat(n))),
        ("patterns", format!("match 1 {{\n  {}1{} => puts(1)\n  _ => puts(2)\n}}\n", "(".repeat(n), ")".repeat(n))),
        ("in a function", format!("fn f() -> Int {{\n  {}\n}}\nputs(f())\n", rep("1").join(" + "))),
    ]
}

fn codes(c: &Compilation) -> Vec<&'static str> {
    c.diags.iter().filter(|d| d.is_error()).map(|d| d.code).collect()
}

#[test]
fn deep_and_long_inputs_never_overflow() {
    for n in [300, 1_500, 5_000, 12_000] {
        for (name, src) in programs(n) {
            for check_only in [true, false] {
                let c = compile_source(Path::new("deep.cx"), &src, &Options { check_only });
                let errs = codes(&c);
                // E0303: a statement dropped for E0901 leaves its variable undefined.
                let allowed = ["E0900", "E0901", "E0303", "E0372"];
                assert!(errs.iter().all(|e| allowed.contains(e)), "{name} n={n}: {errs:?}");
                // A program that compiles for `run` passes `check`, and the other way round.
                if !check_only {
                    assert_eq!(c.program.is_some(), errs.is_empty(), "{name} n={n}");
                }
            }
            // The formatter refuses what does not parse, and formats the rest.
            let _ = format_source(&src);
        }
    }
}

#[test]
fn long_chains_compile() {
    // The issue's cases: chains of hundreds to thousands of operands need constant registers.
    for n in [300, 5_000] {
        for (name, src) in programs(n) {
            if matches!(
                name,
                "sum chain" | "string chain" | "comparison of chains" | "and chain" | "method chain" | "in a function"
            ) {
                let c = compile_source(Path::new("long.cx"), &src, &Options::default());
                assert!(c.program.is_some(), "{name} n={n}: {:?}", codes(&c));
            }
        }
    }
}

#[test]
fn limits_are_reported_where_they_are_hit() {
    let deep =
        format!("puts(1)\nx = {}1{}\n", "(".repeat(MAX_NESTING as usize + 10), ")".repeat(MAX_NESTING as usize + 10));
    let c = compile_source(Path::new("deep.cx"), &deep, &Options { check_only: true });
    let d = c.diags.iter().find(|d| d.code == "E0901").expect("E0901");
    assert_eq!(c.sm.file(d.span.file).path, "deep.cx");
    assert_eq!(c.sm.file(d.span.file).line_col(d.span.start).0, 2);
    assert_eq!(c.diags.iter().filter(|d| d.code == "E0901").count(), 1, "reported once");

    let long = format!("puts(1)\nx = {}\n", vec!["1"; MAX_DEPTH as usize + 10].join(" + "));
    let c = compile_source(Path::new("long.cx"), &long, &Options { check_only: true });
    let d = c.diags.iter().find(|d| d.code == "E0901").expect("E0901");
    assert_eq!(c.sm.file(d.span.file).line_col(d.span.start).0, 2);

    // Too many registers at the top level: reported at the statement, by `check` as well.
    let wide = format!("puts(1)\nx = {}1{}\n", "[".repeat(300), "]".repeat(300));
    for check_only in [true, false] {
        let c = compile_source(Path::new("wide.cx"), &wide, &Options { check_only });
        let d = c.diags.iter().find(|d| d.code == "E0900").unwrap_or_else(|| panic!("E0900: {:?}", codes(&c)));
        assert_eq!(c.sm.file(d.span.file).path, "wide.cx");
        assert_eq!(c.sm.file(d.span.file).line_col(d.span.start), (2, 1));
    }
    // In a function, at the function.
    let wide = format!("puts(1)\nfn f() -> Int {{\n  x = {}1{}\n  0\n}}\n", "[".repeat(300), "]".repeat(300));
    let c = compile_source(Path::new("wide.cx"), &wide, &Options { check_only: true });
    let d = c.diags.iter().find(|d| d.code == "E0900").expect("E0900");
    assert!(d.message.contains("function `f`"), "{}", d.message);
    assert_eq!(c.sm.file(d.span.file).line_col(d.span.start).0, 2);
}
