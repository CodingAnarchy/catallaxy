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
    assert!(err.contains("panic: MemoryError:") && err.contains("over the CATX_MAX_HEAP limit"), "{err}");
    assert!(err.contains("    at grow (") && err.contains("limit.cx:3)"), "{err}");
}
