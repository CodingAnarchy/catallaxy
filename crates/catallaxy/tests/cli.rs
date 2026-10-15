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

/// Every command answers `--help` on stdout with status 0 (#185).
#[test]
fn help_on_every_command() {
    for cmd in ["run", "check", "fmt", "fix", "test", "repl", "lsp", "disasm", "version"] {
        for flag in ["--help", "-h"] {
            let (code, out, err) = catx(&[cmd, flag]);
            assert_eq!(code, 0, "catx {cmd} {flag}: {err}");
            assert!(out.starts_with(&format!("usage: catx {cmd}")), "catx {cmd} {flag}: {out}");
        }
        let (code, out, _) = catx(&["help", cmd]);
        assert_eq!(code, 0);
        assert!(out.starts_with(&format!("usage: catx {cmd}")), "{out}");
    }
    let (code, out, _) = catx(&["--help"]);
    assert!(code == 0 && out.contains("Exit status"), "{out}");
    assert_eq!(catx(&["help", "frobnicate"]).0, 2);
    // After the program file, `--help` is the program's argument.
    let dir = scratch_dir("help");
    let prog = dir.join("args.cx");
    std::fs::write(&prog, "puts(args())\n").unwrap();
    let (code, out, err) = catx(&[OsStr::new("run"), prog.as_os_str(), OsStr::new("--help")]);
    assert_eq!((code, out.trim()), (0, "[\"--help\"]"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Unknown options and stray arguments are errors (status 2) on every command (#185).
#[test]
fn strict_arguments() {
    let dir = scratch_dir("strict");
    let bad = dir.join("bad.cx");
    std::fs::write(&bad, "x: Int = \"s\"\n").unwrap();
    let bad = bad.to_str().unwrap();
    for args in [
        vec!["check", "--jsno", bad],
        vec!["check", "-j", bad],
        vec!["run", "--frobnicate", bad],
        vec!["fmt", "--frobnicate", bad],
        vec!["fix", "--frobnicate", bad],
        vec!["test", "--frobnicate", bad],
        vec!["disasm", "--frobnicate", bad],
        vec!["repl", "extra"],
        vec!["lsp", "--frobnicate"],
        vec!["version", "extra"],
    ] {
        let (code, out, err) = catx(&args);
        assert_eq!(code, 2, "{args:?}: {out}{err}");
        assert!(err.contains("unknown option") || err.contains("unexpected argument"), "{args:?}: {err}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A file that cannot be read says why, and is status 2 from every command (#185).
#[test]
fn unreadable_files_say_why() {
    let dir = scratch_dir("unreadable");
    std::fs::create_dir_all(dir.join("adir")).unwrap();
    std::fs::write(dir.join("latin1.cx"), b"puts(\"caf\xe9\")\n").unwrap();
    let cases = [
        (dir.join("missing.cx"), "missing.cx`: "),
        (dir.join("adir"), "adir`: it is a directory"),
        (dir.join("latin1.cx"), "latin1.cx`: it is not UTF-8 text"),
    ];
    for (path, why) in &cases {
        // (`check` walks a directory instead.)
        let cmds: &[&str] = if path.is_dir() { &["run"] } else { &["check", "run"] };
        for &cmd in cmds {
            let (code, _, err) = catx(&[OsStr::new(cmd), path.as_os_str()]);
            assert_eq!(code, 2, "catx {cmd} {}: {err}", path.display());
            assert!(err.contains("error[E0001]: cannot read `") && err.contains(why), "catx {cmd}: {err}");
        }
    }
    // `check` names the OS reason for a missing file, as `fix` does.
    let (_, _, err) = catx(&[OsStr::new("check"), cases[0].0.as_os_str()]);
    assert!(err.contains("(os error 2)"), "{err}");
    for cmd in ["fmt", "fix"] {
        let (code, _, err) = catx(&[OsStr::new(cmd), cases[0].0.as_os_str()]);
        assert_eq!(code, 2, "catx {cmd}: {err}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Walking a directory, `check`, `fmt` and `fix` leave out programs meant to fail: `reject/`
/// directories and files whose first line is `# expect:`. Named directly, they are checked (#185).
#[test]
fn walks_skip_expected_failures() {
    let dir = scratch_dir("expected-failures");
    std::fs::create_dir_all(dir.join("reject")).unwrap();
    std::fs::write(dir.join("main.cx"), "puts(1)\n").unwrap();
    std::fs::write(dir.join("misuse_test.cx"), "# expect: E0300\nx: Int = \"s\"\n").unwrap();
    std::fs::write(dir.join("reject/wrong.cx"), "y: Int = \"t\"\n").unwrap();
    for cmd in ["check", "fix"] {
        let (code, out, err) = catx(&[OsStr::new(cmd), dir.as_os_str()]);
        assert_eq!(code, 0, "catx {cmd} dir: {out}{err}");
    }
    let (code, out, err) = catx(&[OsStr::new("fmt"), OsStr::new("--check"), dir.as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    let (code, _, err) = catx(&[OsStr::new("check"), dir.join("misuse_test.cx").as_os_str()]);
    assert_eq!(code, 1, "{err}");
    // `catx test` still runs it, as the failing test it is meant to be.
    let (code, out, err) = catx(&[OsStr::new("test"), dir.join("misuse_test.cx").as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The link after errors points at the error index of this version's tag (#185).
#[test]
fn help_link_is_pinned_to_the_version() {
    let dir = scratch_dir("link");
    let bad = dir.join("bad.cx");
    std::fs::write(&bad, "x: Int = \"s\"\n").unwrap();
    let (_, _, err) = catx(&[OsStr::new("check"), bad.as_os_str()]);
    let want = format!("/blob/v{}/docs/errors.md#e0300", env!("CARGO_PKG_VERSION"));
    assert!(err.contains(&want), "{err}");
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
