//! Out-of-memory conditions the runtime can see coming end the program like any other panic:
//! `panic: MemoryError: ...`, a Catallaxy stack trace and exit status 101, not an allocator abort.

use std::process::Command;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

fn run(name: &str, src: &str, env: &[(&str, &str)]) -> (i32, String) {
    let dir = std::env::temp_dir().join(format!("catx-memory-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    let out = Command::new(CATX).arg("run").arg(&path).envs(env.iter().copied()).output().expect("run catx");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn oversized_request_panics_with_a_trace() {
    let src = "fn build() -> Int {\n  xs = List.filled(100_000_000_000_000, 0)\n  xs.length\n}\nputs(build())\n";
    let (code, err) = run("oversized.cx", src, &[]);
    assert_eq!(code, 101, "{err}");
    assert!(err.contains("panic: MemoryError: cannot allocate 800000000000000 bytes"), "{err}");
    assert!(err.contains("    at build (") && err.contains("oversized.cx:2)"), "{err}");
}

#[test]
fn heap_limit_panics_with_a_trace() {
    let src = "fn grow() -> Int {\n  xs: List[List[Int]] = []\n  for i in 0...50_000_000 { xs.push([i]) }\n  xs.length\n}\nputs(grow())\n";
    let (code, err) = run("limit.cx", src, &[("CATX_MAX_HEAP", "32M")]);
    assert_eq!(code, 101, "{err}");
    // Either a collection finds too much live data, or growing the list would pass the cap.
    assert!(err.contains("panic: MemoryError:") && err.contains("the CATX_MAX_HEAP limit of 33.6 MB"), "{err}");
    assert!(err.contains("    at grow (") && err.contains("limit.cx:3)"), "{err}");
}

/// A JSON document of 400,000 small objects.
const BIG_JSON: &str =
    r#"text = "[" + (0...400_000).to_list().map { |i| "{\"k\": [#{i}, \"v#{i}\"]}" }.join(",") + "]""#;

/// Buffers outside the heap count toward the cap (issue #158): a `StringBuilder` grown forever,
/// a list and a dict grown forever, and `Json.parse` of a document whose parsed form does not
/// fit each raise a `MemoryError` naming the cap, at about the cap rather than at twice it or
/// never.
#[test]
fn native_buffers_count_toward_the_heap_limit() {
    let json = format!("fn grow() -> Int {{\n  {BIG_JSON}\n  Json.parse(text)\n  0\n}}\nputs(grow())\n");
    let cases = [
        (
            "sb.cx",
            "fn grow() -> Int {\n  sb = StringBuilder.new()\n  chunk = \"x\" * 1000\n  while true { sb.push(chunk) }\n  0\n}\nputs(grow())\n".to_string(),
            "MemoryError: cannot grow a StringBuilder: ",
        ),
        (
            "list.cx",
            "fn grow() -> Int {\n  xs: List[Int] = []\n  while true { xs.push(1) }\n  0\n}\nputs(grow())\n".to_string(),
            "MemoryError: cannot grow a list to ",
        ),
        (
            "dict.cx",
            "fn grow() -> Int {\n  d: Dict[Int, Int] = [:]\n  i = 0\n  while true {\n    d[i] = i\n    i += 1\n  }\n  0\n}\nputs(grow())\n".to_string(),
            "MemoryError: cannot grow a table to ",
        ),
        ("json.cx", json, "MemoryError: Json.parse: "),
    ];
    for (name, src, want) in cases {
        let (code, err) = run(name, &src, &[("CATX_MAX_HEAP", "64M")]);
        assert_eq!(code, 101, "{name}: {err}");
        assert!(err.contains(&format!("panic: {want}")), "{name}: {err}");
        assert!(err.contains("would pass the CATX_MAX_HEAP limit of 67.1 MB"), "{name}: {err}");
        assert!(err.contains("    at grow ("), "{name}: {err}");
    }
    // With the cap off, the same document parses.
    let src = format!("{BIG_JSON}\nputs(Json.parse(text).unwrap().length)\n");
    let (code, err) = run("json_off.cx", &src, &[("CATX_MAX_HEAP", "off")]);
    assert_eq!(code, 0, "{err}");
}
