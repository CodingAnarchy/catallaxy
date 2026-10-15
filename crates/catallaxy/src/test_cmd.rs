//! `test` command: runs conformance-style test programs, each in its own process.
//!
//! A test is a `*.cx` file. One whose first line is `# expect: CODE[, CODE...]` must fail
//! `catx check` with those codes (unless every code is a warning). Any other must run with
//! standard output equal to its `.out` file (empty if there is none; line endings are not
//! compared) and exit with status 0, or
//! with the status of a `# exit: N` line; a `.in` file next to it is fed to standard input.
//!
//! Each program gets `CATX_TEST_TIMEOUT` seconds (default 120); one that runs longer is killed
//! and fails, so a hang names its test instead of stalling the whole suite.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: catx test [path...]    (default: the current directory)";

/// Directories skipped while walking for tests: `lib/` holds modules that tests import.
const SKIP_DIRS: &[&str] = &["lib"];

pub fn cmd_test(args: &[String]) -> i32 {
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            other if other.starts_with('-') => {
                eprintln!("unknown option `{other}`\n{USAGE}");
                return 2;
            }
            other => paths.push(other),
        }
    }
    if paths.is_empty() {
        paths.push(".");
    }
    for p in &paths {
        if let Err(e) = std::fs::metadata(p) {
            eprintln!("cannot read `{p}`: {e}");
            return 2;
        }
    }
    let files = crate::files::expand(&paths, SKIP_DIRS);
    if files.is_empty() {
        eprintln!("no tests found in {}", paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "));
        return 2;
    }
    let exe = std::env::current_exe().expect("current exe");
    let limit =
        Duration::from_secs_f64(std::env::var("CATX_TEST_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(120.0));
    let (mut pass, mut fail) = (0, 0);
    for f in &files {
        match run_one(&exe, f, limit) {
            Ok(()) => pass += 1,
            Err(msg) => {
                fail += 1;
                println!("FAIL {}\n{}\n", f.display(), msg);
            }
        }
    }
    println!("{pass} passed, {fail} failed");
    if fail > 0 {
        1
    } else {
        0
    }
}

/// Runs the test program `f`; the error is the failure report.
fn run_one(exe: &Path, f: &Path, limit: Duration) -> Result<(), String> {
    let src = std::fs::read_to_string(f).unwrap_or_default();
    let first = src.lines().next().unwrap_or("");
    if let Some(codes) = first.strip_prefix("# expect:") {
        let want: Vec<String> = codes.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        let mut cmd = Command::new(exe);
        cmd.args(["check", "--json"]).arg(f);
        let out = output_within(&mut cmd, limit)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let missing: Vec<&String> = want.iter().filter(|c| !text.contains(&format!("\"code\":\"{c}\""))).collect();
        // Warnings alone (`W` codes) do not fail `catx check`.
        let want_failure = want.iter().any(|c| !c.starts_with('W'));
        if missing.is_empty() && (!want_failure || !out.status.success()) {
            Ok(())
        } else {
            Err(format!("expected {want:?}, got {}", text.trim()))
        }
    } else {
        let out_file = f.with_extension("out");
        // Line endings are not compared: a checkout may have turned `\n` into `\r\n`.
        let want = std::fs::read_to_string(&out_file).unwrap_or_default().replace("\r\n", "\n");
        let want_code: i32 =
            src.lines().find_map(|l| l.strip_prefix("# exit:").map(|c| c.trim().parse().unwrap_or(0))).unwrap_or(0);
        let input = f.with_extension("in");
        let mut cmd = Command::new(exe);
        cmd.arg("run").arg(f);
        if input.exists() {
            let file = std::fs::File::open(&input).map_err(|e| format!("cannot read `{}`: {e}", input.display()))?;
            cmd.stdin(file);
        }
        let out = output_within(&mut cmd, limit)?;
        let got = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        let code = out.status.code().unwrap_or(-1);
        if got == want && code == want_code {
            Ok(())
        } else if code != want_code {
            Err(format!("exit code {code} (want {want_code})\n{}", String::from_utf8_lossy(&out.stderr)))
        } else {
            let diff = crate::diff::unified(&out_file.display().to_string(), &want, "output", &got);
            Err(format!("output differs:\n{diff}{}", String::from_utf8_lossy(&out.stderr)))
        }
    }
}

/// Runs `cmd` to completion like [`Command::output`], killing it after `limit`.
fn output_within(cmd: &mut Command, limit: Duration) -> Result<Output, String> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("run catx");
    // Drain both pipes on their own threads so a chatty child cannot block on a full pipe.
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut b).ok();
        b
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut b).ok();
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for catx") {
            break Some(status);
        }
        if start.elapsed() >= limit {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (stdout, stderr) = (out.join().unwrap(), err.join().unwrap());
    match status {
        Some(status) => Ok(Output { status, stdout, stderr }),
        None => Err(format!(
            "timed out after {:?}\n--- stdout\n{}--- stderr\n{}",
            limit,
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )),
    }
}
