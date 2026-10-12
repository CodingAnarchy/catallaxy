//! `catx`: the Catallaxy command-line tool.

mod run;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
catx — the Catallaxy language

USAGE:
    catx run [--promote-on-access] <file.cx> [args...]
    catx check [--json] <file.cx>...
    catx fmt [--check] <file.cx>...
    catx test [dir]
    catx repl
    catx lsp
    catx disasm <file.cx>
    catx version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "run" => run::cmd_run(rest),
        "check" => run::cmd_check(rest),
        "disasm" => run::cmd_disasm(rest),
        "test" => run::cmd_test(rest),
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
