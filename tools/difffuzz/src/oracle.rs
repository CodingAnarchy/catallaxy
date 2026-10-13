//! The oracle: runs one program under every configuration and compares the results.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One way of running a program: which build of `catx` and which environment.
#[derive(Clone, Debug)]
pub struct Mode {
    pub name: String,
    pub debug: bool,
    pub env: Vec<(String, String)>,
}

impl Mode {
    fn new(name: &str, debug: bool, env: &[(&str, &str)]) -> Mode {
        Mode { name: name.into(), debug, env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }
}

/// All known modes. The first is the baseline every other mode is compared against.
pub fn all_modes() -> Vec<Mode> {
    vec![
        Mode::new("release", false, &[]),
        Mode::new("debug", true, &[]),
        Mode::new("gc-stress", false, &[("CATX_GC_STRESS", "1")]),
        Mode::new("gc-nongen", false, &[("CATX_GC_GENERATIONAL", "0")]),
        Mode::new("gc-threads0", false, &[("CATX_GC_THREADS", "0")]),
        // Not in the default set (slow): both at once, and stress on the debug build.
        Mode::new("gc-stress-nongen", false, &[("CATX_GC_STRESS", "1"), ("CATX_GC_GENERATIONAL", "0")]),
        Mode::new("debug-gc-stress", true, &[("CATX_GC_STRESS", "1")]),
    ]
}

pub const DEFAULT_MODES: &[&str] = &["release", "debug", "gc-stress", "gc-nongen", "gc-threads0"];

#[derive(Clone)]
pub struct Bins {
    pub release: PathBuf,
    pub debug: PathBuf,
}

impl Bins {
    pub fn for_mode(&self, m: &Mode) -> &Path {
        if m.debug {
            &self.debug
        } else {
            &self.release
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    /// `None` when killed by a signal (or by the timeout).
    pub code: Option<i32>,
    pub timed_out: bool,
    pub elapsed: Duration,
}

/// Runs `bin args...` with extra environment, killing it after `timeout`.
pub fn run(bin: &Path, args: &[&str], env: &[(String, String)], timeout: Duration) -> RunResult {
    let start = Instant::now();
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Start from a clean slate so the caller's environment cannot change a mode.
    for k in ["CATX_GC_STRESS", "CATX_GC_GENERATIONAL", "CATX_GC_THREADS", "CATX_GC_TRACE", "CATX_GC_STATS"] {
        cmd.env_remove(k);
    }
    cmd.env("RUST_BACKTRACE", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunResult {
                stdout: String::new(),
                stderr: format!("difffuzz: cannot start {}: {e}", bin.display()),
                code: None,
                timed_out: false,
                elapsed: start.elapsed(),
            }
        }
    };
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = out.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = err.read_to_end(&mut b);
        b
    });
    let mut timed_out = false;
    let mut diag = String::new();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() > timeout {
                    timed_out = true;
                    diag = on_timeout(child.id());
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break None,
        }
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let mut stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    stderr.push_str(&diag);
    RunResult {
        stdout,
        stderr,
        code: if timed_out { None } else { status.and_then(|s| s.code()) },
        timed_out,
        elapsed: start.elapsed(),
    }
}

/// `DIFFFUZZ_ON_TIMEOUT`: a shell command run against a hung process before it is killed, with
/// `{pid}` replaced by its process id (for example `sample {pid} 1` on macOS, or
/// `gdb -p {pid} -batch -ex "thread apply all bt"` on Linux). Its output is added to the report.
fn on_timeout(pid: u32) -> String {
    let Ok(cmd) = std::env::var("DIFFFUZZ_ON_TIMEOUT") else { return String::new() };
    let cmd = cmd.replace("{pid}", &pid.to_string());
    match Command::new("sh").arg("-c").arg(&cmd).stdin(Stdio::null()).output() {
        Ok(o) => {
            format!("\n--- {cmd}\n{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
        }
        Err(e) => format!("\n--- {cmd}: {e}\n"),
    }
}

/// `catx check file`: `Err(diagnostics)` if the program is rejected.
pub fn check(bins: &Bins, file: &Path) -> Result<(), String> {
    let r = run(&bins.release, &["check", &file.to_string_lossy()], &[], Duration::from_secs(30));
    if r.code == Some(0) {
        Ok(())
    } else {
        Err(format!("{}{}", r.stdout, r.stderr))
    }
}

/// How much slower than the release baseline a mode may legitimately be before its timeout counts
/// as a hang.
const SLOWDOWN: u32 = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// A mode timed out, but the baseline was slow enough that it is not evidence of a hang.
    Slow,
    /// `kind` identifies the failure for deduplication and minimization.
    Fail {
        kind: String,
        detail: String,
    },
}

/// Text that only appears when the implementation itself is broken.
fn internal_marker(s: &str) -> Option<&'static str> {
    ["internal error", "panicked at", "RUST_BACKTRACE", "stack backtrace:", "has overflowed its stack"]
        .into_iter()
        .find(|m| s.contains(m))
}

/// The first line of a Catallaxy panic, with digits removed (thread ids, addresses and line
/// numbers may change while minimizing).
pub fn panic_signature(stderr: &str) -> String {
    let line = stderr.lines().find(|l| l.starts_with("panic:")).unwrap_or("");
    line.chars().filter(|c| !c.is_ascii_digit()).collect()
}

fn describe(r: &RunResult) -> String {
    let code = if r.timed_out {
        "timeout".to_string()
    } else {
        r.code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
    };
    format!("exit {code}, {:.2}s", r.elapsed.as_secs_f64())
}

/// Runs `file` under each mode; the first mode is the baseline.
///
/// `require_success`: the baseline must exit 0 (generated programs never panic by construction,
/// so a panic there is a bug in either the generator or the implementation).
pub fn judge(
    bins: &Bins,
    modes: &[Mode],
    file: &Path,
    timeout: Duration,
    require_success: bool,
) -> (Verdict, Vec<(String, RunResult)>) {
    let path = file.to_string_lossy().into_owned();
    let mut results: Vec<(String, RunResult)> = vec![];
    let mut verdict = Verdict::Pass;
    for m in modes {
        let r = run(bins.for_mode(m), &["run", &path], &m.env, timeout);
        let fail = |kind: &str, detail: String| Verdict::Fail { kind: format!("{kind}:{}", m.name), detail };
        let v = if r.timed_out {
            match results.first() {
                // The baseline itself was slow: another mode taking SLOWDOWN times as long is
                // expected (debug builds, collection after every allocation), not a hang.
                Some((_, base)) if base.elapsed * SLOWDOWN >= timeout => Verdict::Slow,
                _ => fail("timeout", format!("{} timed out after {:?}", m.name, timeout)),
            }
        } else if r.code.is_none() {
            fail("crash", format!("{} was killed by a signal\n{}", m.name, tail(&r.stderr)))
        } else if let Some(mark) = internal_marker(&r.stderr).or_else(|| internal_marker(&r.stdout)) {
            fail("internal", format!("{} output contains {mark:?}\n{}", m.name, tail(&r.stderr)))
        } else if let Some((_, base)) = results.first() {
            if base.stdout != r.stdout || base.code != r.code {
                fail(
                    "mismatch",
                    format!(
                        "{} differs from {} ({} vs {})\n{}",
                        m.name,
                        modes[0].name,
                        describe(&r),
                        describe(base),
                        first_diff(&base.stdout, &r.stdout)
                    ),
                )
            } else {
                Verdict::Pass
            }
        } else if require_success && r.code != Some(0) {
            Verdict::Fail {
                kind: format!("panic{}", panic_signature(&r.stderr)),
                detail: format!("baseline exited {:?}\n{}", r.code, tail(&r.stderr)),
            }
        } else {
            Verdict::Pass
        };
        results.push((m.name.clone(), r));
        if verdict == Verdict::Pass {
            verdict = v;
        }
        if verdict != Verdict::Pass {
            break;
        }
    }
    (verdict, results)
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(15);
    lines[start..].join("\n")
}

fn first_diff(a: &str, b: &str) -> String {
    let (al, bl): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    for i in 0..al.len().max(bl.len()) {
        let (x, y) = (al.get(i), bl.get(i));
        if x != y {
            return format!(
                "first difference at output line {}:\n  expected: {}\n  actual:   {}",
                i + 1,
                x.map(|s| truncate(s)).unwrap_or_else(|| "<end of output>".into()),
                y.map(|s| truncate(s)).unwrap_or_else(|| "<end of output>".into())
            );
        }
    }
    "outputs are equal; exit codes differ".into()
}

fn truncate(s: &str) -> String {
    if s.len() > 200 {
        let mut end = 200;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    } else {
        s.to_string()
    }
}

/// A human-readable report of every mode's result.
pub fn report(verdict: &Verdict, results: &[(String, RunResult)]) -> String {
    let mut s = String::new();
    if let Verdict::Fail { kind, detail } = verdict {
        s.push_str(&format!("FAILURE {kind}\n{detail}\n\n"));
    }
    for (name, r) in results {
        s.push_str(&format!("===== {name}: {}\n--- stdout\n{}\n--- stderr\n{}\n", describe(r), r.stdout, r.stderr));
    }
    s
}
