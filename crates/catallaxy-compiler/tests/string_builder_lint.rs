//! W0004: string appends in loops, the StringBuilder rewrite and `# allow(...)` comments.

use std::path::Path;

use catallaxy_compiler::diag::Diagnostic;
use catallaxy_compiler::driver::{compile_source, Options};

fn diags(src: &str) -> Vec<Diagnostic> {
    compile_source(Path::new("t.cx"), src, &Options { check_only: true }).diags
}

/// The W0004 warnings of `src`: (1-based line of the append, whether a fix is offered).
fn lints(src: &str) -> Vec<(usize, bool)> {
    let ds = diags(src);
    assert!(ds.iter().all(|d| !d.is_error()), "{ds:?}");
    ds.iter()
        .filter(|d| d.code == "W0004")
        .map(|d| (src[..d.span.start as usize].matches('\n').count() + 1, !d.fixes.is_empty()))
        .collect()
}

/// `src` with the first fix of every W0004 applied.
fn fixed(src: &str) -> String {
    let mut edits: Vec<_> = diags(src)
        .into_iter()
        .filter(|d| d.code == "W0004")
        .flat_map(|d| {
            let f = d.fixes.into_iter().next().expect("a fix");
            assert!(f.applicable);
            f.edits
        })
        .collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e.span.start));
    let mut out = src.to_string();
    for e in edits {
        out.replace_range(e.span.start as usize..e.span.end as usize, &e.replacement);
    }
    out
}

#[test]
fn appends_in_loops_are_reported() {
    let src = "\
fn a(xs: List[String]) -> String {
  s = \"\"
  for x in xs {
    s += x
  }
  s
}
fn b(n: Int) -> String {
  s = \"<\"
  i = 0
  while i < n {
    s = s + \"#{i}\"
    i += 1
  }
  s
}
fn c(xs: List[Int]) -> String {
  s = \"\"
  xs.each { |x| s += \"#{x}\" }
  n = 0
  loop {
    n += 1
    if n > 5 { break }
    s += \".\"
  }
  s
}
";
    // The block on line 19 captures `s`, so the `loop` below cannot be rewritten mechanically.
    assert_eq!(lints(src), vec![(4, true), (12, true), (19, true), (24, false)]);
}

#[test]
fn quiet_cases() {
    let src = "\
fn small() -> String {
  s = \"\"
  for i in 0..9 { s += \"#{i}\" }
  for i in 0...16 { s += \"#{i}\" }
  4.times { s += \"x\" }
  (1..3).each { |i| s += \"#{i}\" }
  1.upto(5) { |i| s += \"#{i}\" }
  s
}
fn inside(xs: List[String]) -> Int {
  n = 0
  for x in xs {
    t = \"\"
    t += x
    n += t.length
  }
  n
}
fn ints(xs: List[Int]) -> Int {
  total = 0
  for x in xs { total += x }
  total
}
fn later(xs: List[String]) -> List[fn() -> String] {
  s = \"\"
  fs: List[fn() -> String] = []
  for x in xs { fs.push({ || s + x }) }
  fs
}
";
    assert_eq!(lints(src), vec![]);
}

#[test]
fn a_big_loop_around_a_small_one_is_reported() {
    let src = "\
fn f(rows: List[Int]) -> String {
  s = \"\"
  for r in rows {
    for i in 0..2 {
      s += \"#{r}#{i}\"
    }
  }
  s
}
";
    assert_eq!(lints(src), vec![(5, true)]);
    assert_eq!(
        fixed(src),
        "\
fn f(rows: List[Int]) -> String {
  s_sb = StringBuilder.new()
  for r in rows {
    for i in 0..2 {
      s_sb.push(\"#{r}#{i}\")
    }
  }
  s = s_sb.to_s
  s
}
"
    );
}

#[test]
fn allow_comments_silence_warnings() {
    let src = "\
fn f(xs: List[String]) -> String {
  s = \"\"
  t = \"\"
  u = \"\"
  for x in xs {
    s += x # allow(W0004)
    # allow(W0001, W0004): kept on purpose
    t += x
    u += x # allow(W0001)
  }
  s + t + u
}
";
    assert_eq!(lints(src), vec![(9, true)]);
}

#[test]
fn no_fix_when_the_rewrite_is_not_mechanical() {
    let src = "\
fn reads(xs: List[String]) -> String {
  s = \"\"
  for x in xs {
    s += x
    if s.length > 10 { break }
  }
  s
}
fn returns(xs: List[String]) -> String {
  s = \"\"
  for x in xs {
    if x == \"\" { return s }
    s += x
  }
  s
}
fn captured(xs: List[String]) -> String {
  s = \"\"
  show = { || print(s) }
  for x in xs {
    s += x
    show()
  }
  s
}
fn inline(xs: List[String]) -> String {
  s = \"\"
  if true { for x in xs { s += x } }
  s
}
fn value(xs: List[String]) -> List[Int] {
  s = \"\"
  xs.map { |x|
    s += x
    1
  }
}
";
    assert_eq!(lints(src), vec![(4, false), (13, false), (21, false), (28, false), (34, false)]);
}

#[test]
fn the_fix_keeps_the_value_and_formatting() {
    let src = "\
fn f(xs: List[String]) -> String {
  out = \"[\"
  xs.each { |x|
    out += x # one
    out = out + \",\"
  } # done
  out += \"]\"
  out
}
puts(f([\"a\", \"b\"]))
";
    let want = "\
fn f(xs: List[String]) -> String {
  out = \"[\"
  out_sb = StringBuilder.new()
  out_sb.push(out)
  xs.each { |x|
    out_sb.push(x) # one
    out_sb.push(\",\")
  } # done
  out = out_sb.to_s
  out += \"]\"
  out
}
puts(f([\"a\", \"b\"]))
";
    assert_eq!(fixed(src), want);
    assert_eq!(lints(want), vec![]);
}

#[test]
fn the_builder_name_does_not_clash() {
    let src = "s_sb = 1\nfn f(xs: List[String]) -> String {\n  s = \"\"\n  for x in xs { s += x }\n  s\n}\n";
    assert!(fixed(src).contains("s_sb2 = StringBuilder.new()\n  for x in xs { s_sb2.push(x) }\n  s = s_sb2.to_s\n"));
}
