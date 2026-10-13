//! `fix` command: applies the checker's machine-applicable fixes to source files.
//!
//! Each file is checked, the first applicable fix of each of its diagnostics is applied (fixes
//! whose edits overlap an earlier one wait for the next round), and the file is checked again,
//! until no fix applies or [`MAX_ROUNDS`] is reached. Only fixes marked applicable are used:
//! suggestions that guess (`did you mean`) or leave a placeholder are left to the editor. A file
//! that was formatted before is formatted again after its fixes.

use std::path::Path;

use catallaxy_compiler::diag::{Diagnostic, Edit, SourceFile};
use catallaxy_compiler::driver::{compile_source, Options, PRELUDE};
use catallaxy_compiler::fmt::format_source;

const USAGE: &str = "usage: catx fix [--dry-run] <file.cx|dir>...";

/// Rounds of check-then-fix per file; fixes can uncover further fixable diagnostics.
const MAX_ROUNDS: usize = 10;

/// One applied fix, for the report.
struct Applied {
    line: u32,
    col: u32,
    code: &'static str,
    message: String,
}

/// The result of fixing one file's text.
struct Outcome {
    text: String,
    applied: Vec<Applied>,
    errors_left: usize,
}

pub fn cmd_fix(args: &[String]) -> i32 {
    let mut dry_run = false;
    let mut paths = Vec::new();
    for a in args {
        match a.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            other if other.starts_with('-') => {
                eprintln!("unknown option `{other}`\n{USAGE}");
                return 2;
            }
            other => paths.push(other),
        }
    }
    if paths.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }
    let (mut failed, mut errors_left, mut fixed_files) = (false, 0, 0);
    for path in crate::files::expand(&paths, &["reject"]) {
        let shown = path.to_string_lossy().replace('\\', "/");
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot read `{shown}`: {e}");
                failed = true;
                continue;
            }
        };
        let mut out = fix_text(&path, &src);
        errors_left += out.errors_left;
        if out.applied.is_empty() {
            continue;
        }
        if format_source(&src).is_ok_and(|f| f == src) {
            if let Ok(f) = format_source(&out.text) {
                out.text = f;
            }
        }
        fixed_files += 1;
        for a in &out.applied {
            println!("{shown}:{}:{}: {} [{}]", a.line, a.col, a.message, a.code);
        }
        if !dry_run {
            if let Err(e) = std::fs::write(&path, &out.text) {
                eprintln!("cannot write `{shown}`: {e}");
                failed = true;
            }
        }
    }
    let verb = if dry_run { "would fix" } else { "fixed" };
    eprintln!("{verb} {fixed_files} file{}", if fixed_files == 1 { "" } else { "s" });
    if errors_left > 0 {
        eprintln!("{errors_left} error{} left; see `catx check`", if errors_left == 1 { " is" } else { "s are" });
    }
    if failed {
        2
    } else if errors_left > 0 {
        1
    } else {
        0
    }
}

/// Applies fixes to `src` (the text of the file at `path`) round by round.
fn fix_text(path: &Path, src: &str) -> Outcome {
    let entry = PRELUDE.len() as u32;
    let mut text = src.to_string();
    let mut applied = Vec::new();
    for round in 0..=MAX_ROUNDS {
        let comp = compile_source(path, &text, &Options { check_only: true });
        let errors = comp.diags.iter().filter(|d| d.is_error() && d.span.file == entry).count();
        if round == MAX_ROUNDS {
            return Outcome { text, applied, errors_left: errors };
        }
        let mut edits: Vec<Edit> = Vec::new();
        let sf = SourceFile::new(String::new(), text.clone());
        for d in &comp.diags {
            let Some(fix_edits) = applicable_edits(d, entry) else { continue };
            if fix_edits.iter().any(|e| edits.iter().any(|o| overlaps(e, o))) {
                continue;
            }
            let fix = d.fixes.iter().find(|f| f.applicable).unwrap();
            let (line, col) = sf.line_col(d.span.start);
            applied.push(Applied { line, col, code: d.code, message: fix.message.clone() });
            edits.extend(fix_edits.iter().cloned());
        }
        if edits.is_empty() {
            return Outcome { text, applied, errors_left: errors };
        }
        edits.sort_by_key(|e| std::cmp::Reverse((e.span.start, e.span.end)));
        for e in &edits {
            text.replace_range(e.span.start as usize..e.span.end as usize, &e.replacement);
        }
    }
    unreachable!()
}

/// The edits of a diagnostic's first applicable fix, if it only touches the entry file.
fn applicable_edits(d: &Diagnostic, entry: u32) -> Option<&[Edit]> {
    if d.span.file != entry {
        return None;
    }
    let fix = d.fixes.iter().find(|f| f.applicable)?;
    if fix.edits.is_empty() || fix.edits.iter().any(|e| e.span.file != entry || e.span.start > e.span.end) {
        return None;
    }
    Some(&fix.edits)
}

/// Whether two edits touch (adjacent or same-point edits count, so their order cannot matter).
fn overlaps(a: &Edit, b: &Edit) -> bool {
    a.span.start <= b.span.end && b.span.start <= a.span.end
}
