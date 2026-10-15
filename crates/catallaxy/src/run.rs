//! `run`, `check` and `disasm` commands.

use std::io::IsTerminal;
use std::path::Path;

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
    // The index as of this release, which explains exactly the codes this `catx` emits.
    let url = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/v", env!("CARGO_PKG_VERSION"), "/docs/errors.md");
    match codes.as_slice() {
        [] => {}
        [one] => eprintln!("help: for more on {one}, see {url}#{}", one.to_lowercase()),
        [init @ .., last] => eprintln!("help: for more on {} and {last}, see {url}", init.join(", ")),
    }
}

pub fn cmd_run(args: &[String]) -> i32 {
    let mut i = 0;
    let mut promote = false;
    while i < args.len() && args[i].starts_with('-') {
        match args[i].as_str() {
            "--promote-on-access" => promote = true,
            "--help" | "-h" => {
                print!("{}", crate::command_help("run").unwrap_or_default());
                return 0;
            }
            other => return crate::unexpected("run", other),
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
    catallaxy_stdlib::install_signal_flush();
    let code = execute(program);
    #[cfg(feature = "op-profile")]
    eprint!("{}", catallaxy_vm::profile::report(25));
    code
}

pub use catallaxy::execute;

pub fn cmd_check(args: &[String]) -> i32 {
    let mut json = false;
    let mut files: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other if other.starts_with('-') && other != "-" => return crate::unexpected("check", other),
            other => files.push(other),
        }
    }
    if files.is_empty() {
        eprintln!("usage: catx check [--json] <file.cx|dir>...");
        return 2;
    }
    let files = crate::files::expand(&files, crate::files::MEANT_TO_FAIL);
    let mut any_error = false;
    // A file that cannot be read is a different failure from one with errors (status 2, not 1).
    let mut unreadable = false;
    let mut all_diags: Vec<Box<RawValue>> = Vec::new();
    let mut codes = Vec::new();
    for f in files {
        let comp = compile_path(&f, &FsLoader, &Options { check_only: true });
        any_error |= comp.has_errors();
        unreadable |= comp.diags.iter().any(is_unreadable);
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
    if unreadable {
        2
    } else if any_error {
        1
    } else {
        0
    }
}

/// Whether `d` says the file named on the command line could not be read (E0001 without a place
/// in a file; the same code in a file is an unexpected character).
fn is_unreadable(d: &catallaxy_compiler::diag::Diagnostic) -> bool {
    d.code == "E0001" && d.span.file == u32::MAX
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
    if let Some(other) = args.iter().find(|a| a.starts_with('-')) {
        return crate::unexpected("disasm", other);
    }
    if args.len() > 2 {
        return crate::unexpected("disasm", &args[2]);
    }
    let Some(f) = args.first() else {
        eprintln!("usage: catx disasm <file.cx> [function]");
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
