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

/// Lines of `out` with the per-test times (`(12 ms)`) taken out.
fn without_times(out: &str) -> String {
    out.lines().map(|l| l.split(" (").next().unwrap_or(l)).collect::<Vec<_>>().join("\n")
}

/// A suite of twelve tests: passing ones, a failure, and a slow first test.
fn suite(tag: &str) -> PathBuf {
    let dir = scratch_dir(tag);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    // The first test takes longest, so with -j the others finish before it.
    std::fs::write(dir.join("a/00_slow.cx"), "sleep(0.5)\nputs(\"slow\")\n").unwrap();
    std::fs::write(dir.join("a/00_slow.out"), "slow\n").unwrap();
    for i in 1..10 {
        std::fs::write(dir.join(format!("a/{i:02}.cx")), format!("puts({i})\n")).unwrap();
        std::fs::write(dir.join(format!("a/{i:02}.out")), format!("{i}\n")).unwrap();
    }
    std::fs::write(dir.join("b/wrong.cx"), "puts(1)\n").unwrap();
    std::fs::write(dir.join("b/wrong.out"), "2\n").unwrap();
    std::fs::write(dir.join("b/reject.cx"), "# expect: E0300\nx: Int = \"s\"\n").unwrap();
    dir
}

#[test]
fn parallel_runs_report_in_order() {
    let dir = suite("jobs");
    let (code1, out1, _) = catx_test(&dir, &["-v"]);
    assert_eq!(code1, 1);
    assert!(out1.ends_with("11 passed, 1 failed\n"), "{out1}");
    assert!(out1.starts_with("ok   ./a/00_slow.cx (") || out1.starts_with("ok   a/00_slow.cx ("), "{out1}");
    for jobs in [["-j", "4"], ["--jobs", "4"]] {
        let (code, out, err) = catx_test(&dir, &[jobs[0], jobs[1], "-v"]);
        assert_eq!(code, 1, "{err}");
        assert_eq!(without_times(&out), without_times(&out1), "the order does not depend on -j");
    }
    for args in [&["-j4", "-v"][..], &["--jobs=4", "-v"]] {
        let (_, out, _) = catx_test(&dir, args);
        assert_eq!(without_times(&out), without_times(&out1));
    }
    for bad in [&["-j", "0"][..], &["-j", "many"], &["-j"], &["--filter"]] {
        let (code, _, err) = catx_test(&dir, bad);
        assert_eq!(code, 2, "{bad:?}: {err}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn filters_select_tests_by_path() {
    let dir = suite("filter");
    let (code, out, err) = catx_test(&dir, &["--filter", "wrong"]);
    assert_eq!(code, 1, "{err}");
    assert!(out.ends_with("0 passed, 1 failed\n"), "{out}");
    let (code, out, _) = catx_test(&dir, &["--filter", "a/0", "--filter=reject"]);
    assert_eq!(code, 0);
    assert!(out.ends_with("11 passed, 0 failed\n"), "{out}");
    // Paths and filters combine.
    let (code, out, _) = catx_test(&dir, &["b", "--filter", "reject"]);
    assert_eq!(code, 0);
    assert!(out.ends_with("1 passed, 0 failed\n"), "{out}");
    let (code, _, err) = catx_test(&dir, &["--filter", "nothing-matches"]);
    assert_eq!(code, 2);
    assert!(err.contains("no tests match `nothing-matches`"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn json_report() {
    let dir = suite("json");
    // (Not `catx_test`, which rewrites backslashes: the report's own escapes must stay.)
    let o = Command::new(CATX).args(["test", "--json", "-j", "3", "b", "a/01.cx"]).current_dir(&dir).output().unwrap();
    let (code, out, err) =
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr));
    assert_eq!(code, Some(1), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!((v["passed"].as_u64(), v["failed"].as_u64()), (Some(2), Some(1)), "{v}");
    let tests = v["tests"].as_array().unwrap();
    let paths: Vec<String> = tests.iter().map(|t| t["path"].as_str().unwrap().replace('\\', "/")).collect();
    assert_eq!(paths, ["b/reject.cx", "b/wrong.cx", "a/01.cx"], "{v}");
    assert_eq!(tests[0]["kind"], "expect");
    assert_eq!(tests[1]["kind"], "run");
    assert_eq!(tests[1]["passed"], false);
    assert!(tests[1]["failure"].as_str().unwrap().contains("-2\n+1\n"), "{v}");
    assert!(tests[0].get("failure").is_none() && tests[0]["time_ms"].as_f64().unwrap() > 0.0, "{v}");
    std::fs::remove_dir_all(&dir).ok();
}
