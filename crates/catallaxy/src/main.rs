//! `catx`: the Catallaxy command-line tool.

mod files;
mod fix_cmd;
mod fmt_cmd;
mod lsp;
mod repl;
mod run;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
catx — the Catallaxy language

USAGE:
    catx run [--promote-on-access] <file.cx> [args...]
    catx check [--json] <file.cx|dir>...
    catx fmt [--check] <file.cx|dir>...   (`catx fmt -` formats stdin to stdout)
    catx fix [--dry-run] <file.cx|dir>...
    catx test [dir]
    catx repl
    catx lsp
    catx disasm <file.cx>
    catx version
";

/// Network servers hold a descriptor per connection; the default soft limit (256 on macOS) is far
/// below what fibers can serve. Raise it toward the hard limit, as Go's runtime does.
#[cfg(unix)]
fn raise_fd_limit() {
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        // macOS rejects values above the per-process maximum, so step down until one sticks.
        for want in [lim.rlim_max, 1 << 20, 1 << 17, 65536, 10240] {
            if want <= lim.rlim_cur || want > lim.rlim_max {
                continue;
            }
            let new = libc::rlimit { rlim_cur: want, rlim_max: lim.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
        }
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

fn main() -> ExitCode {
    raise_fd_limit();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "run" => run::cmd_run(rest),
        "check" => run::cmd_check(rest),
        "fmt" => fmt_cmd::cmd_fmt(rest),
        "fix" => fix_cmd::cmd_fix(rest),
        "disasm" => run::cmd_disasm(rest),
        "test" => run::cmd_test(rest),
        "repl" => repl::cmd_repl(rest),
        "lsp" => lsp::cmd_lsp(rest),
        "__repl-exec" => repl::cmd_repl_exec(rest),
        "version" | "--version" | "-V" => {
            println!("catx {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            0
        }
        other if other.ends_with(".cx") => run::cmd_run(&args),
        other => {
            eprintln!("unknown command `{other}`\n");
            eprint!("{USAGE}");
            2
        }
    };
    ExitCode::from(code.clamp(0, 255) as u8)
}

pub fn path_arg(s: &str) -> PathBuf {
    PathBuf::from(s)
}
