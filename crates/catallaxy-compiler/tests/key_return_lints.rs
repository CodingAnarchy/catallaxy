//! W0005 (a bare-identifier dict key that names a variable in scope) and W0006 (`return` with a
//! value in a block whose result is discarded), both silenced by `# allow(...)`.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

fn codes(src: &str) -> Vec<String> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().map(|d| d.code.to_string()).collect()
}

#[test]
fn a_bare_key_naming_a_variable_is_w0005() {
    for src in [
        "fn f(src: String) -> Dict[String, Int] { [src: 0] }\nputs(f(\"a\"))\n",
        "src = \"a\"\nd = [src: 0, dst: 1]\nputs(d)\n",
        // A variable of an enclosing function, seen from a block.
        "fn f(src: String) -> List[Dict[String, Int]] { [1].map { |i| [src: i] } }\nputs(f(\"a\"))\n",
    ] {
        assert_eq!(codes(src), ["W0005"], "{src}");
    }
    for src in [
        // No such variable, a quoted key, a computed key.
        "d = [src: 0]\nputs(d)\n",
        "src = \"a\"\nd = [\"src\": 0]\nputs(d)\n",
        "src = \"a\"\nd = [(src): 0]\nputs(d)\n",
        // A variable declared later is not in scope.
        "d = [src: 0]\nsrc = 1\nputs(d)\nputs(src)\n",
        "src = \"a\"\n# allow(W0005)\nd = [src: 0]\nputs(d)\n",
        "src = \"a\"\nd = [src: 0] # allow(W0005)\nputs(d)\n",
    ] {
        assert!(codes(src).is_empty(), "{src}\n{:?}", codes(src));
    }
}

#[test]
fn a_value_returned_from_a_discarded_block_is_w0006() {
    let first = "fn first_big(xs: List[Int]) -> Int? {\n";
    for body in [
        "  xs.each { |x| if x > 1 { return x } }\n  nil\n",
        "  xs.each { |x|\n    for y in [x] { return y }\n  }\n  nil\n",
        "  xs.each(fn(x: Int) { return x })\n  nil\n",
    ] {
        let src = format!("{first}{body}}}\nputs(first_big([1, 2]))\n");
        assert_eq!(codes(&src), ["W0006"], "{src}");
    }
    for body in [
        // Without a value, or `nil`, it only ends the block's call: nothing is dropped.
        "  xs.each { |x| if x > 1 { return } }\n  nil\n",
        "  xs.each { |x| if x > 1 { return nil } }\n  nil\n",
        // A block whose result is used.
        "  ys = xs.map { |x| return x + 1 }\n  ys.first\n",
        // A `for` loop returns from the function.
        "  for x in xs { if x > 1 { return x } }\n  nil\n",
        // A closure inside the block has its own result.
        "  xs.each { |x| f = fn() -> Int { return x }\n    puts(f()) }\n  nil\n",
        "  # allow(W0006)\n  xs.each { |x| if x > 1 { return x } }\n  nil\n",
    ] {
        let src = format!("{first}{body}}}\nputs(first_big([1, 2]))\n");
        assert!(codes(&src).is_empty(), "{src}\n{:?}", codes(&src));
    }
}
