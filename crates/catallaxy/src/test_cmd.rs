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

/// `catx test --help`.
pub const HELP: &str = "\
usage: catx test [options] [path...]

Runs test programs, each in its own process: every named `*.cx` file, and every `*.cx` file
under each named directory (skipping hidden directories, `target/` and `lib/`), by default
under the current directory. A program whose first line is `# expect: CODE, ...` must fail
`catx check` with those codes; any other must print its `.out` file and exit with status 0 (or
that of a `# exit: N` line), reading its `.in` file as input. A failure shows a diff of the
expected and actual output. Exits with 1 if a test fails, 2 if a path does not exist or no
tests are found.

    -j, --jobs N       run N tests at a time (default 1); results are reported in order
    --filter TEXT      run only the tests whose path contains TEXT (repeatable: any of them)
    -v, --verbose      list every test with its time, not only the failures
    --json             print one JSON report on stdout instead (see the reference, section 10)

Environment: CATX_TEST_TIMEOUT   seconds each program may run (default 120)
";

/// What the walk for tests skips: `lib/` holds modules that tests import.
const SKIP: crate::files::Skip = crate::files::Skip { dirs: &["lib"], expected_failures: false };

/// One test's result.
struct Outcome {
    /// The failure report; `None` if the test passed.
    failure: Option<String>,
    time: Duration,
}

pub fn cmd_test(args: &[String]) -> i32 {
    let mut paths: Vec<&str> = Vec::new();
    let mut filters: Vec<&str> = Vec::new();
    let (mut jobs, mut verbose, mut json) = (1usize, false, false);
    let mut it = args.iter().map(|a| a.as_str());
    while let Some(a) = it.next() {
        // `--jobs N`, `--jobs=N`, `-j N` and `-jN` all take a value.
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ if a.starts_with("-j") && a.len() > 2 => ("-j", Some(&a[2..])),
            _ => (a, None),
        };
        match flag {
            "-j" | "--jobs" | "--filter" => {
                let Some(value) = inline.or_else(|| it.next()) else {
                    eprintln!("`{flag}` needs a value\n\n{HELP}");
                    return 2;
                };
                if flag == "--filter" {
                    filters.push(value);
                } else {
                    match value.parse::<usize>() {
                        Ok(n) if n > 0 => jobs = n,
                        _ => {
                            eprintln!("`{flag}` takes a number of tests to run at a time (1 or more), not `{value}`");
                            return 2;
                        }
                    }
                }
            }
            "-v" | "--verbose" if inline.is_none() => verbose = true,
            "--json" if inline.is_none() => json = true,
            other if other.starts_with('-') => return crate::unexpected("test", a),
            _ => paths.push(a),
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
    let mut files = crate::files::expand(&paths, SKIP);
    if files.is_empty() {
        eprintln!("no tests found in {}", paths.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "));
        return 2;
    }
    if !filters.is_empty() {
        files.retain(|f| filters.iter().any(|t| shown(f).contains(t)));
        if files.is_empty() {
            let list = filters.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ");
            eprintln!("no tests match {list}");
            return 2;
        }
    }
    let exe = std::env::current_exe().expect("current exe");
    let limit =
        Duration::from_secs_f64(std::env::var("CATX_TEST_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(120.0));

    let (mut pass, mut fail) = (0, 0);
    let mut report = Vec::new();
    run_all(&exe, &files, limit, jobs, |f, o| {
        match &o.failure {
            None => pass += 1,
            Some(_) => fail += 1,
        }
        if json {
            report.push(test_json(f, &o));
        } else {
            match &o.failure {
                Some(msg) => println!("FAIL {} ({})\n{}\n", f.display(), millis(o.time), msg),
                None if verbose => println!("ok   {} ({})", f.display(), millis(o.time)),
                None => {}
            }
        }
    });
    if json {
        let doc = serde_json::json!({"tests": report, "passed": pass, "failed": fail});
        println!("{doc}");
    } else {
        println!("{pass} passed, {fail} failed");
    }
    if fail > 0 {
        1
    } else {
        0
    }
}

/// `f` as shown and matched by `--filter`, with `/` separators on every platform.
fn shown(f: &Path) -> String {
    f.to_string_lossy().replace('\\', "/")
}

fn millis(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

/// One entry of the `--json` report.
fn test_json(f: &Path, o: &Outcome) -> serde_json::Value {
    let kind = if crate::files::expects_failure(f) { "expect" } else { "run" };
    let mut v = serde_json::json!({
        "path": shown(f),
        "kind": kind,
        "passed": o.failure.is_none(),
        "time_ms": o.time.as_secs_f64() * 1000.0,
    });
    if let Some(msg) = &o.failure {
        v["failure"] = serde_json::Value::String(msg.clone());
    }
    v
}

/// Runs every test, `jobs` at a time, handing each result to `report` in the order of `files`
/// as soon as it and all before it are done.
fn run_all(
    exe: &Path,
    files: &[std::path::PathBuf],
    limit: Duration,
    jobs: usize,
    mut report: impl FnMut(&Path, Outcome),
) {
    let run = |f: &Path| {
        let start = Instant::now();
        let failure = run_one(exe, f, limit).err();
        Outcome { failure, time: start.elapsed() }
    };
    if jobs <= 1 || files.len() <= 1 {
        for f in files {
            report(f, run(f));
        }
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done: std::sync::Mutex<Vec<Option<Outcome>>> = std::sync::Mutex::new((0..files.len()).map(|_| None).collect());
    let ready = std::sync::Condvar::new();
    std::thread::scope(|s| {
        for _ in 0..jobs.min(files.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                // A test that cannot even be started must not leave the reporter waiting.
                let o = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(f))).unwrap_or_else(|_| Outcome {
                    failure: Some("internal error: the test could not be run".into()),
                    time: Duration::ZERO,
                });
                done.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(o);
                ready.notify_all();
            });
        }
        let mut g = done.lock().unwrap_or_else(|e| e.into_inner());
        for (i, f) in files.iter().enumerate() {
            let o = loop {
                if let Some(o) = g[i].take() {
                    break o;
                }
                g = ready.wait(g).unwrap_or_else(|e| e.into_inner());
            };
            drop(g);
            report(f, o);
            g = done.lock().unwrap_or_else(|e| e.into_inner());
        }
    });
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
