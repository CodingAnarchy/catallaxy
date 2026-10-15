//! `catx`: the Catallaxy command-line tool.

mod files;
mod fix_cmd;
mod fmt_cmd;
mod lsp;
mod repl;
mod run;
mod test_cmd;

use std::path::PathBuf;
use std::process::ExitCode;

/// The system allocator, with the runtime's out-of-memory headroom (see `catallaxy_gc::Allocator`).
#[global_allocator]
static ALLOC: catallaxy_gc::Allocator = catallaxy_gc::Allocator;

const USAGE: &str = "\
catx — the Catallaxy language

USAGE:
    catx run [--promote-on-access] <file.cx> [args...]
    catx check [--json] <file.cx|dir>...
    catx fmt [--check] <file.cx|dir>...   (`catx fmt -` formats stdin to stdout)
    catx fix [--dry-run] <file.cx|dir>...
    catx test [path...]
    catx repl
    catx lsp
    catx disasm <file.cx> [function]
    catx version

`catx <command> --help` describes a command. Exit status: 0 success, 1 problems found (errors,
failed tests, unformatted files), 2 a usage error or a file that cannot be read or does not
compile; `catx run` otherwise exits with the program's status.
";

/// `catx <command> --help`: each command's usage and options.
fn command_help(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "run" => "\
usage: catx run [--promote-on-access] <file.cx> [args...]

Compiles and runs a program; arguments after the file go to the program (`args()`).
Exits with the program's status (101 after a panic, 141 when its output pipe closes).

    --promote-on-access   promote objects another thread touches to shared instead of
                          panicking (a debugging aid; turns off the generational collector)
",
        "check" => "\
usage: catx check [--json] <file.cx|dir>...

Type-checks without running. Directories are walked for `*.cx` files, skipping hidden
directories, `target/`, `reject/` and files whose first line is `# expect:` (tests that are
meant to fail). Exits with 1 if there are errors, 2 if a file cannot be read.

    --json   print the diagnostics as JSON on stdout
",
        "fmt" => "\
usage: catx fmt [--check] <file.cx|dir>...
       catx fmt -    (stdin to stdout)

Formats files in place (directories walked as for `catx check`). Exits with 1 if `--check`
finds files that would change, 2 if a file cannot be read, written or parsed.

    --check   list the files that would change instead of changing them
",
        "fix" => "\
usage: catx fix [--dry-run] <file.cx|dir>...

Applies the checker's machine-applicable fixes (directories walked as for `catx check`).
Exits with 1 if errors remain, 2 if a file cannot be read or written.

    -n, --dry-run   print the fixes without writing them
",
        "test" => crate::test_cmd::HELP,
        "repl" => "\
usage: catx repl

An interactive session (or, with stdin not a terminal, a session read from stdin).
",
        "lsp" => "\
usage: catx lsp

A language server over stdin and stdout (`--stdio`, which some clients pass, is accepted).
",
        "disasm" => "\
usage: catx disasm <file.cx> [function]

Prints the bytecode of the program's functions (those whose name contains `function`).
",
        "version" => "usage: catx version\n\nPrints the version.\n",
        _ => return None,
    })
}

/// Whether `args` ask for help: `--help` or `-h` as the first argument, or anywhere for commands
/// that take no program arguments.
fn wants_help(cmd: &str, args: &[String]) -> bool {
    let is_help = |a: &String| a == "--help" || a == "-h";
    match cmd {
        // After the file, arguments belong to the program; `run::cmd_run` checks its options.
        "run" => false,
        _ => args.iter().any(is_help),
    }
}

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
    if wants_help(cmd, rest) {
        if let Some(help) = command_help(cmd) {
            print!("{help}");
            return ExitCode::SUCCESS;
        }
    }
    let code = match cmd.as_str() {
        "run" => run::cmd_run(rest),
        "check" => run::cmd_check(rest),
        "fmt" => fmt_cmd::cmd_fmt(rest),
        "fix" => fix_cmd::cmd_fix(rest),
        "disasm" => run::cmd_disasm(rest),
        "test" => test_cmd::cmd_test(rest),
        "repl" => repl::cmd_repl(rest),
        "lsp" => lsp::cmd_lsp(rest),
        "__repl-exec" => repl::cmd_repl_exec(rest),
        "version" | "--version" | "-V" => match rest.first() {
            Some(other) => unexpected(cmd, other),
            None => {
                println!("catx {}", env!("CARGO_PKG_VERSION"));
                0
            }
        },
        "help" | "--help" | "-h" => match rest.first().map(|c| (c, command_help(c))) {
            Some((_, Some(help))) => {
                print!("{help}");
                0
            }
            Some((other, None)) => {
                eprintln!("unknown command `{other}`\n");
                eprint!("{USAGE}");
                2
            }
            None => {
                print!("{USAGE}");
                0
            }
        },
        other if other.ends_with(".cx") => run::cmd_run(&args),
        other => {
            eprintln!("unknown command `{other}`\n");
            eprint!("{USAGE}");
            2
        }
    };
    ExitCode::from(code.clamp(0, 255) as u8)
}

/// Reports an argument `cmd` does not take, with its usage; the exit status for that.
pub fn unexpected(cmd: &str, arg: &str) -> i32 {
    if arg.starts_with('-') {
        eprintln!("unknown option `{arg}`");
    } else {
        eprintln!("unexpected argument `{arg}`");
    }
    if let Some(help) = command_help(cmd) {
        eprint!("\n{help}");
    }
    2
}

pub fn path_arg(s: &str) -> PathBuf {
    PathBuf::from(s)
}
