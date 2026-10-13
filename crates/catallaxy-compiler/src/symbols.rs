//! Editor navigation over a checked compilation: a symbol index (every declaration and use of
//! locals, functions, methods, classes, enums, variants, interfaces, fields and globals in user
//! modules), signature information for calls, inlay hints and document outlines. Used by the
//! language server for references, rename, signature help, inlay hints and symbol lists.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{
    Arg, ClassDecl, Expr, ExprKind, FnDecl, Item, Module, Pattern, PatternKind, TypeExpr, TypeExprKind, TypeParam,
};
use crate::check::{
    CallRes, Checker, DeclRef, FnId, GlobalId, LocalId, ModId, NameRes, OpenReq, Owner, PatRes, ScopeItem, Synth,
};
use crate::diag::{SourceMap, Span};
use crate::query;
use crate::types::{ClassId, EnumId, IfaceId, Ty, TyId};

/// A named program entity.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Symbol {
    /// A local variable or parameter, identified by the span of its declaring name (a body checked
    /// more than once yields several local ids for one source variable).
    Local(Span),
    /// A function or method. Overriding methods share the symbol of the topmost overridden one.
    Fn(FnId),
    Class(ClassId),
    Enum(EnumId),
    Iface(IfaceId),
    Variant(EnumId, u32),
    /// A field: declaring class and index of the declaration in that class.
    Field(ClassId, u32),
    Global(GlobalId),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub sym: Symbol,
    /// The name's span.
    pub span: Span,
    pub is_decl: bool,
}

/// Every occurrence of every symbol in the compilation's user modules, sorted by position.
pub struct Index {
    pub occs: Vec<Occurrence>,
    /// Source files of user (non-prelude) modules.
    pub user_files: FxHashSet<u32>,
}

fn text_at(sm: &SourceMap, span: Span) -> Option<&str> {
    sm.files.get(span.file as usize)?.text.get(span.start as usize..span.end as usize)
}

/// The symbol's name as written in source.
pub fn symbol_name(ck: &Checker, sm: &SourceMap, sym: Symbol) -> Option<String> {
    Some(match sym {
        Symbol::Local(sp) => text_at(sm, sp)?.to_string(),
        Symbol::Fn(f) => ck.fns.get(f as usize)?.name.clone(),
        Symbol::Class(c) => ck.classes.get(&c)?.name.clone(),
        Symbol::Enum(e) => ck.enums.get(e as usize)?.name.clone(),
        Symbol::Iface(i) => ck.ifaces.get(i as usize)?.name.clone(),
        Symbol::Variant(e, v) => ck.enums.get(e as usize)?.variants.get(v as usize)?.name.clone(),
        Symbol::Field(c, i) => {
            ck.classes.get(&c)?.fields.iter().find(|f| f.decl_class == c && f.decl_index == i as usize)?.name.clone()
        }
        Symbol::Global(g) => ck.globals.get(g as usize)?.name.clone(),
    })
}

/// Where the symbol is declared (its name), if it has a source declaration.
pub fn decl_span(ck: &Checker, sym: Symbol) -> Option<Span> {
    let sp = match sym {
        Symbol::Local(sp) => sp,
        Symbol::Fn(f) => {
            let fi = ck.fns.get(f as usize)?;
            if fi.decl == DeclRef::Synthetic {
                return None;
            }
            fi.span
        }
        Symbol::Class(c) => ck.classes.get(&c).filter(|c| !c.is_builtin)?.span,
        Symbol::Enum(e) => ck.enums.get(e as usize)?.span,
        Symbol::Iface(i) => ck.ifaces.get(i as usize)?.span,
        Symbol::Variant(e, v) => {
            let vi = ck.enums.get(e as usize)?.variants.get(v as usize)?;
            Span::new(vi.span.file, vi.span.start, vi.span.start + vi.name.len() as u32)
        }
        Symbol::Field(c, i) => {
            ck.classes.get(&c)?.fields.iter().find(|f| f.decl_class == c && f.decl_index == i as usize)?.span
        }
        Symbol::Global(g) => ck.globals.get(g as usize)?.span,
    };
    if sp.file == u32::MAX {
        None
    } else {
        Some(sp)
    }
}

/// The class of a (possibly frozen) class type.
fn class_of(ck: &Checker, t: TyId) -> Option<ClassId> {
    let t = ck.types.shallow_resolve(t);
    let t = match ck.types.get(t) {
        Ty::Frozen(i) => ck.types.shallow_resolve(*i),
        _ => t,
    };
    match ck.types.get(t) {
        Ty::Class(c, _) => Some(*c),
        _ => None,
    }
}

fn local_sym(ck: &Checker, sm: &SourceMap, l: LocalId) -> Option<Symbol> {
    let li = ck.locals.get(l as usize)?;
    // Implicit locals (`self`, `it`, generated temporaries, constructor parameters) have no
    // declaring name in source.
    (li.name != "self" && text_at(sm, li.span) == Some(li.name.as_str())).then_some(Symbol::Local(li.span))
}

fn field_sym(ck: &Checker, c: ClassId, index: usize) -> Option<Symbol> {
    let f = ck.classes.get(&c)?.fields.get(index)?;
    Some(Symbol::Field(f.decl_class, f.decl_index as u32))
}

/// The symbol of function `f`: getters stand for their field; an overriding method for the
/// topmost method it overrides, so renaming one renames the whole family.
pub fn fn_sym(ck: &Checker, f: FnId) -> Option<Symbol> {
    let fi = ck.fns.get(f as usize)?;
    match fi.synth {
        Synth::Getter { field } => {
            return match fi.owner {
                Owner::Class(c) => field_sym(ck, c, field),
                _ => None,
            }
        }
        Synth::Constructor(_) | Synth::Main(_) => return None,
        Synth::None => {}
    }
    let mut root = f;
    if let (Owner::Class(mut c), true) = (fi.owner, fi.self_kind != crate::ast::SelfKind::None) {
        while let Some(sc) = ck.classes.get(&c).and_then(|ci| ci.superclass).and_then(|t| class_of(ck, t)) {
            if let Some(&g) = ck.classes.get(&sc).and_then(|ci| ci.methods.get(&fi.name)) {
                root = g;
            }
            c = sc;
        }
    }
    Some(Symbol::Fn(root))
}

fn scope_item_sym(ck: &Checker, it: ScopeItem) -> Option<Symbol> {
    match it {
        ScopeItem::Class(c) => Some(Symbol::Class(c)),
        ScopeItem::Enum(e) => Some(Symbol::Enum(e)),
        ScopeItem::Iface(i) => Some(Symbol::Iface(i)),
        ScopeItem::Fn(f) => fn_sym(ck, f),
        ScopeItem::Global(g) => Some(Symbol::Global(g)),
        ScopeItem::Variant(e, v) => Some(Symbol::Variant(e, v)),
        ScopeItem::Module(_) => None,
    }
}

/// What `name` refers to at the top level of module `m`: its items, imports, then the prelude.
pub fn lookup_item(ck: &Checker, m: ModId, name: &str) -> Option<ScopeItem> {
    let md = &ck.mods[m as usize];
    md.items
        .get(name)
        .copied()
        .or_else(|| md.imported.get(name).map(|&(i, _)| i))
        .or_else(|| ck.mods.iter().filter(|p| p.is_prelude).find_map(|p| p.items.get(name).copied()))
}

/// The parameter local named `name` of function `f`.
fn param_sym(ck: &Checker, sm: &SourceMap, f: FnId, name: &str) -> Option<Symbol> {
    let ls = ck.fn_locals.get(&f)?;
    let l = *ls.iter().find(|&&l| ck.locals[l as usize].is_param && ck.locals[l as usize].name == name)?;
    local_sym(ck, sm, l)
}

struct Builder<'a> {
    ck: &'a Checker,
    sm: &'a SourceMap,
    user: FxHashSet<u32>,
    out: Vec<Occurrence>,
    module: ModId,
    /// Type parameter names in scope (not references to items).
    tparams: Vec<String>,
}

impl Builder<'_> {
    fn add(&mut self, sym: Option<Symbol>, span: Span, is_decl: bool) {
        let Some(sym) = sym else { return };
        if !self.user.contains(&span.file) {
            return;
        }
        // Only spans that spell the name (skips `Self`, implicit receivers and generated code).
        let Some(text) = text_at(self.sm, span) else { return };
        if symbol_name(self.ck, self.sm, sym).as_deref() != Some(text) {
            return;
        }
        self.out.push(Occurrence { sym, span, is_decl });
    }

    fn ident_sym(&self, e: &Expr) -> Option<Symbol> {
        let ck = self.ck;
        match ck.names.get(&e.id) {
            Some(NameRes::Local(l)) => local_sym(ck, self.sm, *l),
            Some(NameRes::Fn(f)) => fn_sym(ck, *f),
            Some(NameRes::Global(g)) => Some(Symbol::Global(*g)),
            Some(NameRes::Class(c)) => Some(Symbol::Class(*c)),
            Some(NameRes::Enum(x)) => Some(Symbol::Enum(*x)),
            Some(NameRes::Iface(x)) => Some(Symbol::Iface(*x)),
            Some(NameRes::Variant(x, v)) => Some(Symbol::Variant(*x, *v)),
            Some(NameRes::Module(_)) => None,
            None => match ck.calls.get(&e.id) {
                Some(CallRes::Variant { class, .. }) => {
                    ck.variant_of_class.get(class).map(|&(x, v)| Symbol::Variant(x, v))
                }
                _ => None,
            },
        }
    }

    fn member_sym(&self, e: &Expr, recv: &Expr) -> Option<Symbol> {
        let ck = self.ck;
        match ck.calls.get(&e.id)? {
            CallRes::Field { index, .. } => field_sym(ck, class_of(ck, *ck.expr_ty.get(&recv.id)?)?, *index as usize),
            CallRes::Global(g) => Some(Symbol::Global(*g)),
            CallRes::Variant { class, .. } => ck.variant_of_class.get(class).map(|&(x, v)| Symbol::Variant(x, v)),
            CallRes::Construct { .. } => None,
            _ => fn_sym(ck, query::called_fn(ck, e)?),
        }
    }

    /// Named arguments refer to the callee's parameters (or the fields of a constructor).
    fn named_args(&mut self, e: &Expr, args: &[Arg]) {
        let ck = self.ck;
        for a in args {
            let Some(n) = &a.name else { continue };
            let sym = match ck.calls.get(&e.id) {
                Some(CallRes::Construct { class, .. }) => ck.classes.get(class).and_then(|ci| match ci.init {
                    Some(init) => param_sym(ck, self.sm, init, &n.name),
                    None => ci
                        .fields
                        .iter()
                        .find(|f| f.name == n.name)
                        .map(|f| Symbol::Field(f.decl_class, f.decl_index as u32)),
                }),
                _ => query::called_fn(ck, e).and_then(|f| param_sym(ck, self.sm, f, &n.name)),
            };
            self.add(sym, n.span, false);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Ident(_) => {
                let s = self.ident_sym(e);
                self.add(s, e.span, false);
            }
            ExprKind::Member { recv, name, args, .. } => {
                let s = self.member_sym(e, recv);
                self.add(s, name.span, false);
                self.named_args(e, args);
            }
            ExprKind::Call { args, .. } => self.named_args(e, args),
            ExprKind::Closure { params, ret, .. } => {
                for p in params {
                    if let Some(t) = &p.ty {
                        self.ty(t);
                    }
                }
                if let Some(t) = ret {
                    self.ty(t);
                }
            }
            ExprKind::Assign { ty: Some(t), .. } | ExprKind::Is { ty: t, .. } | ExprKind::AsCast { ty: t, .. } => {
                self.ty(t)
            }
            ExprKind::IfLet { pattern, .. } | ExprKind::For { pattern, .. } | ExprKind::Let { pattern, .. } => {
                self.pat(pattern)
            }
            ExprKind::Match { arms, .. } => {
                for a in arms {
                    self.pat(&a.pattern);
                }
            }
            _ => {}
        }
        for c in query::children(e) {
            self.expr(c);
        }
    }

    fn pat(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Typed { ty, .. } => self.ty(ty),
            PatternKind::Ctor { path, args, .. } => {
                match self.ck.pats.get(&p.id) {
                    Some(PatRes::Variant { class, .. }) => {
                        if let Some(&(x, v)) = self.ck.variant_of_class.get(class) {
                            let n = path.len();
                            self.add(Some(Symbol::Variant(x, v)), path[n - 1].span, false);
                            if n >= 2 {
                                self.add(Some(Symbol::Enum(x)), path[n - 2].span, false);
                            }
                        }
                    }
                    Some(PatRes::Type { .. }) => self.type_path(path),
                    _ => {}
                }
                for a in args {
                    self.pat(a);
                }
            }
            PatternKind::Tuple(ps) | PatternKind::Or(ps) => {
                for a in ps {
                    self.pat(a);
                }
            }
            _ => {}
        }
    }

    fn type_path(&mut self, path: &[crate::ast::Ident]) {
        let ck = self.ck;
        let item = match path {
            [one] => {
                if one.name == "Self" || self.tparams.contains(&one.name) {
                    return;
                }
                lookup_item(ck, self.module, &one.name)
            }
            [m, last] => match lookup_item(ck, self.module, &m.name) {
                Some(ScopeItem::Module(x)) => ck.mods[x as usize].items.get(&last.name).copied(),
                _ => None,
            },
            _ => None,
        };
        let sym = match item {
            Some(ScopeItem::Class(c)) => Some(Symbol::Class(c)),
            Some(ScopeItem::Enum(x)) => Some(Symbol::Enum(x)),
            Some(ScopeItem::Iface(x)) => Some(Symbol::Iface(x)),
            _ => None,
        };
        if let Some(last) = path.last() {
            self.add(sym, last.span, false);
        }
    }

    fn ty(&mut self, te: &TypeExpr) {
        match &te.kind {
            TypeExprKind::Named { path, args } => {
                self.type_path(path);
                for a in args {
                    self.ty(a);
                }
            }
            TypeExprKind::Optional(t) | TypeExprKind::Frozen(t) => self.ty(t),
            TypeExprKind::Union(ts) | TypeExprKind::Inter(ts) | TypeExprKind::Tuple(ts) => {
                for t in ts {
                    self.ty(t);
                }
            }
            TypeExprKind::Fn { params, ret } => {
                for t in params {
                    self.ty(t);
                }
                if let Some(r) = ret {
                    self.ty(r);
                }
            }
            TypeExprKind::Inline(methods) => {
                for m in methods {
                    self.fn_sig(m);
                }
            }
            TypeExprKind::SelfType | TypeExprKind::Error => {}
        }
    }

    fn push_tparams(&mut self, tps: &[TypeParam]) -> usize {
        let n = self.tparams.len();
        self.tparams.extend(tps.iter().map(|t| t.name.name.clone()));
        for t in tps {
            for b in &t.bounds {
                self.ty(b);
            }
        }
        n
    }

    fn fn_sig(&mut self, f: &FnDecl) {
        let n = self.push_tparams(&f.type_params);
        for p in &f.params {
            if let Some(t) = &p.ty {
                self.ty(t);
            }
        }
        if let Some(t) = &f.ret {
            self.ty(t);
        }
        self.tparams.truncate(n);
    }

    fn class(&mut self, c: &ClassDecl) {
        let n = self.push_tparams(&c.type_params);
        if let Some(s) = &c.superclass {
            self.ty(s);
        }
        for f in &c.fields {
            self.ty(&f.ty);
        }
        for k in &c.consts {
            if let Some(t) = &k.ty {
                self.ty(t);
            }
        }
        for m in &c.methods {
            self.fn_sig(m);
        }
        self.tparams.truncate(n);
    }

    fn module(&mut self, m: ModId) {
        self.module = m;
        let md = &self.ck.mods[m as usize];
        let ast: &Module = &md.ast;
        for it in &ast.items {
            match it {
                Item::Fn(f) => self.fn_sig(f),
                Item::Class(c) => self.class(c),
                Item::Interface(i) => {
                    let n = self.push_tparams(&i.type_params);
                    i.methods.iter().for_each(|f| self.fn_sig(f));
                    self.tparams.truncate(n);
                }
                Item::Enum(e) => {
                    let n = self.push_tparams(&e.type_params);
                    for v in &e.variants {
                        for (_, t) in &v.fields {
                            self.ty(t);
                        }
                    }
                    e.methods.iter().for_each(|f| self.fn_sig(f));
                    self.tparams.truncate(n);
                }
                Item::Extend(x) => {
                    let n = self.push_tparams(&x.type_params);
                    self.ty(&x.target);
                    x.methods.iter().for_each(|f| self.fn_sig(f));
                    self.tparams.truncate(n);
                }
                Item::Import(imp) => {
                    for id in imp.items.iter().flatten() {
                        let sym = md.imported.get(&id.name).and_then(|&(si, _)| scope_item_sym(self.ck, si));
                        self.add(sym, id.span, false);
                    }
                }
                Item::Const(c) => {
                    if let Some(t) = &c.ty {
                        self.ty(t);
                    }
                }
                Item::Var(v) => self.ty(&v.ty),
                Item::Stmt(_) => {}
            }
        }
        for r in query::roots(ast) {
            self.expr(r);
        }
    }

    fn decls(&mut self) {
        let ck = self.ck;
        for l in 0..ck.locals.len() {
            let s = local_sym(ck, self.sm, l as LocalId);
            self.add(s, ck.locals[l].span, true);
        }
        for (f, fi) in ck.fns.iter().enumerate() {
            if matches!(fi.decl, DeclRef::Item(..) | DeclRef::Method(..)) {
                self.add(fn_sym(ck, f as FnId), fi.span, true);
            }
        }
        for ci in ck.classes.values() {
            if ci.is_builtin || ck.variant_of_class.contains_key(&ci.id) {
                continue;
            }
            self.add(Some(Symbol::Class(ci.id)), ci.span, true);
            for f in ci.fields.iter().filter(|f| f.decl_class == ci.id) {
                self.add(Some(Symbol::Field(ci.id, f.decl_index as u32)), f.span, true);
            }
        }
        for (x, ei) in ck.enums.iter().enumerate() {
            self.add(Some(Symbol::Enum(x as EnumId)), ei.span, true);
            for v in 0..ei.variants.len() {
                let sym = Symbol::Variant(x as EnumId, v as u32);
                if let Some(sp) = decl_span(ck, sym) {
                    self.add(Some(sym), sp, true);
                }
            }
        }
        for (x, ii) in ck.ifaces.iter().enumerate() {
            self.add(Some(Symbol::Iface(x as IfaceId)), ii.span, true);
        }
        for (g, gi) in ck.globals.iter().enumerate() {
            self.add(Some(Symbol::Global(g as GlobalId)), gi.span, true);
        }
    }
}

impl Index {
    pub fn build(ck: &Checker, sm: &SourceMap) -> Index {
        let user: FxHashSet<u32> = ck.mods.iter().filter(|m| !m.is_prelude).map(|m| m.file).collect();
        let mut b = Builder { ck, sm, user: user.clone(), out: Vec::new(), module: 0, tparams: Vec::new() };
        b.decls();
        for m in 0..ck.mods.len() {
            if !ck.mods[m].is_prelude {
                b.module(m as ModId);
            }
        }
        let mut occs = b.out;
        occs.sort_by_key(|o| (o.span.file, o.span.start, o.span.end, !o.is_decl));
        // One entry per (span, symbol), a declaration if any source of it is.
        occs.dedup_by(|b, a| a.span == b.span && a.sym == b.sym);
        Index { occs, user_files: user }
    }

    /// The occurrence under `offset` of `file` (end inclusive, so a cursor just past a name
    /// still finds it).
    pub fn at(&self, file: u32, offset: u32) -> Option<&Occurrence> {
        self.occs
            .iter()
            .filter(|o| o.span.file == file && o.span.start <= offset && offset <= o.span.end)
            .min_by_key(|o| (o.span.end - o.span.start, offset == o.span.end))
    }

    /// Every occurrence of `sym`, in position order.
    pub fn refs(&self, sym: Symbol) -> impl Iterator<Item = &Occurrence> {
        self.occs.iter().filter(move |o| o.sym == sym)
    }

    /// Whether every occurrence of `sym` can be rewritten: it is declared in a user module.
    pub fn renamable(&self, ck: &Checker, sym: Symbol) -> bool {
        decl_span(ck, sym).is_some_and(|sp| self.user_files.contains(&sp.file)) && self.refs(sym).any(|o| o.is_decl)
    }
}

/// Whether `name` can replace an identifier: a non-keyword identifier.
pub fn is_identifier(name: &str) -> bool {
    let mut cs = name.chars();
    let Some(first) = cs.next() else { return false };
    (first.is_alphabetic() || first == '_')
        && cs.all(|c| c.is_alphanumeric() || c == '_')
        && !query::KEYWORDS.contains(&name)
}

// ---- signature help --------------------------------------------------------------------------

/// A function signature for signature help: the label and each parameter's byte range in it.
#[derive(Clone, Debug)]
pub struct SignatureInfo {
    pub label: String,
    pub params: Vec<ParamLabel>,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ParamLabel {
    pub name: String,
    /// Byte range of the parameter in the signature label.
    pub range: (u32, u32),
    pub has_default: bool,
    /// The trailing-block parameter (the last one, of function type).
    pub is_block: bool,
}

/// The AST of a declared function.
pub fn fn_decl(ck: &Checker, f: FnId) -> Option<&FnDecl> {
    let item = |m: ModId, i: usize| ck.mods.get(m as usize)?.ast.items.get(i);
    match ck.fns.get(f as usize)?.decl {
        DeclRef::Item(m, i) => match item(m, i)? {
            Item::Fn(d) => Some(d),
            _ => None,
        },
        DeclRef::Method(m, i, j) => match item(m, i)? {
            Item::Class(c) => c.methods.get(j),
            Item::Interface(x) => x.methods.get(j),
            Item::Enum(x) => x.methods.get(j),
            Item::Extend(x) => x.methods.get(j),
            _ => None,
        },
        _ => None,
    }
}

/// Source text of parameter `k`'s default value (`Class.new` takes field defaults).
fn default_text(ck: &Checker, sm: &SourceMap, f: FnId, k: usize) -> Option<String> {
    let fi = &ck.fns[f as usize];
    let span = match (&fi.synth, fn_decl(ck, f)) {
        (_, Some(d)) => d.params.get(k)?.default.as_ref()?.span,
        (Synth::Constructor(c), None) => {
            let ci = ck.classes.get(c)?;
            if let Some(init) = ci.init {
                return default_text(ck, sm, init, k);
            }
            let fd = ci.fields.get(k)?;
            let dc = ck.classes.get(&fd.decl_class)?;
            match ck.mods[dc.module as usize].ast.items.get(dc.item?)? {
                Item::Class(cd) => cd.fields.get(fd.decl_index)?.default.as_ref()?.span,
                _ => return None,
            }
        }
        _ => return None,
    };
    let t = text_at(sm, span)?;
    Some(if t.len() > 40 || t.contains('\n') { "…".into() } else { t.to_string() })
}

/// The signature of `f` as called (without `self`), with default values and the block
/// parameter marked.
pub fn signature_info(ck: &Checker, sm: &SourceMap, f: FnId) -> SignatureInfo {
    let fi = &ck.fns[f as usize];
    let (owner, name) = match (&fi.synth, fi.owner) {
        (Synth::Constructor(c), _) => (ck.classes.get(c).map(|c| format!("{}.", c.name)).unwrap_or_default(), "new"),
        (_, Owner::Class(c)) => (ck.classes.get(&c).map(|c| format!("{}.", c.name)).unwrap_or_default(), &*fi.name),
        (_, Owner::Enum(e)) => (format!("{}.", ck.enums[e as usize].name), &*fi.name),
        (_, Owner::Iface(i)) => (format!("{}.", ck.ifaces[i as usize].name), &*fi.name),
        (_, Owner::Extend(x)) => (format!("{}.", ck.show(ck.exts[x as usize].target)), &*fi.name),
        (_, Owner::None) => (String::new(), &*fi.name),
    };
    let mut label = format!("fn {owner}{name}");
    if !fi.type_params.is_empty() {
        let tps: Vec<String> = fi.type_params.iter().map(|&p| ck.params[p as usize].name.clone()).collect();
        label.push_str(&format!("[{}]", tps.join(", ")));
    }
    label.push('(');
    let n = fi.params.len();
    let mut params = Vec::new();
    for (i, p) in fi.params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        let ty = fi.implicit.as_ref().and_then(|s| s.params.get(i).copied()).unwrap_or(p.ty);
        let is_block = i + 1 == n && matches!(ck.types.get(ck.types.shallow_resolve(ty)), Ty::Fn(..));
        let start = label.len() as u32;
        if p.is_move {
            label.push_str("move ");
        }
        label.push_str(&format!("{}: {}", p.name, ck.show(ty)));
        if p.has_default {
            label.push_str(" = ");
            label.push_str(&default_text(ck, sm, f, i).unwrap_or_else(|| "…".into()));
        }
        params.push(ParamLabel {
            name: p.name.clone(),
            range: (start, label.len() as u32),
            has_default: p.has_default,
            is_block,
        });
    }
    label.push(')');
    let ret = fi.implicit.as_ref().map(|s| s.ret).unwrap_or(fi.ret);
    if ret != ck.types.nil && !matches!(fi.synth, Synth::Constructor(_)) {
        label.push_str(&format!(" -> {}", ck.show(ret)));
    }
    let doc = match fi.synth {
        Synth::Constructor(c) => ck.classes.get(&c).and_then(|c| c.doc.clone()),
        _ => fi.doc.clone(),
    };
    SignatureInfo { label, params, doc }
}

/// The call whose argument list opens with the `(` at byte `paren`, and the function it calls.
pub fn call_at_paren(ck: &Checker, m: ModId, paren: u32) -> Option<FnId> {
    let mut found = None;
    query::walk(&ck.mods[m as usize].ast, &mut |e| {
        let end = match &e.kind {
            ExprKind::Call { callee, .. } => callee.span.end,
            ExprKind::Member { name, parens: true, .. } => name.span.end,
            _ => return,
        };
        if end == paren && found.is_none() {
            found = query::called_fn(ck, e);
        }
    });
    found
}

// ---- inlay hints ------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct InlayHint {
    /// Byte offset the hint is shown at.
    pub offset: u32,
    pub label: String,
}

fn shown(ck: &Checker, t: TyId) -> Option<String> {
    let r = ck.types.shallow_resolve(t);
    if ck.types.has_vars(t) || matches!(ck.types.get(r), Ty::Error | Ty::Never) {
        None
    } else {
        Some(ck.show(t))
    }
}

/// An implicit-generic parameter's type: its inferred structural requirement
/// (`{ fn name(self) -> _ }`) or the types it flows into.
fn shown_param(ck: &Checker, t: TyId) -> Option<String> {
    let Ty::Open(o) = ck.types.get(ck.types.shallow_resolve(t)) else { return shown(ck, t) };
    let info = ck.opens.get(*o as usize)?;
    // Types still open (unconstrained results) read as `_`.
    let show = |t: TyId| match ck.types.get(ck.types.shallow_resolve(t)) {
        Ty::Open(_) => "_".to_string(),
        _ => ck.show(t),
    };
    let ret =
        |t: TyId| if ck.types.shallow_resolve(t) == ck.types.nil { String::new() } else { format!(" -> {}", show(t)) };
    let mut parts: Vec<String> = Vec::new();
    for r in &info.reqs {
        let s = match r {
            OpenReq::Method { name, args, ret: r, .. } => {
                let ps: Vec<String> =
                    std::iter::once("self".to_string()).chain(args.iter().map(|&a| show(a))).collect();
                format!("fn {name}({}){}", ps.join(", "), ret(*r))
            }
            OpenReq::BinOp { op, rhs, ret: r, .. } => format!("fn {}(self, {}){}", op.symbol(), show(*rhs), ret(*r)),
            OpenReq::BinOpRhs { op, lhs, ret: r, .. } => format!("{} {} self{}", show(*lhs), op.symbol(), ret(*r)),
            OpenReq::SetIndex { index, value, .. } => format!("fn []=(self, {}, {})", show(*index), show(*value)),
            OpenReq::Unary { op, ret: r, .. } => {
                let sym = match op {
                    crate::ast::UnOp::Neg => "-",
                    crate::ast::UnOp::Not => "not",
                    crate::ast::UnOp::BitNot => "~",
                };
                format!("fn {sym}(self){}", ret(*r))
            }
            OpenReq::Index { args, ret: r, .. } => {
                let ps: Vec<String> =
                    std::iter::once("self".to_string()).chain(args.iter().map(|&a| show(a))).collect();
                format!("fn []({}){}", ps.join(", "), ret(*r))
            }
            OpenReq::Iter { elem, .. } => format!("Enumerable[{}]", show(*elem)),
            OpenReq::Call { args, ret: r, .. } => {
                format!("fn({}){}", args.iter().map(|&a| show(a)).collect::<Vec<_>>().join(", "), ret(*r))
            }
        };
        if !parts.contains(&s) {
            parts.push(s);
        }
    }
    if !parts.is_empty() {
        return Some(format!("{{ {} }}", parts.join("; ")));
    }
    let uppers: Vec<String> = info.uppers.iter().map(|&(u, _)| show(u)).collect();
    (!uppers.is_empty()).then(|| uppers.join(" & "))
}

/// Inferred types of unannotated locals (assignments, `let`/`for`/`if let` bindings, block
/// parameters) and of implicit-generic function parameters in module `m`.
pub fn inlay_hints(ck: &Checker, m: ModId) -> Vec<InlayHint> {
    let md = &ck.mods[m as usize];
    let mut out = Vec::new();
    let local = |out: &mut Vec<InlayHint>, l: LocalId, at: u32| {
        if let Some(s) = ck.locals.get(l as usize).and_then(|li| shown(ck, li.ty)) {
            out.push(InlayHint { offset: at, label: format!(": {s}") });
        }
    };
    fn binds(ck: &Checker, p: &Pattern, f: &mut dyn FnMut(LocalId, u32)) {
        match &p.kind {
            PatternKind::Bind(n) => {
                if let Some(PatRes::Bind(l)) = ck.pats.get(&p.id) {
                    f(*l, n.span.end);
                }
            }
            PatternKind::Ctor { args: ps, .. } | PatternKind::Tuple(ps) => ps.iter().for_each(|a| binds(ck, a, f)),
            _ => {}
        }
    }
    query::walk(&md.ast, &mut |e| match &e.kind {
        ExprKind::Assign { target, ty: None, op: None, .. } => {
            if let Some(&l) = ck.decl_locals.get(&e.id) {
                local(&mut out, l, target.span.end);
            }
        }
        ExprKind::Let { pattern, .. } | ExprKind::For { pattern, .. } | ExprKind::IfLet { pattern, .. } => {
            binds(ck, pattern, &mut |l, at| local(&mut out, l, at))
        }
        ExprKind::Closure { params, .. } => {
            let info = ck.closures.get(&e.id);
            if let Some(info) = info.filter(|i| i.params.len() == params.len()) {
                for (p, &l) in params.iter().zip(&info.params) {
                    if p.ty.is_none() {
                        local(&mut out, l, p.name.span.end);
                    }
                }
            }
        }
        _ => {}
    });
    // Implicit generics: the inferred requirement of each untyped parameter.
    let by_span: FxHashMap<Span, FnId> = ck
        .fns
        .iter()
        .enumerate()
        .filter(|(_, f)| f.has_untyped_params && f.implicit.is_some())
        .map(|(i, f)| (f.span, i as FnId))
        .collect();
    let mut fns: Vec<&FnDecl> = Vec::new();
    for it in &md.ast.items {
        match it {
            Item::Fn(f) => fns.push(f),
            Item::Class(c) => fns.extend(c.methods.iter()),
            Item::Enum(x) => fns.extend(x.methods.iter()),
            Item::Extend(x) => fns.extend(x.methods.iter()),
            _ => {}
        }
    }
    for d in fns {
        let Some(&f) = by_span.get(&d.name.span) else { continue };
        let sig = ck.fns[f as usize].implicit.as_ref().unwrap();
        for (p, &t) in d.params.iter().zip(&sig.params) {
            if p.ty.is_none() {
                if let Some(s) = shown_param(ck, t) {
                    out.push(InlayHint { offset: p.name.span.end, label: format!(": {s}") });
                }
            }
        }
    }
    out.sort_by_key(|h| h.offset);
    out
}

// ---- outline ---------------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OutlineKind {
    Function,
    Method,
    Class,
    Interface,
    Enum,
    Variant,
    Field,
    Constant,
    Variable,
    Extension,
}

/// A declaration in a module's outline.
#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub name: String,
    pub kind: OutlineKind,
    /// The whole declaration.
    pub span: Span,
    pub name_span: Span,
    pub detail: Option<String>,
    pub children: Vec<OutlineItem>,
}

/// The declarations of module `m`: items with their members, and top-level variables.
pub fn outline(ck: &Checker, sm: &SourceMap, m: ModId) -> Vec<OutlineItem> {
    let md = &ck.mods[m as usize];
    let by_span: FxHashMap<Span, FnId> = ck
        .fns
        .iter()
        .enumerate()
        .filter(|(_, f)| f.module == m && f.decl != DeclRef::Synthetic)
        .map(|(i, f)| (f.span, i as FnId))
        .collect();
    let func = |f: &FnDecl, kind: OutlineKind| OutlineItem {
        name: f.name.name.clone(),
        kind,
        span: f.span.to(f.name.span),
        name_span: f.name.span,
        detail: by_span.get(&f.name.span).map(|&id| query::fn_signature(ck, id)),
        children: Vec::new(),
    };
    let leaf = |name: &crate::ast::Ident, kind: OutlineKind, span: Span, detail: Option<String>| OutlineItem {
        name: name.name.clone(),
        kind,
        span: span.to(name.span),
        name_span: name.span,
        detail,
        children: Vec::new(),
    };
    let ty_text = |t: &TypeExpr| text_at(sm, t.span).map(str::to_string);
    let mut out = Vec::new();
    for it in &md.ast.items {
        match it {
            Item::Fn(f) => out.push(func(f, OutlineKind::Function)),
            Item::Class(c) => {
                let mut item = leaf(&c.name, OutlineKind::Class, c.span, None);
                item.children
                    .extend(c.fields.iter().map(|f| leaf(&f.name, OutlineKind::Field, f.span, ty_text(&f.ty))));
                item.children.extend(c.consts.iter().map(|k| leaf(&k.name, OutlineKind::Constant, k.span, None)));
                item.children.extend(c.methods.iter().map(|f| func(f, OutlineKind::Method)));
                out.push(item);
            }
            Item::Interface(i) => {
                let mut item = leaf(&i.name, OutlineKind::Interface, i.span, None);
                item.children.extend(i.methods.iter().map(|f| func(f, OutlineKind::Method)));
                out.push(item);
            }
            Item::Enum(e) => {
                let mut item = leaf(&e.name, OutlineKind::Enum, e.span, None);
                item.children.extend(e.variants.iter().map(|v| leaf(&v.name, OutlineKind::Variant, v.span, None)));
                item.children.extend(e.methods.iter().map(|f| func(f, OutlineKind::Method)));
                out.push(item);
            }
            Item::Extend(x) => {
                let target = text_at(sm, x.target.span).unwrap_or("?").to_string();
                let mut item = OutlineItem {
                    name: format!("extend {target}"),
                    kind: OutlineKind::Extension,
                    span: x.span,
                    name_span: x.target.span,
                    detail: None,
                    children: Vec::new(),
                };
                item.children.extend(x.methods.iter().map(|f| func(f, OutlineKind::Method)));
                out.push(item);
            }
            Item::Const(c) => out.push(leaf(&c.name, OutlineKind::Constant, c.span, c.ty.as_ref().and_then(ty_text))),
            Item::Var(v) => out.push(leaf(&v.name, OutlineKind::Variable, v.span, ty_text(&v.ty))),
            Item::Stmt(e) => {
                // Top-level variables: their first assignment.
                if let (ExprKind::Assign { target, .. }, Some(&l)) = (&e.kind, ck.decl_locals.get(&e.id)) {
                    if let ExprKind::Ident(n) = &target.kind {
                        out.push(OutlineItem {
                            name: n.clone(),
                            kind: OutlineKind::Variable,
                            span: e.span,
                            name_span: target.span,
                            detail: Some(ck.show(ck.locals[l as usize].ty)),
                            children: Vec::new(),
                        });
                    }
                }
            }
            Item::Import(_) => {}
        }
    }
    out
}
