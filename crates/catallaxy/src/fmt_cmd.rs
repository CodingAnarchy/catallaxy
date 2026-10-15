//! `fmt` command: formats source files in place, checks them, or filters stdin to stdout.

use std::io::{IsTerminal, Read, Write};

use catallaxy_compiler::diag::{Diagnostic, SourceMap};
use catallaxy_compiler::fmt::format_source;

const USAGE: &str = "usage: catx fmt [--check] <file.cx|dir>...\n       catx fmt -    (stdin to stdout)";

fn report(path: &str, text: &str, diags: &[Diagnostic]) {
    let mut sm = SourceMap::default();
    sm.add(path.to_string(), text.to_string());
    let color = std::io::stderr().is_terminal();
    for d in diags {
        eprint!("{}", sm.render(d, color));
    }
    eprintln!("{path}: not formatted");
}

pub fn cmd_fmt(args: &[String]) -> i32 {
    let mut check = false;
    let mut files = Vec::new();
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "-" => files.push(a.as_str()),
            other if other.starts_with('-') => {
                eprintln!("unknown option `{other}`\n{USAGE}");
                return 2;
            }
            other => files.push(other),
        }
    }
    if files.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    if files.contains(&"-") {
        if files.len() > 1 {
            eprintln!("`-` (stdin) cannot be combined with file arguments\n{USAGE}");
            return 2;
        }
        return fmt_stdin(check);
    }
    let mut changed = 0;
    let mut failed = false;
    for path in crate::files::expand(&files, crate::files::MEANT_TO_FAIL) {
        let path = &*path.to_string_lossy();
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot read `{path}`: {e}");
                failed = true;
                continue;
            }
        };
        let out = match format_source(&src) {
            Ok(out) => out,
            Err(diags) => {
                report(path, &src, &diags);
                failed = true;
                continue;
            }
        };
        if out == src {
            continue;
        }
        changed += 1;
        if check {
            println!("{path}");
        } else if let Err(e) = std::fs::write(path, out) {
            eprintln!("cannot write `{path}`: {e}");
            failed = true;
        }
    }
    if failed {
        2
    } else if check && changed > 0 {
        1
    } else {
        0
    }
}

fn fmt_stdin(check: bool) -> i32 {
    let mut src = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut src) {
        eprintln!("cannot read stdin: {e}");
        return 2;
    }
    match format_source(&src) {
        Ok(out) if check => {
            if out == src {
                0
            } else {
                println!("<stdin>");
                1
            }
        }
        Ok(out) => {
            let mut stdout = std::io::stdout();
            if stdout.write_all(out.as_bytes()).and_then(|_| stdout.flush()).is_err() {
                return 2;
            }
            0
        }
        Err(diags) => {
            report("<stdin>", &src, &diags);
            2
        }
    }
}
