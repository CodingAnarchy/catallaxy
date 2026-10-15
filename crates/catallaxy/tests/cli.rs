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
