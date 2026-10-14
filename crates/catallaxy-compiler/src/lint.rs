//! Lints: warnings about code that is correct but likely slow, computed from a checked program,
//! and `# allow(CODE)` comments that silence warnings.
//!
//! W0004: a `String` local declared outside a loop and grown with `s += x` (or `s = s + x`) in the
//! loop copies the whole string on every iteration. A `StringBuilder` appends in place.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{BinOp, Block, Expr, ExprKind, Item, Module, StrPart};
use crate::check::{Checker, LocalId, NameRes};
use crate::diag::{Diagnostic, Edit, SourceMap, Span};
use crate::lexer;
use crate::query::{children, roots};

/// Loops of at most this many iterations, with literal bounds, are not worth a builder.
const SMALL_LOOP: i64 = 16;

/// Block-taking methods whose block runs once per element (or per count).
fn is_iteration_method(name: &str) -> bool {
    name.starts_with("each")
        || matches!(
            name,
            "times" | "upto" | "downto" | "map" | "flat_map" | "filter" | "filter_map" | "reject" | "select"
        )
}

/// The lints over every user module of a checked program.
pub fn run(ck: &Checker, sm: &SourceMap) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for m in &ck.mods {
        if m.is_prelude {
            continue;
        }
        let text = &sm.file(m.file).text;
        StringAppends::new(ck, &m.ast, text).run(&mut out);
    }
    out
}

/// A loop enclosing the expression being visited.
#[derive(Clone, Copy)]
struct Frame<'a> {
    /// The whole loop: the `for`/`while`/`loop` expression or the call taking the block.
    expr: &'a Expr,
    /// A fixed, small number of iterations.
    small: bool,
}

/// One `s += x` / `s = s + x`.
struct Append<'a> {
    assign: &'a Expr,
    rhs: &'a Expr,
}

struct StringAppends<'a> {
    ck: &'a Checker,
    module: &'a Module,
    text: &'a str,
    /// Loop expression id → appends to each local in it (innermost non-small loop only).
    found: FxHashMap<(u32, LocalId), (&'a Expr, Vec<Append<'a>>)>,
    order: Vec<(u32, LocalId)>,
    /// Every read or write of a local: (identifier node id, span, from a closure that captures it).
    refs: FxHashMap<LocalId, Vec<(u32, Span, bool)>>,
    /// Expressions that are statements of a block: (the block's last, the statement before).
    stmts: FxHashMap<u32, (bool, Option<&'a Expr>)>,
    /// The `s` of `s = s + x`: a read that belongs to the append.
    append_reads: FxHashSet<u32>,
}

impl<'a> StringAppends<'a> {
    fn new(ck: &'a Checker, module: &'a Module, text: &'a str) -> Self {
        StringAppends {
            ck,
            module,
            text,
            found: FxHashMap::default(),
            order: Vec::new(),
            refs: FxHashMap::default(),
            stmts: FxHashMap::default(),
            append_reads: FxHashSet::default(),
        }
    }

    fn run(mut self, out: &mut Vec<Diagnostic>) {
        let mut prev_top: Option<&'a Expr> = None;
        for it in &self.module.items {
            if !matches!(it, Item::Stmt(_)) {
                prev_top = None;
            }
            match it {
                Item::Fn(f) => self.fn_body(f.body.as_ref()),
                Item::Class(c) => c.methods.iter().for_each(|f| self.fn_body(f.body.as_ref())),
                Item::Enum(e) => e.methods.iter().for_each(|f| self.fn_body(f.body.as_ref())),
                Item::Extend(x) => x.methods.iter().for_each(|f| self.fn_body(f.body.as_ref())),
                Item::Interface(i) => i.methods.iter().for_each(|f| self.fn_body(f.body.as_ref())),
                // Top-level statements: none of them is a value.
                Item::Stmt(e) => {
                    self.stmts.insert(e.id, (false, prev_top));
                    prev_top = Some(e);
                }
                _ => {}
            }
        }
        for r in roots(self.module) {
            self.visit(r, &mut Vec::new(), None);
        }
        let order = std::mem::take(&mut self.order);
        for key in order {
            let (lp, appends) = self.found.remove(&key).unwrap();
            out.push(self.diagnostic(key.1, lp, &appends));
        }
    }

    fn fn_body(&mut self, b: Option<&'a Block>) {
        if let Some(b) = b {
            self.block(b);
        }
    }

    fn block(&mut self, b: &'a Block) {
        let n = b.stmts.len();
        for (i, s) in b.stmts.iter().enumerate() {
            self.stmts.insert(s.id, (i + 1 == n, i.checked_sub(1).map(|p| &b.stmts[p])));
        }
    }

    fn local_of(&self, e: &Expr) -> Option<LocalId> {
        match (&e.kind, self.ck.names.get(&e.id)) {
            (ExprKind::Ident(_), Some(NameRes::Local(l))) => Some(*l),
            _ => None,
        }
    }

    fn is_string(&self, t: Option<&u32>) -> bool {
        t.is_some_and(|&t| self.ck.types.shallow_resolve(t) == self.ck.types.string)
    }

    /// `s += x` or `s = s + x` on a `String` local, with a `String` `x`.
    fn append(&mut self, e: &'a Expr) -> Option<(LocalId, Append<'a>)> {
        let ExprKind::Assign { target, op, value, ty: None } = &e.kind else { return None };
        let l = self.local_of(target)?;
        let rhs = match op {
            Some(BinOp::Add) => value.as_ref(),
            None => match &value.kind {
                ExprKind::Binary(BinOp::Add, a, b) if self.local_of(a) == Some(l) => {
                    self.append_reads.insert(a.id);
                    b.as_ref()
                }
                _ => return None,
            },
            _ => return None,
        };
        let info = &self.ck.locals[l as usize];
        if !self.is_string(Some(&info.ty)) || !self.is_string(self.ck.expr_ty.get(&rhs.id)) {
            return None;
        }
        Some((l, Append { assign: e, rhs }))
    }

    /// Visits `e` inside `loops`; `closure` is the span of the innermost enclosing closure.
    fn visit(&mut self, e: &'a Expr, loops: &mut Vec<Frame<'a>>, closure: Option<Span>) {
        // Blocks: remember which expressions are statements.
        match &e.kind {
            ExprKind::Closure { body, .. }
            | ExprKind::If { then: body, .. }
            | ExprKind::IfLet { then: body, .. }
            | ExprKind::While { body, .. }
            | ExprKind::Loop { body }
            | ExprKind::Block(body)
            | ExprKind::Spawn { body, .. }
            | ExprKind::For { body, .. }
            | ExprKind::WithLock { body, .. } => self.block(body),
            _ => {}
        }
        if let Some(l) = self.local_of(e) {
            let decl = self.ck.locals[l as usize].span;
            let captured = closure.is_some_and(|c| !contains(c, decl));
            self.refs.entry(l).or_default().push((e.id, e.span, captured));
        }
        if let Some((l, app)) = self.append(e) {
            let decl = self.ck.locals[l as usize].span;
            // The innermost loop that the local is declared outside of and that runs more than a
            // few times.
            let found = loops.iter().rev().find(|f| !contains(f.expr.span, decl) && !f.small);
            if let Some(f) = found {
                let key = (f.expr.id, l);
                if !self.found.contains_key(&key) {
                    self.order.push(key);
                }
                self.found.entry(key).or_insert((f.expr, Vec::new())).1.push(app);
            }
        }
        match &e.kind {
            ExprKind::For { iter, body, .. } => {
                // The iterable is evaluated once, before the loop.
                self.visit(iter, loops, closure);
                loops.push(Frame { expr: e, small: small_range(iter) });
                for s in &body.stmts {
                    self.visit(s, loops, closure);
                }
                loops.pop();
            }
            ExprKind::While { .. } | ExprKind::Loop { .. } => {
                loops.push(Frame { expr: e, small: false });
                for c in children(e) {
                    self.visit(c, loops, closure);
                }
                loops.pop();
            }
            ExprKind::Member { recv, name, args, block: Some(b), .. } if is_iteration_method(&name.name) => {
                let small = match name.name.as_str() {
                    "times" => int_lit(recv).is_some_and(|n| n <= SMALL_LOOP),
                    "upto" | "downto" => match (int_lit(recv), args.first().and_then(|a| int_lit(&a.value))) {
                        (Some(a), Some(b)) => (b - a).abs() < SMALL_LOOP,
                        _ => false,
                    },
                    _ => small_range(recv),
                };
                // The receiver and arguments run once; the block runs per iteration.
                self.visit(recv, loops, closure);
                for a in args {
                    self.visit(&a.value, loops, closure);
                }
                loops.push(Frame { expr: e, small });
                match &b.kind {
                    ExprKind::Closure { body, .. } => {
                        self.block(body);
                        for s in &body.stmts {
                            self.visit(s, loops, Some(b.span));
                        }
                    }
                    _ => self.visit(b, loops, closure),
                }
                loops.pop();
            }
            // Any other closure may run any number of times, or never: appends inside it are not
            // attributed to the loops around it.
            ExprKind::Closure { .. } | ExprKind::Spawn { .. } => {
                let mut inner = Vec::new();
                for c in children(e) {
                    self.visit(c, &mut inner, Some(e.span));
                }
            }
            _ => {
                for c in children(e) {
                    self.visit(c, loops, closure);
                }
            }
        }
    }

    fn diagnostic(&self, l: LocalId, lp: &'a Expr, appends: &[Append<'a>]) -> Diagnostic {
        let name = &self.ck.locals[l as usize].name;
        let first = appends[0].assign;
        let msg = format!("string `{name}` is appended to in a loop, which copies the whole string every time");
        let mut d = Diagnostic::warning("W0004", msg, first.span)
            .note_at("the loop", Span::new(lp.span.file, lp.span.start, lp.span.start))
            .note(format!(
                "use a StringBuilder: `sb = StringBuilder.new()`, `sb.push(x)` in the loop, then `{name} = sb.to_s`"
            ))
            .note("silence this warning with a `# allow(W0004)` comment on the line or the line above");
        if let Some(edits) = self.fix(l, lp, appends) {
            d = d.fix_edits("use a StringBuilder", edits);
        }
        d
    }

    /// The rewrite to a builder, when it is mechanical: the loop is a statement on its own lines,
    /// the local is touched in it only by the appends, nothing can observe it mid-loop, and the
    /// loop cannot leave the function early.
    fn fix(&self, l: LocalId, lp: &'a Expr, appends: &[Append<'a>]) -> Option<Vec<Edit>> {
        let name = &self.ck.locals[l as usize].name;
        let (is_last, prev) = *self.stmts.get(&lp.id)?;
        let value_loop = !matches!(lp.kind, ExprKind::For { .. } | ExprKind::While { .. });
        if is_last && value_loop {
            return None;
        }
        let appended: FxHashSet<u32> = appends
            .iter()
            .map(|a| match &a.assign.kind {
                ExprKind::Assign { target, .. } => target.id,
                _ => unreachable!(),
            })
            .collect();
        for &(id, span, captured) in self.refs.get(&l).map(Vec::as_slice).unwrap_or(&[]) {
            if contains(lp.span, span) {
                if !appended.contains(&id) && !self.append_reads.contains(&id) {
                    return None;
                }
            } else if captured {
                // A closure elsewhere could read it while the loop runs.
                return None;
            }
        }
        if appends.iter().any(|a| mentions(a.rhs, &|e| self.local_of(e) == Some(l))) {
            return None;
        }
        if mentions(lp, &|e| matches!(e.kind, ExprKind::Return(_) | ExprKind::Try(_))) {
            return None;
        }
        // The loop starts its line and ends it (a trailing comment may follow).
        let text = self.text;
        let start = lp.span.start as usize;
        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
        let indent = &text[line_start..start];
        if !indent.chars().all(|c| c == ' ' || c == '\t') {
            return None;
        }
        let end = lp.span.end as usize;
        let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
        let rest = text[end..line_end].trim_start();
        if !(rest.is_empty() || rest.starts_with('#')) {
            return None;
        }
        let line_end = if text[..line_end].ends_with('\r') { line_end - 1 } else { line_end };
        let sb = self.fresh_name(&format!("{name}_sb"));
        let file = lp.span.file;
        // `s = ""` just before the loop becomes the builder; otherwise the builder starts with `s`.
        let mut edits = match prev {
            Some(p) if self.is_empty_init(p, l) => {
                vec![Edit { span: p.span, replacement: format!("{sb} = StringBuilder.new()") }]
            }
            _ => vec![Edit {
                span: Span::new(file, lp.span.start, lp.span.start),
                replacement: format!("{sb} = StringBuilder.new()\n{indent}{sb}.push({name})\n{indent}"),
            }],
        };
        for a in appends {
            let rhs = &text[a.rhs.span.start as usize..a.rhs.span.end as usize];
            edits.push(Edit { span: a.assign.span, replacement: format!("{sb}.push({rhs})") });
        }
        edits.push(Edit {
            span: Span::new(file, line_end as u32, line_end as u32),
            replacement: format!("\n{indent}{name} = {sb}.to_s"),
        });
        Some(edits)
    }

    /// `s = ""` (no type annotation) for the local `l`.
    fn is_empty_init(&self, e: &Expr, l: LocalId) -> bool {
        match &e.kind {
            ExprKind::Assign { target, op: None, value, ty: None } => {
                self.local_of(target) == Some(l)
                    && matches!(&value.kind, ExprKind::Str(parts) if parts.iter().all(|p| matches!(p, StrPart::Lit(t) if t.is_empty())))
            }
            _ => false,
        }
    }

    /// `base`, or `base2`, `base3`, ... : a name not used anywhere in the file.
    fn fresh_name(&self, base: &str) -> String {
        let used = |n: &str| {
            self.text.match_indices(n).any(|(i, _)| {
                let before = self.text[..i].chars().next_back();
                let after = self.text[i + n.len()..].chars().next();
                !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
            })
        };
        if !used(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base}{i}")).find(|n| !used(n)).unwrap()
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.file == inner.file && outer.start <= inner.start && inner.end <= outer.end
}

/// Whether `e` or any sub-expression satisfies `p`.
fn mentions(e: &Expr, p: &dyn Fn(&Expr) -> bool) -> bool {
    p(e) || children(e).into_iter().any(|c| mentions(c, p))
}

fn int_lit(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(s) => s.replace('_', "").parse().ok(),
        ExprKind::Unary(crate::ast::UnOp::Neg, x) => int_lit(x).map(|n| -n),
        _ => None,
    }
}

/// A range literal with literal bounds and at most [`SMALL_LOOP`] elements.
fn small_range(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Range { lo, hi, exclusive } => match (int_lit(lo), int_lit(hi)) {
            (Some(a), Some(b)) => b - a + i64::from(!*exclusive) <= SMALL_LOOP,
            _ => false,
        },
        _ => false,
    }
}

/// Drops warnings silenced by a `# allow(CODE, ...)` comment on their line or the line above.
pub fn apply_allows(diags: &mut Vec<Diagnostic>, sm: &SourceMap) {
    let mut allows: FxHashMap<u32, FxHashMap<u32, Vec<String>>> = FxHashMap::default();
    let files: FxHashSet<u32> = diags
        .iter()
        .filter(|d| !d.is_error() && (d.span.file as usize) < sm.files.len())
        .map(|d| d.span.file)
        .collect();
    for f in files {
        let file = sm.file(f);
        let (_, _, comments) = lexer::lex_with_comments(&file.text, f);
        let lines = allows.entry(f).or_default();
        for c in comments {
            if let Some(codes) = allow_codes(&c.text) {
                let (line, _) = file.line_col(c.span.start);
                lines.entry(line).or_default().extend(codes);
            }
        }
    }
    diags.retain(|d| {
        if d.is_error() {
            return true;
        }
        let Some(lines) = allows.get(&d.span.file) else { return true };
        let (line, _) = sm.file(d.span.file).line_col(d.span.start);
        let allowed = |l: u32| lines.get(&l).is_some_and(|cs| cs.iter().any(|c| c == d.code));
        !(allowed(line) || (line > 1 && allowed(line - 1)))
    });
}

/// The codes of an `# allow(W0004, ...)` comment.
fn allow_codes(comment: &str) -> Option<Vec<String>> {
    let body = comment.trim_start_matches('#').trim_start();
    let rest = body.strip_prefix("allow(")?;
    let inner = &rest[..rest.find(')')?];
    Some(inner.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_comment_syntax() {
        assert_eq!(allow_codes("# allow(W0004)"), Some(vec!["W0004".to_string()]));
        assert_eq!(allow_codes("#allow(W0001, W0004) because"), Some(vec!["W0001".into(), "W0004".into()]));
        assert_eq!(allow_codes("# we allow(W0004) here"), None);
        assert_eq!(allow_codes("# allow W0004"), None);
    }
}
