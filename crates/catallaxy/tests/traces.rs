//! Panic traces and thread panics as `catx run` reports them on stderr.

use std::process::Command;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

/// Runs `src` as a program; returns (exit code, stdout, stderr).
fn run(tag: &str, src: &str) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!("catx-traces-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.cx");
    std::fs::write(&path, src).unwrap();
    let out = Command::new(CATX).arg("run").arg(&path).output().expect("run catx");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const F: &str = "fn f(x: Int) -> Int {\n  if x > 1 { panic(\"boom #{x}\") }\n  x\n}\n";

#[test]
fn entry_frame_is_main() {
    let (code, _, err) = run("main", &format!("{F}f(3)\n"));
    assert_eq!(code, 101);
    assert!(err.contains("panic: boom 3\n    at f (") && err.contains("t.cx:2)"), "{err}");
    assert!(err.contains("    at <main> (") && err.contains("t.cx:5)"), "{err}");
    assert!(!err.contains("<entry>"), "{err}");
}

#[test]
fn parallel_block_frames_are_kept() {
    // Whichever thread runs the failing element, the trace shows the block and its caller.
    let src = format!("{F}xs = (0...64).to_list()\nxs.par_map {{ |x| f(if x == 40 {{ 2 }} else {{ 0 }}) }}\n");
    for _ in 0..3 {
        let (code, _, err) = run("par", &src);
        assert_eq!(code, 101);
        assert!(err.contains("    at f ("), "{err}");
        assert!(err.contains("    at <parallel block in main> ("), "{err}");
        assert!(err.contains("    at <main> ("), "{err}");
    }
}

#[test]
fn joined_thread_panic_is_an_err_with_the_message() {
    let (code, out, err) = run("joined", &format!("{F}t = spawn {{ f(5) }}\nputs(t.join())\n"));
    assert_eq!(code, 0);
    assert_eq!(out, "Err(\"boom 5\")\n");
    assert!(!err.contains("panicked"), "{err}");
}

#[test]
fn unjoined_thread_panic_is_reported_at_exit() {
    let src = format!("{F}t = spawn {{ f(7) }}\nwhile not t.is_done() {{ sleep(0.01) }}\nputs(\"end\")\n");
    let (code, out, err) = run("unjoined", &src);
    assert_eq!(code, 0);
    assert_eq!(out, "end\n");
    assert!(err.contains("panicked and was never joined: boom 7\n    at f ("), "{err}");
    assert!(err.contains("    at <spawn in main> ("), "{err}");
}
