//! `run`, `check`, `disasm` and `test` commands.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use catallaxy_compiler::diag::SourceMap;
use catallaxy_compiler::driver::{compile_path, FsLoader, Options};
use catallaxy_runtime::Trap;
use catallaxy_vm::interp::MAIN_STACK;
use catallaxy_vm::{Program, Vm};

fn print_diags(sm: &SourceMap, diags: &[catallaxy_compiler::diag::Diagnostic]) {
    let color = std::io::stderr().is_terminal();
    for d in diags {
        eprint!("{}", sm.render(d, color));
    }
    let errors = diags.iter().filter(|d| d.is_error()).count();
    if errors > 0 {
        eprintln!("{} error{} found", errors, if errors == 1 { "" } else { "s" });
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
    }
    let _ = catallaxy_stdlib::ARGS.set(prog_args);
    execute(program)
}

/// Runs a program on a fresh main VM thread (with a large native stack). Returns the exit code.
pub fn execute(program: Program) -> i32 {
    let h = std::thread::Builder::new()
        .name("catx-main".into())
        .stack_size(256 << 20)
        .spawn(move || run_program(&program))
        .expect("spawn main thread");
    h.join().unwrap_or(101)
}

fn run_program(program: &Program) -> i32 {
    let mut vm = Vm::for_current_thread(MAIN_STACK);
    catallaxy_runtime::ownership::set_promotion_hook(|cls| {
        eprintln!("warning: promoted a {cls} to shared on access from another thread");
    });
    if let Err(e) = catallaxy_vm::load(program, &mut vm.m, &catallaxy_stdlib::lookup) {
        eprintln!("error: {}", e.0);
        return 70;
    }
    for &fid in &program.entry {
        match vm.call_func(fid, &[]) {
            Ok(_) => {}
            Err(Trap::Panic(msg)) => {
                catallaxy_stdlib::flush();
                eprintln!("panic: {msg}");
                if let Some(trace) = vm.panic_trace.take() {
                    for line in trace.iter().take(30) {
                        eprintln!("    at {line}");
                    }
                    if trace.len() > 30 {
                        eprintln!("    ... {} more", trace.len() - 30);
                    }
                }
                return 101;
            }
            Err(Trap::Exit(code)) => {
                catallaxy_stdlib::flush();
                return code;
            }
            Err(Trap::Unwind { .. }) => {
                catallaxy_stdlib::flush();
                eprintln!("panic: internal error: stray non-local exit");
                return 101;
            }
        }
    }
    catallaxy_stdlib::flush();
    0
}

pub fn cmd_check(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        eprintln!("usage: catx check [--json] <file.cx>...");
        return 2;
    }
    let mut any_error = false;
    let mut all_json = Vec::new();
    for f in files {
        let comp = compile_path(Path::new(f), &FsLoader, &Options { check_only: true });
        any_error |= comp.has_errors();
        if json {
            all_json.push(comp.sm.to_json(&comp.diags));
        } else {
            print_diags(&comp.sm, &comp.diags);
        }
    }
    if json {
        if all_json.len() == 1 {
            println!("{}", all_json[0]);
        } else {
            // Merge the per-file diagnostic arrays.
            let inner: Vec<String> = all_json
                .iter()
                .map(|j| j.trim_start_matches("{\"diagnostics\":[").trim_end_matches("]}").to_string())
                .filter(|s| !s.is_empty())
                .collect();
            println!("{{\"diagnostics\":[{}]}}", inner.join(","));
        }
    } else if !any_error {
        eprintln!("ok");
    }
    if any_error {
        1
    } else {
        0
    }
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
        if func.file as usize >= 2 || filter.is_some_and(|n| func.name.contains(n.as_str())) {
            if filter.is_none_or(|n| func.name.contains(n.as_str())) {
                println!("{}", catallaxy_vm::bytecode::disassemble(&p, func));
            }
        }
    }
    0
}

/// Runs the conformance layout under `dir`:
/// `run/*.cx` with `.out` files (expected stdout), and `reject/*.cx` whose first line is
/// `# expect: CODE[, CODE...]`.
pub fn cmd_test(args: &[String]) -> i32 {
    let dir = PathBuf::from(args.first().map(|s| s.as_str()).unwrap_or("tests/conformance"));
    let exe = std::env::current_exe().expect("current exe");
    let mut files = Vec::new();
    collect(&dir, &mut files);
    files.sort();
    let (mut pass, mut fail) = (0, 0);
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap_or_default();
        let first = src.lines().next().unwrap_or("");
        let result = if let Some(codes) = first.strip_prefix("# expect:") {
            let want: Vec<String> = codes.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
            let out = Command::new(&exe).args(["check", "--json"]).arg(f).output().expect("run catx");
            let text = String::from_utf8_lossy(&out.stdout);
            let missing: Vec<&String> = want.iter().filter(|c| !text.contains(&format!("\"code\":\"{c}\""))).collect();
            if missing.is_empty() && !out.status.success() {
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
            let out = cmd.output().expect("run catx");
            let got = String::from_utf8_lossy(&out.stdout).to_string();
            let code = out.status.code().unwrap_or(-1);
            if got == want && code == want_code {
                Ok(())
            } else if code != want_code {
                Err(format!("exit code {code} (want {want_code})\n{}", String::from_utf8_lossy(&out.stderr)))
            } else {
                Err(format!("output differs:\n--- want\n{want}--- got\n{got}{}", String::from_utf8_lossy(&out.stderr)))
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
