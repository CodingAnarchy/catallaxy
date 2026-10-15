//! Requests that work from a document's text or symbol index alone: folding ranges, semantic
//! tokens, range formatting (a line diff of the formatted document) and completion of import
//! paths.

use std::path::{Path, PathBuf};

use catallaxy_compiler::ast::{Block, ExprKind, Item};
use catallaxy_compiler::check::{Checker, Owner};
use catallaxy_compiler::diag::{SourceFile, SourceMap};
use catallaxy_compiler::symbols::{self, Index, Symbol};
use catallaxy_compiler::{lexer, parser, query};
use lsp_types as lt;

// ---- folding ranges ------------------------------------------------------------------------

/// Foldable regions of `text`: declarations, multi-line blocks and literals, runs of comment
/// lines and runs of imports. A region ending in a closing bracket on its own line folds up to
/// the line before it, so the bracket stays visible.
pub fn folding_ranges(text: &str) -> Vec<lt::FoldingRange> {
    let sf = SourceFile::new(String::new(), text.to_string());
    let line = |off: u32| sf.lsp_pos(off).0;
    let mut out: Vec<lt::FoldingRange> = Vec::new();
    let mut add = |start: u32, end: u32, kind: Option<lt::FoldingRangeKind>| {
        let (a, mut b) = (line(start), line(end));
        // `}` (or `]`, `)`) closing the region at the start of its line stays visible.
        let close = end.saturating_sub(1) as usize;
        if kind.is_none() && text.as_bytes().get(close).is_some_and(|c| b"}])".contains(c)) {
            let line_start = text[..close].rfind('\n').map_or(0, |i| i + 1);
            if text[line_start..close].trim().is_empty() && b > a {
                b -= 1;
            }
        }
        if b > a {
            out.push(lt::FoldingRange {
                start_line: a,
                start_character: None,
                end_line: b,
                end_character: None,
                kind,
                collapsed_text: None,
            });
        }
    };
    let mut next_id = 0;
    let (m, _) = parser::parse(text, 0, &mut next_id);
    let mut imports: Option<(u32, u32)> = None;
    let flush = |imports: &mut Option<(u32, u32)>, add: &mut dyn FnMut(u32, u32, Option<lt::FoldingRangeKind>)| {
        if let Some((a, b)) = imports.take() {
            add(a, b, Some(lt::FoldingRangeKind::Imports));
        }
    };
    for it in &m.items {
        if let Item::Import(i) = it {
            imports = Some(match imports {
                Some((a, _)) => (a, i.span.end),
                None => (i.span.start, i.span.end),
            });
            continue;
        }
        flush(&mut imports, &mut add);
        match it {
            Item::Fn(f) => add(f.span.start, f.span.end, None),
            Item::Class(c) => {
                add(c.span.start, c.span.end, None);
                c.methods.iter().for_each(|f| add(f.span.start, f.span.end, None));
            }
            Item::Interface(x) => {
                add(x.span.start, x.span.end, None);
                x.methods.iter().for_each(|f| add(f.span.start, f.span.end, None));
            }
            Item::Enum(e) => {
                add(e.span.start, e.span.end, None);
                e.methods.iter().for_each(|f| add(f.span.start, f.span.end, None));
            }
            Item::Extend(x) => {
                add(x.span.start, x.span.end, None);
                x.methods.iter().for_each(|f| add(f.span.start, f.span.end, None));
            }
            _ => {}
        }
    }
    flush(&mut imports, &mut add);
    let block =
        |b: &Block, add: &mut dyn FnMut(u32, u32, Option<lt::FoldingRangeKind>)| add(b.span.start, b.span.end, None);
    query::walk(&m, &mut |e| match &e.kind {
        ExprKind::If { then, .. } | ExprKind::IfLet { then, .. } => block(then, &mut add),
        ExprKind::While { body, .. }
        | ExprKind::For { body, .. }
        | ExprKind::Loop { body }
        | ExprKind::Block(body)
        | ExprKind::WithLock { body, .. }
        | ExprKind::Spawn { body, .. } => block(body, &mut add),
        ExprKind::Closure { .. } | ExprKind::Match { .. } | ExprKind::List(_) | ExprKind::Dict(_) => {
            add(e.span.start, e.span.end, None)
        }
        _ => {}
    });
    // Runs of two or more whole-line comments.
    let (_, _, comments) = lexer::lex_with_comments(text, 0);
    let mut run: Option<(u32, u32, u32)> = None; // (first line, last line, start offset)
    let mut runs = Vec::new();
    for c in &comments {
        let ls = text[..c.span.start as usize].rfind('\n').map_or(0, |i| i + 1);
        if !text[ls..c.span.start as usize].trim().is_empty() {
            continue;
        }
        let l = line(c.span.start);
        run = match run {
            Some((a, b, s)) if b + 1 == l => Some((a, l, s)),
            other => {
                runs.extend(other);
                Some((l, l, c.span.start))
            }
        };
    }
    runs.extend(run);
    for (a, b, _) in runs {
        if b > a {
            out.push(lt::FoldingRange {
                start_line: a,
                start_character: None,
                end_line: b,
                end_character: None,
                kind: Some(lt::FoldingRangeKind::Comment),
                collapsed_text: None,
            });
        }
    }
    out.sort_by_key(|r| (r.start_line, std::cmp::Reverse(r.end_line)));
    out.dedup_by(|b, a| a.start_line == b.start_line && a.end_line == b.end_line);
    out
}

// ---- semantic tokens -----------------------------------------------------------------------

/// The token types of the legend, in order.
pub const TOKEN_TYPES: &[lt::SemanticTokenType] = &[
    lt::SemanticTokenType::NAMESPACE,
    lt::SemanticTokenType::TYPE,
    lt::SemanticTokenType::CLASS,
    lt::SemanticTokenType::ENUM,
    lt::SemanticTokenType::INTERFACE,
    lt::SemanticTokenType::TYPE_PARAMETER,
    lt::SemanticTokenType::PARAMETER,
    lt::SemanticTokenType::VARIABLE,
    lt::SemanticTokenType::PROPERTY,
    lt::SemanticTokenType::ENUM_MEMBER,
    lt::SemanticTokenType::FUNCTION,
    lt::SemanticTokenType::METHOD,
];

/// The token modifiers of the legend, in order (bit `i` is modifier `i`).
pub const TOKEN_MODIFIERS: &[lt::SemanticTokenModifier] = &[
    lt::SemanticTokenModifier::DECLARATION,
    lt::SemanticTokenModifier::READONLY,
    lt::SemanticTokenModifier::DEFAULT_LIBRARY,
    lt::SemanticTokenModifier::STATIC,
];

const DECLARATION: u32 = 1;
const READONLY: u32 = 2;
const DEFAULT_LIBRARY: u32 = 4;
const STATIC: u32 = 8;

pub fn legend() -> lt::SemanticTokensLegend {
    lt::SemanticTokensLegend { token_types: TOKEN_TYPES.to_vec(), token_modifiers: TOKEN_MODIFIERS.to_vec() }
}

fn type_index(t: lt::SemanticTokenType) -> u32 {
    TOKEN_TYPES.iter().position(|x| *x == t).unwrap_or(0) as u32
}

/// The token type and modifiers of a symbol.
fn classify(ck: &Checker, sym: Symbol) -> Option<(u32, u32)> {
    use lt::SemanticTokenType as T;
    let from_prelude = |m: u32| ck.mods.get(m as usize).is_some_and(|m| m.is_prelude);
    Some(match sym {
        Symbol::Local(sp) => {
            let param = ck.locals.iter().any(|l| l.span == sp && l.is_param);
            (type_index(if param { T::PARAMETER } else { T::VARIABLE }), 0)
        }
        Symbol::Fn(f) => {
            let fi = ck.fns.get(f as usize)?;
            let lib = if from_prelude(fi.module) { DEFAULT_LIBRARY } else { 0 };
            match fi.owner {
                Owner::None => (type_index(T::FUNCTION), lib),
                _ if fi.self_kind == catallaxy_compiler::ast::SelfKind::None => (type_index(T::METHOD), lib | STATIC),
                _ => (type_index(T::METHOD), lib),
            }
        }
        Symbol::Class(c) => {
            let ci = ck.classes.get(&c)?;
            let lib = if ci.is_builtin || from_prelude(ci.module) { DEFAULT_LIBRARY } else { 0 };
            (type_index(T::CLASS), lib)
        }
        Symbol::Enum(e) => {
            (type_index(T::ENUM), if from_prelude(ck.enums.get(e as usize)?.module) { DEFAULT_LIBRARY } else { 0 })
        }
        Symbol::Iface(i) => (
            type_index(T::INTERFACE),
            if from_prelude(ck.ifaces.get(i as usize)?.module) { DEFAULT_LIBRARY } else { 0 },
        ),
        Symbol::Variant(..) => (type_index(T::ENUM_MEMBER), 0),
        Symbol::Field(..) => (type_index(T::PROPERTY), 0),
        Symbol::Global(g) => {
            let gi = ck.globals.get(g as usize)?;
            let lib = if from_prelude(gi.module) { DEFAULT_LIBRARY } else { 0 };
            (type_index(T::VARIABLE), lib | if gi.is_const { READONLY } else { 0 })
        }
    })
}

/// The semantic tokens of source file `file`: every name the symbol index knows, classified by
/// what it refers to, encoded relative to the previous token.
pub fn semantic_tokens(ck: &Checker, sm: &SourceMap, index: &Index, file: u32) -> Vec<lt::SemanticToken> {
    let Some(sf) = sm.files.get(file as usize) else { return Vec::new() };
    let mut toks: Vec<(u32, u32, u32, u32, u32)> = Vec::new(); // line, col, len, type, mods
    let mut last_end = 0u32;
    for o in index.occs.iter().filter(|o| o.span.file == file) {
        // One token per span (several symbols can share one, as a named argument and its field).
        if o.span.start < last_end {
            continue;
        }
        let Some((ty, mut mods)) = classify(ck, o.sym) else { continue };
        if o.is_decl {
            mods |= DECLARATION;
        }
        let (l1, c1) = sf.lsp_pos(o.span.start);
        let (l2, c2) = sf.lsp_pos(o.span.end);
        if l1 != l2 || c2 <= c1 {
            continue;
        }
        toks.push((l1, c1, c2 - c1, ty, mods));
        last_end = o.span.end;
    }
    let (mut pl, mut pc) = (0, 0);
    toks.into_iter()
        .map(|(l, c, len, ty, mods)| {
            let dl = l - pl;
            let dc = if dl == 0 { c - pc } else { c };
            (pl, pc) = (l, c);
            lt::SemanticToken {
                delta_line: dl,
                delta_start: dc,
                length: len,
                token_type: ty,
                token_modifiers_bitset: mods,
            }
        })
        .collect()
}

// ---- range formatting ----------------------------------------------------------------------

/// The changed regions between two texts, by lines: `(first old line, old lines replaced, new
/// lines)`, from a shortest edit script (Myers).
fn line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(usize, usize, Vec<&'a str>)> {
    // Common prefix and suffix first: formatting usually touches little.
    let mut pre = 0;
    while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < old.len() - pre && suf < new.len() - pre && old[old.len() - 1 - suf] == new[new.len() - 1 - suf] {
        suf += 1;
    }
    let (a, b) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'outer: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[(k - 1 + off) as usize] < v[(k + 1 + off) as usize]) {
                v[(k + 1 + off) as usize]
            } else {
                v[(k - 1 + off) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(k + off) as usize] = x;
            if x >= n && y >= m {
                break 'outer;
            }
            k += 2;
        }
    }
    // Walk back to the edit script: (is_insert, old index, new index).
    let (mut x, mut y) = (n, m);
    let mut ops: Vec<(bool, isize, isize)> = Vec::new();
    for d in (1..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k =
            if k == -d || (k != d && v[(k - 1 + off) as usize] < v[(k + 1 + off) as usize]) { k + 1 } else { k - 1 };
        let prev_x = v[(prev_k + off) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
        }
        if x == prev_x {
            ops.push((true, x, prev_y));
        } else {
            ops.push((false, prev_x, y));
        }
        (x, y) = (prev_x, prev_y);
    }
    ops.reverse();
    // Group adjacent operations into hunks.
    let mut hunks: Vec<(usize, usize, Vec<&'a str>)> = Vec::new();
    for (ins, ox, ny) in ops {
        let (ox, ny) = (ox as usize, ny as usize);
        match hunks.last_mut() {
            Some((start, len, lines)) if *start + *len == ox + pre => {
                if ins {
                    lines.push(b[ny]);
                } else {
                    *len += 1;
                }
                continue;
            }
            _ => {}
        }
        if ins {
            hunks.push((ox + pre, 0, vec![b[ny]]));
        } else {
            hunks.push((ox + pre, 1, Vec::new()));
        }
    }
    hunks
}

/// Edits that turn `text` into `formatted` on the lines that touch `lo..=hi` only (lines of
/// `text`, zero-based).
pub fn range_edits(text: &str, formatted: &str, lo: u32, hi: u32) -> Vec<lt::TextEdit> {
    let split = |t: &str| -> Vec<String> { t.split_inclusive('\n').map(str::to_string).collect() };
    let (old, new) = (split(text), split(formatted));
    let old_r: Vec<&str> = old.iter().map(String::as_str).collect();
    let new_r: Vec<&str> = new.iter().map(String::as_str).collect();
    let (lo, hi) = (lo as usize, hi as usize);
    line_diff(&old_r, &new_r)
        .into_iter()
        .filter(
            |(start, len, _)| {
                if *len == 0 {
                    lo <= *start && *start <= hi + 1
                } else {
                    *start <= hi && lo < start + len
                }
            },
        )
        .map(|(start, len, lines)| {
            let pos = |l: usize| -> lt::Position {
                if l < old.len() {
                    lt::Position::new(l as u32, 0)
                } else {
                    // Past the last line break: the end of the text.
                    let last = old.last().map_or("", String::as_str);
                    if last.ends_with('\n') {
                        lt::Position::new(old.len() as u32, 0)
                    } else {
                        lt::Position::new(old.len().saturating_sub(1) as u32, last.encode_utf16().count() as u32)
                    }
                }
            };
            lt::TextEdit { range: lt::Range::new(pos(start), pos(start + len)), new_text: lines.concat() }
        })
        .collect()
}

// ---- import completion ---------------------------------------------------------------------

/// What an `import` line is completing at the cursor: a module path, or the names in braces.
pub enum ImportContext {
    /// The path segments before the one being typed.
    Path(Vec<String>),
    /// Items of the module with this dotted path.
    Items(String),
}

/// The import being typed on the line before the cursor, if any.
pub fn import_context(line_before: &str) -> Option<ImportContext> {
    let rest = line_before.trim_start().strip_prefix("import")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    if let Some((path, items)) = rest.split_once(".{") {
        if items.contains('}') {
            return None;
        }
        return Some(ImportContext::Items(path.trim().to_string()));
    }
    if !rest.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
        return None;
    }
    let mut segs: Vec<String> = rest.split('.').map(str::to_string).collect();
    segs.pop();
    Some(ImportContext::Path(segs))
}

/// Modules and directories importable from `dir` under the path `segs`: `.cx` files (by stem)
/// and directories, skipping hidden ones and `exclude` (the importing file).
pub fn module_candidates(dir: &Path, segs: &[String], exclude: &Path) -> Vec<(String, bool)> {
    let base: PathBuf = segs.iter().fold(dir.to_path_buf(), |p, s| p.join(s));
    let Ok(rd) = std::fs::read_dir(&base) else { return Vec::new() };
    let mut out: Vec<(String, bool)> = Vec::new();
    for e in rd.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            if symbols::is_identifier(&name) {
                out.push((name, true));
            }
        } else if let Some(stem) = name.strip_suffix(".cx") {
            if symbols::is_identifier(stem) && p != exclude {
                out.push((stem.to_string(), false));
            }
        }
    }
    out.sort();
    out.dedup_by(|b, a| a.0 == b.0);
    out
}

/// The public names a module's source declares, with their kinds.
pub fn module_items(src: &str) -> Vec<(String, query::CompletionKind)> {
    use query::CompletionKind as K;
    let mut next_id = 0;
    let (m, _) = parser::parse(src, 0, &mut next_id);
    let mut out = Vec::new();
    for it in &m.items {
        let (name, pub_, kind) = match it {
            Item::Fn(f) => (&f.name.name, f.is_pub, K::Function),
            Item::Class(c) => (&c.name.name, c.is_pub, K::Class),
            Item::Interface(x) => (&x.name.name, x.is_pub, K::Interface),
            Item::Enum(e) => (&e.name.name, e.is_pub, K::Enum),
            Item::Const(k) => (&k.name.name, k.is_pub, K::Constant),
            Item::Var(v) => (&v.name.name, v.is_pub, K::Variable),
            _ => continue,
        };
        if pub_ {
            out.push((name.clone(), kind));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edits: &[lt::TextEdit]) -> String {
        let sf = SourceFile::new(String::new(), text.to_string());
        let mut out = text.to_string();
        let mut es: Vec<_> = edits.iter().collect();
        es.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
        for e in es {
            let a = sf.offset_of(e.range.start.line, e.range.start.character) as usize;
            let b = sf.offset_of(e.range.end.line, e.range.end.character) as usize;
            out.replace_range(a..b, &e.new_text);
        }
        out
    }

    #[test]
    fn range_edits_touch_only_the_range() {
        let old = "a\nB\nc\nD\ne\n";
        let new = "a\nb\nc\nd\ne\n";
        assert_eq!(apply(old, &range_edits(old, new, 0, 4)), new);
        assert_eq!(apply(old, &range_edits(old, new, 1, 1)), "a\nb\nc\nD\ne\n");
        assert_eq!(apply(old, &range_edits(old, new, 3, 3)), "a\nB\nc\nd\ne\n");
        // Insertions and deletions, and a text without a final newline.
        for (old, new) in [("a\nb", "a\nx\nb\n"), ("x\na\ny\nb\n", "a\nb\n"), ("", "a\n"), ("a\n", "")] {
            assert_eq!(apply(old, &range_edits(old, new, 0, 10)), new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn import_contexts() {
        assert!(matches!(import_context("import lib."), Some(ImportContext::Path(s)) if s == ["lib"]));
        assert!(matches!(import_context("  import "), Some(ImportContext::Path(s)) if s.is_empty()));
        assert!(
            matches!(import_context("import lib.greeting.{hel"), Some(ImportContext::Items(p)) if p == "lib.greeting")
        );
        assert!(import_context("important = 1").is_none());
        assert!(import_context("import lib.{a} ").is_none());
    }

    #[test]
    fn folds() {
        let text = "import a\nimport b\n# one\n# two\nfn f() {\n  if x {\n    1\n  }\n}\n";
        let r: Vec<(u32, u32)> = folding_ranges(text).iter().map(|r| (r.start_line, r.end_line)).collect();
        assert_eq!(r, vec![(0, 1), (2, 3), (4, 7), (5, 6)]);
    }
}
