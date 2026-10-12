//! Editor queries over a checked compilation: the expression at an offset, hover text, go to
//! definition and completion candidates. Used by the language server and the REPL.

use rustc_hash::FxHashSet;

use crate::ast::{Block, ClassDecl, Expr, ExprKind, FnDecl, Item, Module, Pattern, PatternKind, StrPart};
use crate::check::{CallRes, Checker, Dispatch, ModId, NameRes, Owner, ScopeItem};
use crate::diag::Span;
use crate::types::{Ty, TyId};

// ---- AST walking ---------------------------------------------------------------------------

fn block_exprs<'a>(b: &'a Block, out: &mut Vec<&'a Expr>) {
    out.extend(b.stmts.iter());
}

fn pattern_exprs<'a>(p: &'a Pattern, out: &mut Vec<&'a Expr>) {
    match &p.kind {
        PatternKind::Literal(e) => out.push(e),
        PatternKind::Range { lo, hi, .. } => {
            out.push(lo);
            out.push(hi);
        }
        PatternKind::Ctor { args, .. } | PatternKind::Tuple(args) | PatternKind::Or(args) => {
            for a in args {
                pattern_exprs(a, out);
            }
        }
        PatternKind::Wildcard | PatternKind::Bind(_) | PatternKind::Typed { .. } => {}
    }
}

/// The direct sub-expressions of `e`, in source order.
pub fn children(e: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Nil
        | ExprKind::Ident(_)
        | ExprKind::SelfValue
        | ExprKind::Continue
        | ExprKind::Error => {}
        ExprKind::Str(parts) => {
            for p in parts {
                if let StrPart::Expr(x) = p {
                    out.push(x);
                }
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) => out.extend(xs.iter()),
        ExprKind::Dict(kvs) => {
            for (k, v) in kvs {
                out.push(k);
                out.push(v);
            }
        }
        ExprKind::Unary(_, x)
        | ExprKind::Unwrap(x)
        | ExprKind::Try(x)
        | ExprKind::Move(x)
        | ExprKind::Is { expr: x, .. }
        | ExprKind::AsCast { expr: x, .. } => out.push(x),
        ExprKind::Binary(_, a, b) | ExprKind::Range { lo: a, hi: b, .. } => {
            out.push(a);
            out.push(b);
        }
        ExprKind::Call { callee, args, block } => {
            out.push(callee);
            out.extend(args.iter().map(|a| &a.value));
            if let Some(b) = block {
                out.push(b);
            }
        }
        ExprKind::Member { recv, args, block, .. } => {
            out.push(recv);
            out.extend(args.iter().map(|a| &a.value));
            if let Some(b) = block {
                out.push(b);
            }
        }
        ExprKind::Index { recv, args } => {
            out.push(recv);
            out.extend(args.iter());
        }
        ExprKind::Closure { body, .. } => block_exprs(body, &mut out),
        ExprKind::If { cond, then, else_ } => {
            out.push(cond);
            block_exprs(then, &mut out);
            if let Some(x) = else_ {
                out.push(x);
            }
        }
        ExprKind::IfLet { pattern, value, then, else_ } => {
            pattern_exprs(pattern, &mut out);
            out.push(value);
            block_exprs(then, &mut out);
            if let Some(x) = else_ {
                out.push(x);
            }
        }
        ExprKind::While { cond, body } => {
            out.push(cond);
            block_exprs(body, &mut out);
        }
        ExprKind::Loop { body } | ExprKind::Block(body) | ExprKind::Spawn { body, .. } => block_exprs(body, &mut out),
        ExprKind::For { pattern, iter, body } => {
            pattern_exprs(pattern, &mut out);
            out.push(iter);
            block_exprs(body, &mut out);
        }
        ExprKind::Match { subject, arms } => {
            out.push(subject);
            for a in arms {
                pattern_exprs(&a.pattern, &mut out);
                if let Some(g) = &a.guard {
                    out.push(g);
                }
                out.push(&a.body);
            }
        }
        ExprKind::Return(x) | ExprKind::Break(x) => {
            if let Some(x) = x {
                out.push(x);
            }
        }
        ExprKind::Assign { target, value, .. } => {
            out.push(target);
            out.push(value);
        }
        ExprKind::Let { pattern, value } => {
            pattern_exprs(pattern, &mut out);
            out.push(value);
        }
        ExprKind::WithLock { obj, body } => {
            out.push(obj);
            block_exprs(body, &mut out);
        }
    }
    out
}

fn fn_roots<'a>(f: &'a FnDecl, out: &mut Vec<&'a Expr>) {
    for p in &f.params {
        if let Some(d) = &p.default {
            out.push(d);
        }
    }
    if let Some(b) = &f.body {
        block_exprs(b, out);
    }
}

fn class_roots<'a>(c: &'a ClassDecl, out: &mut Vec<&'a Expr>) {
    for f in &c.fields {
        if let Some(d) = &f.default {
            out.push(d);
        }
    }
    for k in &c.consts {
        out.push(&k.value);
    }
    for m in &c.methods {
        fn_roots(m, out);
    }
}

/// Top-level expressions of a module: statements, initializers, and function bodies.
pub fn roots(m: &Module) -> Vec<&Expr> {
    let mut out = Vec::new();
    for it in &m.items {
        match it {
            Item::Fn(f) => fn_roots(f, &mut out),
            Item::Class(c) => class_roots(c, &mut out),
            Item::Interface(i) => i.methods.iter().for_each(|f| fn_roots(f, &mut out)),
            Item::Enum(e) => e.methods.iter().for_each(|f| fn_roots(f, &mut out)),
            Item::Extend(x) => x.methods.iter().for_each(|f| fn_roots(f, &mut out)),
            Item::Const(c) => out.push(&c.value),
            Item::Var(v) => out.push(&v.value),
            Item::Stmt(e) => out.push(e),
            Item::Import(_) => {}
        }
    }
    out
}

/// Visits every expression of the module in pre-order.
pub fn walk<'a>(m: &'a Module, f: &mut dyn FnMut(&'a Expr)) {
    fn go<'a>(e: &'a Expr, f: &mut dyn FnMut(&'a Expr)) {
        f(e);
        for c in children(e) {
            go(c, f);
        }
    }
    for r in roots(m) {
        go(r, f);
    }
}

fn contains(s: Span, offset: u32) -> bool {
    s.start <= offset && offset <= s.end
}

/// The innermost expression whose span contains `offset` (end inclusive, so a cursor just past
/// an identifier still finds it).
pub fn expr_at(m: &Module, offset: u32) -> Option<&Expr> {
    fn go(e: &Expr, offset: u32) -> Option<&Expr> {
        if !contains(e.span, offset) {
            return None;
        }
        for c in children(e) {
            if let Some(x) = go(c, offset) {
                return Some(x);
            }
        }
        Some(e)
    }
    roots(m).into_iter().find_map(|r| go(r, offset))
}

/// The smallest expression ending exactly at `end` (the receiver before a `.`), falling back to
/// the innermost expression containing the preceding byte.
pub fn expr_ending_at(m: &Module, end: u32) -> Option<&Expr> {
    let mut best: Option<&Expr> = None;
    walk(m, &mut |e| {
        if e.span.end == end && e.span.start < end && !matches!(e.kind, ExprKind::Error) {
            if best.is_none_or(|b| e.span.end - e.span.start < b.span.end - b.span.start) {
                best = Some(e);
            }
        }
    });
    best.or_else(|| expr_at(m, end.saturating_sub(1)))
}

/// Spans of every function-like scope (declared functions and closures) in the module.
pub fn fn_scopes(m: &Module) -> Vec<Span> {
    let mut out = Vec::new();
    let add_fn = |f: &FnDecl, out: &mut Vec<Span>| out.push(f.span);
    for it in &m.items {
        match it {
            Item::Fn(f) => add_fn(f, &mut out),
            Item::Class(c) => c.methods.iter().for_each(|f| add_fn(f, &mut out)),
            Item::Interface(i) => i.methods.iter().for_each(|f| add_fn(f, &mut out)),
            Item::Enum(e) => e.methods.iter().for_each(|f| add_fn(f, &mut out)),
            Item::Extend(x) => x.methods.iter().for_each(|f| add_fn(f, &mut out)),
            _ => {}
        }
    }
    walk(m, &mut |e| {
        if matches!(e.kind, ExprKind::Closure { .. } | ExprKind::Spawn { .. }) {
            out.push(e.span);
        }
    });
    out
}

/// The checker module whose AST lives in source file `file`.
pub fn module_of_file(ck: &Checker, file: u32) -> Option<ModId> {
    ck.mods.iter().position(|m| m.file == file && !m.is_prelude).map(|i| i as ModId)
}

/// The last top-level statement of module `m` starting at or after `from` (REPL inputs).
pub fn last_stmt_from(ck: &Checker, m: ModId, from: u32) -> Option<&Expr> {
    match ck.mods[m as usize].ast.items.last()? {
        Item::Stmt(e) if e.span.start >= from => Some(e),
        _ => None,
    }
}

// ---- rendering -----------------------------------------------------------------------------

/// `fn Owner.name[T](a: A, b: B = …) -> R`.
pub fn fn_signature(ck: &Checker, fid: u32) -> String {
    let f = &ck.fns[fid as usize];
    let owner = match f.owner {
        Owner::Class(c) => ck.classes.get(&c).map(|c| format!("{}.", c.name)).unwrap_or_default(),
        Owner::Enum(e) => format!("{}.", ck.enums[e as usize].name),
        Owner::Iface(i) => format!("{}.", ck.ifaces[i as usize].name),
        Owner::Extend(x) => format!("{}.", ck.show(ck.exts[x as usize].target)),
        Owner::None => String::new(),
    };
    let tps = if f.type_params.is_empty() {
        String::new()
    } else {
        format!("[{}]", f.type_params.iter().map(|&p| ck.params[p as usize].name.clone()).collect::<Vec<_>>().join(", "))
    };
    let mut ps: Vec<String> = Vec::new();
    match f.self_kind {
        crate::ast::SelfKind::None => {}
        crate::ast::SelfKind::Ref => ps.push("self".into()),
        crate::ast::SelfKind::Mut => ps.push("mut self".into()),
    }
    for (i, p) in f.params.iter().enumerate() {
        let ty = f.implicit.as_ref().and_then(|s| s.params.get(i).copied()).unwrap_or(p.ty);
        let mut s = format!("{}{}: {}", if p.is_move { "move " } else { "" }, p.name, ck.show(ty));
        if p.has_default {
            s.push_str(" = …");
        }
        ps.push(s);
    }
    let ret = f.implicit.as_ref().map(|s| s.ret).unwrap_or(f.ret);
    let ret = if ret == ck.types.nil { String::new() } else { format!(" -> {}", ck.show(ret)) };
    format!("fn {owner}{}{tps}({}){ret}", f.name, ps.join(", "))
}

fn with_doc(code: String, doc: &Option<String>) -> String {
    let mut s = format!("```catallaxy\n{code}\n```");
    if let Some(d) = doc {
        s.push_str("\n\n");
        s.push_str(d);
    }
    s
}

/// The function a call or member expression resolves to, if any.
fn called_fn(ck: &Checker, e: &Expr) -> Option<u32> {
    match ck.calls.get(&e.id)? {
        CallRes::Fn { dispatch, .. } => match dispatch {
            Dispatch::Static(f) | Dispatch::Virtual(f, _) => Some(*f),
            Dispatch::Dynamic(_, f) => *f,
        },
        CallRes::FnValue(f) => Some(*f),
        CallRes::Construct { ctor, .. } => Some(*ctor),
        _ => None,
    }
}

/// Markdown hover text for the expression at `offset` of module `m`.
pub fn hover(ck: &Checker, m: ModId, offset: u32) -> Option<(String, Span)> {
    let ast = &ck.mods[m as usize].ast;
    let e = expr_at(ast, offset)?;
    let ty_line = |e: &Expr| ck.expr_ty.get(&e.id).map(|&t| ck.show(t));
    match &e.kind {
        ExprKind::Ident(name) => {
            let text = match ck.names.get(&e.id) {
                Some(NameRes::Local(l)) => {
                    let li = &ck.locals[*l as usize];
                    with_doc(format!("{}: {}", li.name, ck.show(li.ty)), &None)
                }
                Some(NameRes::Fn(f)) => with_doc(fn_signature(ck, *f), &ck.fns[*f as usize].doc),
                Some(NameRes::Global(g)) => {
                    let gi = &ck.globals[*g as usize];
                    with_doc(format!("{} {}: {}", if gi.is_const { "const" } else { "var" }, gi.name, ck.show(gi.ty)), &None)
                }
                Some(NameRes::Class(c)) => {
                    let ci = ck.classes.get(c)?;
                    with_doc(format!("class {}", ci.name), &ci.doc)
                }
                Some(NameRes::Enum(x)) => {
                    let ei = &ck.enums[*x as usize];
                    with_doc(format!("enum {}", ei.name), &ei.doc)
                }
                Some(NameRes::Iface(x)) => {
                    let ii = &ck.ifaces[*x as usize];
                    with_doc(format!("interface {}", ii.name), &ii.doc)
                }
                Some(NameRes::Module(x)) => with_doc(format!("module {}", ck.mods[*x as usize].name), &None),
                Some(NameRes::Variant(x, v)) => {
                    let ei = &ck.enums[*x as usize];
                    with_doc(format!("{}.{}", ei.name, ei.variants[*v as usize].name), &None)
                }
                None => with_doc(format!("{name}: {}", ty_line(e)?), &None),
            };
            Some((text, e.span))
        }
        ExprKind::Member { name, .. } if contains(name.span, offset) => {
            let text = match called_fn(ck, e) {
                Some(f) => with_doc(fn_signature(ck, f), &ck.fns[f as usize].doc),
                None => with_doc(format!("{}: {}", name.name, ty_line(e)?), &None),
            };
            Some((text, name.span))
        }
        _ => Some((with_doc(ty_line(e)?, &None), e.span)),
    }
}

/// Where the name at `offset` of module `m` is defined.
pub fn definition(ck: &Checker, m: ModId, offset: u32) -> Option<Span> {
    let ast = &ck.mods[m as usize].ast;
    let e = expr_at(ast, offset)?;
    let span = match &e.kind {
        ExprKind::Ident(_) => match ck.names.get(&e.id)? {
            NameRes::Local(l) => ck.locals[*l as usize].span,
            NameRes::Global(g) => ck.globals[*g as usize].span,
            NameRes::Fn(f) => ck.fns[*f as usize].span,
            NameRes::Class(c) => ck.classes.get(c)?.span,
            NameRes::Enum(x) => ck.enums[*x as usize].span,
            NameRes::Iface(x) => ck.ifaces[*x as usize].span,
            NameRes::Variant(x, v) => ck.enums[*x as usize].variants[*v as usize].span,
            NameRes::Module(_) => return None,
        },
        ExprKind::Member { recv, name, .. } if contains(name.span, offset) => match ck.calls.get(&e.id)? {
            CallRes::Field { index, .. } => {
                let t = ck.types.shallow_resolve(*ck.expr_ty.get(&recv.id)?);
                let t = match ck.types.get(t) {
                    Ty::Frozen(i) => ck.types.shallow_resolve(*i),
                    _ => t,
                };
                match ck.types.get(t) {
                    Ty::Class(c, _) => ck.classes.get(c)?.fields.get(*index as usize)?.span,
                    _ => return None,
                }
            }
            CallRes::Construct { class, .. } => ck.classes.get(class)?.span,
            CallRes::Global(g) => ck.globals[*g as usize].span,
            _ => ck.fns[called_fn(ck, e)? as usize].span,
        },
        _ => return None,
    };
    if span.file == u32::MAX {
        None
    } else {
        Some(span)
    }
}

// ---- completion ----------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Method,
    Field,
    Function,
    Variable,
    Class,
    Enum,
    Interface,
    Module,
    Keyword,
    Constant,
    Variant,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
}

pub const KEYWORDS: &[&str] = &[
    "and", "as", "break", "class", "const", "continue", "else", "enum", "extend", "false", "fn", "for", "frozen",
    "if", "import", "in", "interface", "is", "let", "loop", "match", "move", "mut", "native", "nil", "not", "or",
    "pub", "return", "self", "Self", "shared", "spawn", "true", "var", "while", "with",
];

fn scope_item_completion(ck: &Checker, name: &str, it: ScopeItem) -> Completion {
    let (kind, detail) = match it {
        ScopeItem::Class(c) => (CompletionKind::Class, ck.classes.get(&c).map(|c| format!("class {}", c.name))),
        ScopeItem::Enum(e) => (CompletionKind::Enum, Some(format!("enum {}", ck.enums[e as usize].name))),
        ScopeItem::Iface(i) => (CompletionKind::Interface, Some(format!("interface {}", ck.ifaces[i as usize].name))),
        ScopeItem::Fn(f) => (CompletionKind::Function, Some(fn_signature(ck, f))),
        ScopeItem::Global(g) => (CompletionKind::Constant, Some(ck.show(ck.globals[g as usize].ty))),
        ScopeItem::Module(m) => (CompletionKind::Module, Some(format!("module {}", ck.mods[m as usize].name))),
        ScopeItem::Variant(..) => (CompletionKind::Variant, None),
    };
    Completion { label: name.to_string(), kind, detail }
}

/// Members available on a value of type `recv` in module `m`: fields, methods (inherited, from
/// extensions and interface defaults). Each candidate name is validated with the checker's own
/// method lookup.
pub fn member_completions(ck: &mut Checker, m: ModId, recv: TyId) -> Vec<Completion> {
    let mut names: FxHashSet<String> = FxHashSet::default();
    for c in ck.classes.values() {
        names.extend(c.methods.keys().cloned());
        names.extend(c.getters.keys().cloned());
        names.extend(c.fields.iter().map(|f| f.name.clone()));
    }
    for e in &ck.enums {
        names.extend(e.methods.keys().cloned());
    }
    for i in &ck.ifaces {
        names.extend(i.by_name.keys().cloned());
    }
    for x in &ck.exts {
        names.extend(x.methods.keys().cloned());
    }
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    let saved_module = ck.cur_module;
    ck.cur_module = m;
    let mut out = Vec::new();
    for name in names {
        let mut field: Option<TyId> = None;
        ck.trial(|ck| {
            field = ck.lookup_field(recv, &name).map(|(_, t)| t);
            false
        });
        if let Some(t) = field {
            out.push(Completion { detail: Some(ck.show(t)), label: name, kind: CompletionKind::Field });
            continue;
        }
        let mut found: Option<u32> = None;
        ck.trial(|ck| {
            found = ck.lookup_method(recv, &name, true).map(|r| r.fid);
            false
        });
        if let Some(f) = found {
            if ck.fns[f as usize].self_kind == crate::ast::SelfKind::None {
                continue;
            }
            out.push(Completion { detail: Some(fn_signature(ck, f)), label: name, kind: CompletionKind::Method });
        }
    }
    ck.cur_module = saved_module;
    out
}

/// What a bare name refers to in module `m` when used as a path prefix (`Point` in `Point.`):
/// the checker's resolution if recorded, else the module's items, imports and the prelude.
pub fn path_res(ck: &Checker, m: ModId, e: &Expr) -> Option<NameRes> {
    let ExprKind::Ident(name) = &e.kind else { return None };
    if let Some(r) = ck.names.get(&e.id) {
        return Some(*r);
    }
    let md = &ck.mods[m as usize];
    let it = md
        .items
        .get(name)
        .copied()
        .or_else(|| md.imported.get(name).map(|&(i, _)| i))
        .or_else(|| ck.mods.iter().filter(|p| p.is_prelude).find_map(|p| p.items.get(name).copied()))?;
    Some(match it {
        ScopeItem::Class(c) => NameRes::Class(c),
        ScopeItem::Enum(x) => NameRes::Enum(x),
        ScopeItem::Iface(x) => NameRes::Iface(x),
        ScopeItem::Fn(f) => NameRes::Fn(f),
        ScopeItem::Global(g) => NameRes::Global(g),
        ScopeItem::Module(x) => NameRes::Module(x),
        ScopeItem::Variant(x, v) => NameRes::Variant(x, v),
    })
}

/// Static members reachable through a type or module name (`Point.`, `Color.`, `mod.`).
pub fn path_completions(ck: &Checker, res: NameRes) -> Vec<Completion> {
    let mut out = Vec::new();
    match res {
        NameRes::Class(c) => {
            let Some(ci) = ck.classes.get(&c) else { return out };
            if ci.constructor.is_some() {
                out.push(Completion { label: "new".into(), kind: CompletionKind::Function, detail: ci.constructor.map(|f| fn_signature(ck, f)) });
            }
            for (n, &f) in &ci.methods {
                if ck.fns[f as usize].self_kind == crate::ast::SelfKind::None {
                    out.push(Completion { label: n.clone(), kind: CompletionKind::Function, detail: Some(fn_signature(ck, f)) });
                }
            }
            for (n, &g) in &ci.consts {
                out.push(Completion { label: n.clone(), kind: CompletionKind::Constant, detail: Some(ck.show(ck.globals[g as usize].ty)) });
            }
        }
        NameRes::Enum(e) => {
            let ei = &ck.enums[e as usize];
            for v in &ei.variants {
                out.push(Completion { label: v.name.clone(), kind: CompletionKind::Variant, detail: None });
            }
            for (n, &f) in &ei.methods {
                if ck.fns[f as usize].self_kind == crate::ast::SelfKind::None {
                    out.push(Completion { label: n.clone(), kind: CompletionKind::Function, detail: Some(fn_signature(ck, f)) });
                }
            }
        }
        NameRes::Module(m) => {
            for (n, &it) in &ck.mods[m as usize].items {
                out.push(scope_item_completion(ck, n, it));
            }
        }
        _ => {}
    }
    out
}

/// Names in scope at `offset` of module `m`: locals declared earlier in an enclosing function (or
/// at top level), module items, imports, prelude items and keywords.
pub fn scope_completions(ck: &Checker, m: ModId, offset: u32) -> Vec<Completion> {
    let md = &ck.mods[m as usize];
    let scopes = fn_scopes(&md.ast);
    let innermost_scope = |s: Span| -> Option<Span> {
        scopes.iter().copied().filter(|f| f.start <= s.start && s.end <= f.end).min_by_key(|f| f.end - f.start)
    };
    let enclosing: Vec<Span> = scopes.iter().copied().filter(|f| contains(*f, offset)).collect();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut out = Vec::new();
    // Nearest declarations first.
    let mut locals: Vec<&crate::check::LocalInfo> = ck
        .locals
        .iter()
        .filter(|l| l.span.file == md.file && l.span.start <= offset && !l.name.starts_with('<') && l.name != "self")
        .collect();
    locals.sort_by_key(|l| std::cmp::Reverse(l.span.start));
    for l in locals {
        let visible = match innermost_scope(l.span) {
            None => true,
            Some(s) => enclosing.contains(&s),
        };
        if visible && seen.insert(l.name.clone()) {
            out.push(Completion { label: l.name.clone(), kind: CompletionKind::Variable, detail: Some(ck.show(l.ty)) });
        }
    }
    let mut items: Vec<(&String, ScopeItem)> = md.items.iter().map(|(n, &i)| (n, i)).collect();
    items.extend(md.imported.iter().map(|(n, &(i, _))| (n, i)));
    for p in ck.mods.iter().filter(|p| p.is_prelude) {
        items.extend(p.items.iter().map(|(n, &i)| (n, i)));
    }
    for (n, it) in items {
        if n.starts_with('_') || n.starts_with('<') {
            continue;
        }
        if seen.insert(n.clone()) {
            out.push(scope_item_completion(ck, n, it));
        }
    }
    for k in KEYWORDS {
        if seen.insert(k.to_string()) {
            out.push(Completion { label: k.to_string(), kind: CompletionKind::Keyword, detail: None });
        }
    }
    out
}
