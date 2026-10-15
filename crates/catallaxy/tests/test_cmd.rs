//! End-to-end tests of `catx test`: which programs it finds and how it reports them.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("catx-test-cmd-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `catx test` with `args` in `cwd`; returns the exit status, stdout and stderr.
fn catx_test<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> (i32, String, String) {
    let out = Command::new(CATX).arg("test").args(args).current_dir(cwd).output().expect("run catx");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).replace('\\', "/"),
        String::from_utf8_lossy(&out.stderr).replace('\\', "/"),
    )
}

/// A project with two passing tests, a helper module and a failing test in `bad/`.
fn project(tag: &str) -> PathBuf {
    let dir = scratch_dir(tag);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::create_dir_all(dir.join("bad")).unwrap();
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    std::fs::write(dir.join("lib/helper.cx"), "pub fn two() -> Int { 2 }\n").unwrap();
    std::fs::write(dir.join("one.cx"), "import lib.helper.{two}\nputs(two())\n").unwrap();
    std::fs::write(dir.join("one.out"), "2\n").unwrap();
    std::fs::write(dir.join("reject.cx"), "# expect: E0300\nx: Int = \"s\"\n").unwrap();
    std::fs::write(dir.join("bad/wrong.cx"), "puts(1)\n").unwrap();
    std::fs::write(dir.join("bad/wrong.out"), "2\n").unwrap();
    dir
}

#[test]
fn missing_paths_and_empty_directories_fail() {
    let dir = project("missing");
    let (code, out, err) = catx_test(&dir, &["nonexistent"]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("cannot read `nonexistent`"), "{err}");
    // A missing path fails the run even next to a good one.
    let (code, _, err) = catx_test(&dir, &["one.cx", "nonexistent"]);
    assert_eq!(code, 2, "{err}");

    let (code, out, err) = catx_test(&dir, &["empty"]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("no tests found in `empty`"), "{err}");
    // `lib/` holds modules, not tests.
    let (code, _, err) = catx_test(&dir, &["lib"]);
    assert_eq!(code, 0, "a directory named directly is walked: {err}");
    let (code, _, err) = catx_test(&dir.join("lib"), &["."]);
    assert_eq!(code, 0, "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn files_and_several_paths_are_accepted() {
    let dir = project("files");
    let (code, out, err) = catx_test(&dir, &["one.cx"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.ends_with("1 passed, 0 failed\n"), "{out}");

    let (code, out, err) = catx_test(&dir, &["one.cx", "reject.cx"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.ends_with("2 passed, 0 failed\n"), "{out}");

    let (code, out, err) = catx_test(&dir, &["bad", "one.cx"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("FAIL bad/wrong.cx") && out.ends_with("1 passed, 1 failed\n"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn default_is_the_current_directory() {
    let dir = project("default");
    let (code, out, err) = catx_test::<&str>(&dir, &[]);
    assert_eq!(code, 1, "{out}{err}");
    // `lib/helper.cx` is not run as a test.
    assert!(out.ends_with("2 passed, 1 failed\n"), "{out}");
    std::fs::remove_dir_all(dir.join("bad")).unwrap();
    let (code, out, err) = catx_test::<&str>(&dir, &[]);
    assert_eq!(code, 0, "{out}{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_options_are_rejected() {
    let dir = project("options");
    let (code, _, err) = catx_test(&dir, &["--frobnicate", "."]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("unknown option `--frobnicate`"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn crlf_out_files_pass_and_failures_show_a_unified_diff() {
    let dir = scratch_dir("crlf");
    // A checkout with `core.autocrlf` turns the expected output's `\n` into `\r\n`.
    std::fs::write(dir.join("hi.cx"), "puts(\"hi\")\r\nputs(\"there\")\r\n").unwrap();
    std::fs::write(dir.join("hi.out"), "hi\r\nthere\r\n").unwrap();
    let (code, out, err) = catx_test(&dir, &["hi.cx"]);
    assert_eq!(code, 0, "{out}{err}");

    // A real difference is a unified diff, with the invisible trailing space made visible.
    std::fs::write(dir.join("space.cx"), "puts(\"a\")\nputs(\"b \")\nputs(\"c\")\n").unwrap();
    std::fs::write(dir.join("space.out"), "a\nb\nc\n").unwrap();
    let (code, out, err) = catx_test(&dir, &["space.cx"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("--- ") && out.contains("space.out\n+++ output\n"), "{out}");
    assert!(out.contains("@@ -1,3 +1,3 @@\n a\n-b\n+b·\n c\n"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}
