//! `catx repl`: an interactive session.
//!
//! # Evaluation model: replay in a fresh process
//!
//! The VM's program loader appends to process-global, append-only registries (functions, classes,
//! globals, runtime types). Loading is incremental only when each new program extends the previous
//! one exactly, and the compiler gives no such guarantee: re-compiling a session with one more
//! input renumbers closures, rewrites the module's main function, and top-level variables are
//! locals of that main function, so their values cannot outlive one run.
//!
//! So the REPL keeps the *source* of every accepted input. For each new input it
//!
//! 1. type checks and compiles `session + input` in this process (errors are shown and the input
//!    is discarded, leaving the session unchanged);
//! 2. runs `session + marker + input + end marker` in a child `catx` process, discarding the
//!    child's output before the marker (the replayed session) and printing the rest;
//! 3. accepts the input into the session only if the child reached the end marker and exited
//!    successfully (a panic or `exit` discards it).
//!
//! An expression input whose type is not `Nil` is evaluated into a temporary and printed with
//! `inspect` (`=> 42`); a runtime `nil` is not printed.
//!
//! The cost of replay is that earlier side effects run again (silently) and nondeterministic
//! values (random numbers, clocks, thread interleavings) are recomputed on every input. The
//! child's stdin is empty so replayed reads cannot consume the REPL's own input.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use catallaxy_compiler::diag::Diagnostic;
use catallaxy_compiler::driver::{compile_source, Compilation, Options, PRELUDE};
use catallaxy_compiler::lexer::{lex, Tok};
use catallaxy_compiler::query;

/// Virtual path of the session module (imports resolve relative to the working directory).
const REPL_PATH: &str = "<repl>";

/// Exit status of `__repl-exec` when the program does not compile.
const EXEC_COMPILE_FAILED: i32 = 125;

pub fn cmd_repl(args: &[String]) -> i32 {
    if let Some(a) = args.first() {
        return crate::unexpected("repl", a);
    }
    let mut r = Repl::new();
    if std::io::stdin().is_terminal() {
        r.interactive()
    } else {
        r.piped()
    }
}

/// Runs a program given on stdin in the REPL's child process (hidden `__repl-exec` command).
pub fn cmd_repl_exec(_args: &[String]) -> i32 {
    let mut src = String::new();
    if std::io::stdin().read_to_string(&mut src).is_err() {
        return 2;
    }
    let comp = compile_source(Path::new(REPL_PATH), &src, &Options::default());
    let errors: Vec<&Diagnostic> = comp.diags.iter().filter(|d| d.is_error()).collect();
    for d in &errors {
        eprint!("{}", comp.sm.render(d, false));
    }
    let Some(program) = comp.program else { return EXEC_COMPILE_FAILED };
    crate::run::execute(program)
}

struct Repl {
    session: String,
    mark: String,
    end_mark: String,
    value_mark: String,
    color: bool,
}

enum Input {
    Complete,
    Incomplete,
}

impl Repl {
    fn new() -> Self {
        let nonce = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        );
        Repl {
            session: String::new(),
            mark: format!("__catx_repl_start_{nonce}__"),
            end_mark: format!("__catx_repl_end_{nonce}__"),
            value_mark: format!("__catx_repl_value_{nonce}__"),
            color: std::io::stderr().is_terminal(),
        }
    }

    fn interactive(&mut self) -> i32 {
        let mut ed = match rustyline::DefaultEditor::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("catx repl: {e}");
                return self.piped();
            }
        };
        let hist = std::env::var_os("HOME").map(|h| Path::new(&h).join(".catx_history"));
        if let Some(h) = &hist {
            let _ = ed.load_history(h);
        }
        println!("Catallaxy {} — :help for commands, :quit to exit", env!("CARGO_PKG_VERSION"));
        let mut buf = String::new();
        loop {
            let prompt = if buf.is_empty() { "cx> " } else { "... " };
            match ed.readline(prompt) {
                Ok(line) => {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(&line);
                    if matches!(input_state(&buf), Input::Incomplete) {
                        continue;
                    }
                    let input = std::mem::take(&mut buf);
                    if !input.trim().is_empty() {
                        let _ = ed.add_history_entry(input.as_str());
                    }
                    if !self.handle(&input) {
                        break;
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    buf.clear();
                }
                Err(_) => break,
            }
        }
        if let Some(h) = &hist {
            let _ = ed.save_history(h);
        }
        0
    }

    /// Non-interactive mode: no prompts, input lines grouped while brackets are open.
    fn piped(&mut self) -> i32 {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&line);
            if matches!(input_state(&buf), Input::Incomplete) {
                continue;
            }
            let input = std::mem::take(&mut buf);
            if !self.handle(&input) {
                return 0;
            }
        }
        if !buf.trim().is_empty() {
            self.handle(&buf);
        }
        0
    }

    /// Handles one complete input. Returns false to quit.
    fn handle(&mut self, input: &str) -> bool {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return true;
        }
        if let Some(cmd) = trimmed.strip_prefix(':') {
            let (name, arg) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
            match name {
                "q" | "quit" | "exit" => return false,
                "reset" => {
                    self.session.clear();
                    println!("session cleared");
                }
                "t" | "type" => self.show_type(arg.trim()),
                "session" => print!("{}", self.session),
                "h" | "help" => println!(
                    ":type <expr>   show the static type of an expression\n\
                     :reset         forget all definitions\n\
                     :session       print the accepted inputs\n\
                     :quit          leave the REPL"
                ),
                other => eprintln!("unknown command `:{other}` (try :help)"),
            }
            return true;
        }
        self.eval(input);
        true
    }

    fn base(&self) -> String {
        let mut s = self.session.clone();
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    fn print_diags(&self, comp: &Compilation, from: u32) {
        let file = PRELUDE.len() as u32;
        let mut errors = 0;
        for d in &comp.diags {
            // Warnings in earlier inputs were shown when they were entered.
            if !d.is_error() && d.span.file == file && d.span.start < from {
                continue;
            }
            errors += d.is_error() as usize;
            eprint!("{}", comp.sm.render(d, self.color));
        }
        if errors > 0 {
            let _ = std::io::stderr().flush();
        }
    }

    fn show_type(&self, expr: &str) {
        if expr.is_empty() {
            eprintln!("usage: :type <expr>");
            return;
        }
        let base = self.base();
        let from = base.len() as u32;
        let src = format!("{base}{expr}\n");
        let comp = compile_source(Path::new(REPL_PATH), &src, &Options { check_only: true });
        if comp.has_errors() {
            self.print_diags(&comp, from);
            return;
        }
        let ck = &comp.ck;
        let ty = query::module_of_file(ck, PRELUDE.len() as u32)
            .and_then(|m| query::last_stmt_from(ck, m, from))
            .and_then(|e| ck.expr_ty.get(&e.id))
            .map(|&t| ck.show(t));
        match ty {
            Some(t) => println!("{t}"),
            None => eprintln!("not an expression"),
        }
    }

    fn eval(&mut self, input: &str) {
        let base = self.base();
        let from = base.len() as u32;
        let src = format!("{base}{input}\n");
        let comp = compile_source(Path::new(REPL_PATH), &src, &Options::default());
        self.print_diags(&comp, from);
        if comp.program.is_none() {
            return;
        }

        // Expression inputs with a non-Nil type are evaluated into a temporary and printed.
        let ck = &comp.ck;
        let wrap = query::module_of_file(ck, PRELUDE.len() as u32)
            .and_then(|m| query::last_stmt_from(ck, m, from))
            .filter(|e| {
                !matches!(
                    e.kind,
                    catallaxy_compiler::ast::ExprKind::Assign { .. } | catallaxy_compiler::ast::ExprKind::Let { .. }
                )
            })
            .filter(|e| {
                ck.expr_ty.get(&e.id).is_some_and(|&t| {
                    let t = ck.types.shallow_resolve(t);
                    t != ck.types.nil
                        && t != ck.types.error
                        && !matches!(ck.types.get(t), catallaxy_compiler::types::Ty::Never)
                })
            })
            .map(|e| ((e.span.start - from) as usize, (e.span.end - from) as usize));
        let body = match wrap {
            Some((s, e)) => format!(
                "{}__catx_repl_value = (\n{}\n)\nputs(\"{}\" + inspect(__catx_repl_value))\n{}",
                &input[..s],
                &input[s..e],
                self.value_mark,
                &input[e..]
            ),
            None => input.to_string(),
        };
        let mut program =
            format!("{base}puts(\"{m}\")\neputs(\"{m}\")\n{body}\nputs(\"{e}\")\n", m = self.mark, e = self.end_mark);
        let mut outcome = self.run_child(&program);
        if wrap.is_some() && matches!(outcome, Some((_, _, EXEC_COMPILE_FAILED, _))) {
            // The value wrapper did not compile (unusual syntax); run the input as written.
            program = format!(
                "{base}puts(\"{m}\")\neputs(\"{m}\")\n{input}\nputs(\"{e}\")\n",
                m = self.mark,
                e = self.end_mark
            );
            outcome = self.run_child(&program);
        }
        let Some((stdout, stderr, code, reached_end)) = outcome else { return };
        print!("{stdout}");
        if !stdout.is_empty() && !stdout.ends_with('\n') {
            println!();
        }
        eprint!("{stderr}");
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        if code == 0 && reached_end {
            self.session = format!("{base}{input}\n");
        } else if code != 101 {
            eprintln!("(input discarded: program exited with status {code})");
        } else {
            eprintln!("(input discarded)");
        }
    }

    /// Runs a program in a child process. Returns the output after the start marker (with the
    /// value line rendered as `=> v`), the exit code, and whether the end marker was reached.
    fn run_child(&self, program: &str) -> Option<(String, String, i32, bool)> {
        let exe = std::env::current_exe().ok()?;
        let mut child = match Command::new(exe)
            .arg("__repl-exec")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("catx repl: cannot start evaluator: {e}");
                return None;
            }
        };
        {
            let mut stdin = child.stdin.take()?;
            let _ = stdin.write_all(program.as_bytes());
        }
        let out = child.wait_with_output().ok()?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let after = |s: &str| -> Option<String> {
            let i = s.find(&self.mark)?;
            let rest = &s[i + self.mark.len()..];
            Some(rest.strip_prefix('\n').unwrap_or(rest).to_string())
        };
        if code == EXEC_COMPILE_FAILED {
            return Some((String::new(), stderr, code, false));
        }
        let Some(mut stdout) = after(&stdout) else {
            // The replayed session itself failed before reaching the new input.
            eprint!("{stderr}");
            eprintln!("catx repl: replaying the session failed (status {code}); use :reset to start over");
            return None;
        };
        let stderr = after(&stderr).unwrap_or(stderr);
        let reached_end = match stdout.find(&self.end_mark) {
            Some(i) => {
                let tail = stdout[i + self.end_mark.len()..]
                    .strip_prefix('\n')
                    .unwrap_or(&stdout[i + self.end_mark.len()..])
                    .to_string();
                stdout.truncate(i);
                stdout.push_str(&tail);
                true
            }
            None => false,
        };
        if let Some(i) = stdout.find(&self.value_mark) {
            let rest = &stdout[i + self.value_mark.len()..];
            let (value, tail) = rest.split_once('\n').unwrap_or((rest, ""));
            let mut s = stdout[..i].to_string();
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            if value != "nil" {
                s.push_str(&format!("=> {value}\n"));
            }
            s.push_str(tail);
            stdout = s;
        }
        Some((stdout, stderr, code, reached_end))
    }
}

/// Whether an input is complete: brackets balanced, strings closed, and not ending in an
/// operator that continues the line.
fn input_state(src: &str) -> Input {
    if src.trim_start().starts_with(':') {
        return Input::Complete;
    }
    let (toks, diags) = lex(src, 0);
    if diags.iter().any(|d| d.code == "E0004") {
        return Input::Incomplete;
    }
    let mut depth = 0i32;
    let mut last: Option<&Tok> = None;
    for t in &toks {
        match &t.tok {
            Tok::Sym("(" | "[" | "{") => depth += 1,
            Tok::Sym(")" | "]" | "}") => depth -= 1,
            _ => {}
        }
        if !matches!(t.tok, Tok::Newline | Tok::Eof) {
            last = Some(&t.tok);
        }
    }
    if depth > 0 {
        return Input::Incomplete;
    }
    let continues = match last {
        Some(Tok::Sym(s)) => matches!(
            *s,
            "+" | "-"
                | "*"
                | "/"
                | "%"
                | "**"
                | "=="
                | "!="
                | "<"
                | "<="
                | ">"
                | ">="
                | "<=>"
                | "&&"
                | "||"
                | "??"
                | "="
                | "+="
                | "-="
                | "*="
                | "/="
                | "%="
                | "->"
                | "=>"
                | ","
                | "."
                | "?."
                | ".."
                | "..."
                | "|"
        ),
        Some(Tok::Kw(k)) => {
            use catallaxy_compiler::lexer::Kw;
            matches!(k, Kw::And | Kw::Or | Kw::Not | Kw::In | Kw::Is | Kw::As)
        }
        _ => false,
    };
    if continues {
        Input::Incomplete
    } else {
        Input::Complete
    }
}
