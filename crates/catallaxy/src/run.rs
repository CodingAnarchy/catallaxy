//! `run`, `check`, `disasm` and `test` commands.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use catallaxy_compiler::diag::SourceMap;
use catallaxy_compiler::driver::{compile_path, FsLoader, Options};
use serde_json::value::RawValue;

/// Prints `diags` to stderr, then (if any is an error) where their codes are explained.
fn print_diags(sm: &SourceMap, diags: &[catallaxy_compiler::diag::Diagnostic]) {
    render_diags(sm, diags);
    if diags.iter().any(|d| d.is_error()) {
        print_code_help(diags.iter().map(|d| d.code));
    }
}

fn render_diags(sm: &SourceMap, diags: &[catallaxy_compiler::diag::Diagnostic]) {
    let color = std::io::stderr().is_terminal();
    for d in diags {
        eprint!("{}", sm.render(d, color));
    }
    let errors = diags.iter().filter(|d| d.is_error()).count();
    if errors > 0 {
        eprintln!("{} error{} found", errors, if errors == 1 { "" } else { "s" });
    }
}

/// Points at the entries for `codes` in the error-code index (`docs/errors.md`).
fn print_code_help<'a>(codes: impl IntoIterator<Item = &'a str>) {
    let mut codes: Vec<&str> = codes.into_iter().collect();
    codes.sort_unstable();
    codes.dedup();
    let url = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/docs/errors.md");
    match codes.as_slice() {
        [] => {}
        [one] => eprintln!("help: for more on {one}, see {url}#{}", one.to_lowercase()),
        [init @ .., last] => eprintln!("help: for more on {} and {last}, see {url}", init.join(", ")),
    }
}

pub fn cmd_run(args: &[String]) -> i32 {
    let mut i = 0;
    let mut promote = false;
    while i < args.len() && args[i].starts_with("--") {
        match args[i].as_str() {
            "--promote-on-access" => promote = true,
            other => {
                eprintln!("unknown option `{other}`");
                return 2;
            }
        }
        i += 1;
    }
    let Some(file) = args.get(i) else {
        eprintln!("usage: catx run <file.cx> [args...]");
        return 2;
    };
    let prog_args: Vec<String> = args[i + 1..].to_vec();
    let comp = compile_path(Path::new(file), &FsLoader, &Options::default());
    if !comp.diags.is_empty() {
        print_diags(&comp.sm, &comp.diags);
    }
    let Some(program) = comp.program else { return 2 };
    if promote {
        catallaxy_runtime::ownership::PROMOTE_ON_ACCESS.store(true, std::sync::atomic::Ordering::Relaxed);
        // Stealing another thread's owned objects would race with its minor collections.
        catallaxy_gc::set_generational(false);
    }
    let _ = catallaxy_stdlib::ARGS.set(prog_args);
    let code = execute(program);
    #[cfg(feature = "op-profile")]
    eprint!("{}", catallaxy_vm::profile::report(25));
    code
}

pub use catallaxy::execute;

pub fn cmd_check(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let files: Vec<&str> = args.iter().filter(|a| !a.starts_with("--")).map(|a| a.as_str()).collect();
    if files.is_empty() {
        eprintln!("usage: catx check [--json] <file.cx|dir>...");
        return 2;
    }
    let files = crate::files::expand(&files, &[]);
    let mut any_error = false;
    let mut all_diags: Vec<Box<RawValue>> = Vec::new();
    let mut codes = Vec::new();
    for f in files {
        let comp = compile_path(&f, &FsLoader, &Options { check_only: true });
        any_error |= comp.has_errors();
        if json {
            all_diags.extend(diagnostics_json(&comp.sm, &comp.diags));
        } else {
            render_diags(&comp.sm, &comp.diags);
            codes.extend(comp.diags.iter().map(|d| d.code));
        }
    }
    print_code_help(codes);
    if json {
        let report = std::collections::BTreeMap::from([("diagnostics", all_diags)]);
        println!("{}", serde_json::to_string(&report).expect("serialize diagnostics"));
    } else if !any_error {
        eprintln!("ok");
    }
    if any_error {
        1
    } else {
        0
    }
}

/// The entries of the `diagnostics` array that [`SourceMap::to_json`] renders for `diags`, kept
/// verbatim so the merged output has the same field order as a single file's.
fn diagnostics_json(sm: &SourceMap, diags: &[catallaxy_compiler::diag::Diagnostic]) -> Vec<Box<RawValue>> {
    let text = sm.to_json(diags);
    let mut report: std::collections::HashMap<String, Vec<Box<RawValue>>> =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("invalid diagnostics JSON ({e}): {text}"));
    report.remove("diagnostics").unwrap_or_default()
}

pub fn cmd_disasm(args: &[String]) -> i32 {
    let Some(f) = args.first() else {
        eprintln!("usage: catx disasm <file.cx>");
        return 2;
    };
    let comp = compile_path(Path::new(f), &FsLoader, &Options::default());
    print_diags(&comp.sm, &comp.diags);
    let Some(p) = comp.program else { return 2 };
    let filter = args.get(1);
    for func in &p.funcs {
        if func.native.is_some() || func.code.is_empty() {
            continue;
        }
        if (func.file as usize >= 2 || filter.is_some_and(|n| func.name.contains(n.as_str())))
            && filter.is_none_or(|n| func.name.contains(n.as_str()))
        {
            println!("{}", catallaxy_vm::bytecode::disassemble(&p, func));
        }
    }
    0
}

/// Runs the conformance layout under `dir`:
/// `run/*.cx` with `.out` files (expected stdout), and `reject/*.cx` whose first line is
/// `# expect: CODE[, CODE...]` (which must fail `catx check` unless every code is a warning).
///
/// Each program gets `CATX_TEST_TIMEOUT` seconds (default 120); one that runs longer is killed
/// and fails, so a hang names its test instead of stalling the whole suite.
pub fn cmd_test(args: &[String]) -> i32 {
    let dir = PathBuf::from(args.first().map(|s| s.as_str()).unwrap_or("tests/conformance"));
    let exe = std::env::current_exe().expect("current exe");
    let limit =
        Duration::from_secs_f64(std::env::var("CATX_TEST_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(120.0));
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    let (mut pass, mut fail) = (0, 0);
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap_or_default();
        let first = src.lines().next().unwrap_or("");
        let result = 'case: {
            if let Some(codes) = first.strip_prefix("# expect:") {
                let want: Vec<String> =
                    codes.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
                let mut cmd = Command::new(&exe);
                cmd.args(["check", "--json"]).arg(f);
                let out = match output_within(&mut cmd, limit) {
                    Ok(out) => out,
                    Err(msg) => break 'case Err(msg),
                };
                let text = String::from_utf8_lossy(&out.stdout);
                let missing: Vec<&String> =
                    want.iter().filter(|c| !text.contains(&format!("\"code\":\"{c}\""))).collect();
                // Warnings alone (`W` codes) do not fail `catx check`.
                let want_failure = want.iter().any(|c| !c.starts_with('W'));
                if missing.is_empty() && (!want_failure || !out.status.success()) {
                    Ok(())
                } else {
                    Err(format!("expected {want:?}, got {}", text.trim()))
                }
            } else {
                let expected_out = f.with_extension("out");
                let want = std::fs::read_to_string(&expected_out).unwrap_or_default();
                let want_code: i32 = src
                    .lines()
                    .find_map(|l| l.strip_prefix("# exit:").map(|c| c.trim().parse().unwrap_or(0)))
                    .unwrap_or(0);
                let input = f.with_extension("in");
                let mut cmd = Command::new(&exe);
                cmd.arg("run").arg(f);
                if input.exists() {
                    cmd.stdin(std::fs::File::open(&input).unwrap());
                }
                let out = match output_within(&mut cmd, limit) {
                    Ok(out) => out,
                    Err(msg) => break 'case Err(msg),
                };
                let got = String::from_utf8_lossy(&out.stdout).to_string();
                let code = out.status.code().unwrap_or(-1);
                if got == want && code == want_code {
                    Ok(())
                } else if code != want_code {
                    Err(format!("exit code {code} (want {want_code})\n{}", String::from_utf8_lossy(&out.stderr)))
                } else {
                    Err(format!(
                        "output differs:\n--- want\n{want}--- got\n{got}{}",
                        String::from_utf8_lossy(&out.stderr)
                    ))
                }
            }
        };
        match result {
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

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "cx") {
            // Helper modules imported by tests live in `lib/` subdirectories.
            if p.components().any(|c| c.as_os_str() == "lib") {
                continue;
            }
            out.push(p);
        }
    }
}
