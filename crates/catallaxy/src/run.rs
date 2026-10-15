//! `run`, `check` and `disasm` commands.

use std::io::IsTerminal;
use std::path::Path;

use catallaxy_compiler::diag::SourceMap;
use catallaxy_compiler::driver::{compile_path, FsLoader, Options};

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
    let mut all_json = Vec::new();
    let mut codes = Vec::new();
    for f in files {
        let comp = compile_path(&f, &FsLoader, &Options { check_only: true });
        any_error |= comp.has_errors();
        if json {
            all_json.push(comp.sm.to_json(&comp.diags));
        } else {
            render_diags(&comp.sm, &comp.diags);
            codes.extend(comp.diags.iter().map(|d| d.code));
        }
    }
    print_code_help(codes);
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
        if (func.file as usize >= 2 || filter.is_some_and(|n| func.name.contains(n.as_str())))
            && filter.is_none_or(|n| func.name.contains(n.as_str()))
        {
            println!("{}", catallaxy_vm::bytecode::disassemble(&p, func));
        }
    }
    0
}
