//! End-to-end tests of the `catx` command line: argument handling, exit statuses and output
//! formats of `check` and `test`.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("catx-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `catx` with `args`; returns the exit status, stdout and stderr.
fn catx<S: AsRef<OsStr>>(args: &[S]) -> (i32, String, String) {
    let out = Command::new(CATX).args(args).output().expect("run catx");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn check_json_merges_several_files_and_directories() {
    let dir = scratch_dir("check-json");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("a.cx"), "x: Int = \"s\"\n").unwrap();
    std::fs::write(dir.join("b.cx"), "puts(1)\n").unwrap();
    std::fs::write(dir.join("sub/c.cx"), "y: Int = \"t\"\nputs(nope)\n").unwrap();
    let other = scratch_dir("check-json-other");
    std::fs::write(other.join("d.cx"), "z: String = 1\n").unwrap();

    let (code, out, err) = catx(&[OsStr::new("check"), OsStr::new("--json"), dir.as_os_str(), other.as_os_str()]);
    assert_eq!(code, 1, "{out}{err}");
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("invalid JSON ({e}): {out}"));
    let diags = v["diagnostics"].as_array().unwrap();
    let files: Vec<&str> = diags.iter().map(|d| d["file"].as_str().unwrap()).collect();
    assert_eq!(diags.len(), 4, "{out}");
    assert!(files[0].ends_with("a.cx") && files[1].ends_with("c.cx") && files[3].ends_with("d.cx"), "{files:?}");
    for d in diags {
        assert!(d["code"].is_string() && d["fixes"].is_array() && d["notes"].is_array(), "{d}");
    }
    // Fields keep their documented order, `code` first, as for a single file.
    assert!(out.starts_with("{\"diagnostics\":[{\"code\":"), "{out}");

    // Clean files contribute nothing; no files with diagnostics is an empty array.
    let (code, out, _) = catx(&[OsStr::new("check"), OsStr::new("--json"), dir.join("b.cx").as_os_str()]);
    assert_eq!(code, 0);
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), serde_json::json!({"diagnostics": []}));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&other).ok();
}

/// `fmt` keeps a file's line endings (#184).
#[test]
fn fmt_keeps_crlf_line_endings() {
    let dir = scratch_dir("fmt-crlf");
    let tidy = dir.join("tidy.cx");
    std::fs::write(&tidy, "x = 1\r\nputs(x) # one\r\n").unwrap();
    let (code, out, err) = catx(&[OsStr::new("fmt"), OsStr::new("--check"), tidy.as_os_str()]);
    assert_eq!(code, 0, "an already formatted CRLF file is formatted: {out}{err}");
    let messy = dir.join("messy.cx");
    std::fs::write(&messy, "x=1\r\nif x>0 {\r\nputs( x )\r\n}\r\n").unwrap();
    let (code, out, err) = catx(&[OsStr::new("fmt"), messy.as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(std::fs::read_to_string(&messy).unwrap(), "x = 1\r\nif x > 0 {\r\n  puts(x)\r\n}\r\n");
    // A file with `\n` endings keeps them.
    let lf = dir.join("lf.cx");
    std::fs::write(&lf, "x=1\n").unwrap();
    catx(&[OsStr::new("fmt"), lf.as_os_str()]);
    assert_eq!(std::fs::read_to_string(&lf).unwrap(), "x = 1\n");
    // A syntax error in a CRLF file is reported at its real position.
    let bad = dir.join("bad.cx");
    std::fs::write(&bad, "x = 1\r\ny = (\r\n").unwrap();
    let (code, _, err) = catx(&[OsStr::new("fmt"), OsStr::new("--check"), bad.as_os_str()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("bad.cx:2:") || err.contains("bad.cx:3:"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Long operator chains run; nesting past the limit is a diagnostic from `check` and `run`
/// alike, never a stack overflow (#156).
#[test]
fn deep_and_long_expressions() {
    let dir = scratch_dir("deep");
    let long = dir.join("long.cx");
    std::fs::write(&long, format!("x = {}\nputs(x)\n", vec!["1"; 5_000].join(" + "))).unwrap();
    let (code, out, err) = catx(&[OsStr::new("run"), long.as_os_str()]);
    assert_eq!((code, out.trim()), (0, "5000"), "{err}");
    let deep = dir.join("deep.cx");
    std::fs::write(&deep, format!("x = {}1{}\nputs(x)\n", "(".repeat(10_000), ")".repeat(10_000))).unwrap();
    let (code, _, err) = catx(&[OsStr::new("check"), deep.as_os_str()]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("error[E0901]") && err.matches("error[").count() == 1, "{err}");
    let (code, _, err) = catx(&[OsStr::new("run"), deep.as_os_str()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("error[E0901]"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}
