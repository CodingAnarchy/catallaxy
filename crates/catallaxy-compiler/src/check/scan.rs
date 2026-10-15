//! The pre-scan of a function, closure or `for` body for what its closures assign
//! ([`AssignScan`]), resolved by binding: an assignment inside a closure refers to the local in
//! scope where the closure is written, as the checker will resolve it, so a closure's own local
//! that shares a name with an outer one does not count.
//!
//! A closure that is only ever called directly through the local holding it (`f = fn() { ... }`
//! and then only `f()`, in the same function) is *contained*: it runs only at those calls, so the
//! locals it assigns lose their narrowings only there. Any other closure (a block, an argument,
//! one stored in a field or collection, captured by another closure, or returned) may run during
//! any later call.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{Block, Expr, ExprKind, Pattern, PatternKind};
use crate::diag::Span;

/// What a function, closure or `for` body (closures in it included) assigns, found before it is
/// checked: assignments that come later in the text may still run first, through closures,
/// loops and fibers.
#[derive(Default, Debug)]
pub struct AssignScan {
    /// Locals (by the span of their declaring name) that a closure which may run at any call
    /// assigns.
    pub closure_decls: FxHashSet<Span>,
    /// The same for locals the scan does not declare itself (parameters, pattern bindings): by
    /// name.
    pub in_closures: FxHashSet<String>,
    /// Locals (by declaring span) that contained closures assign: the declaring spans of the
    /// locals holding those closures, whose direct calls run them.
    pub runner_decls: FxHashMap<Span, Vec<(Span, String)>>,
    /// The same by name, for locals the scan does not declare itself.
    pub runner_names: FxHashMap<String, Vec<(Span, String)>>,
    /// How many times each name is assigned (declarations included).
    pub counts: FxHashMap<String, u32>,
}

impl AssignScan {
    /// Scans a body whose parameters (or `for` variables) are `params`.
    pub fn new<'a>(stmts: impl IntoIterator<Item = &'a Expr>, params: &[&str]) -> Self {
        let mut r = Resolver::default();
        r.scopes.push(Vec::new());
        for p in params {
            r.declare(p, None);
        }
        for s in stmts {
            r.walk(s);
        }
        r.finish()
    }

    /// Whether some closure assigns the local `name` declared at `span`, and may run at any call.
    pub fn closure_assigned(&self, name: &str, span: Span) -> bool {
        self.closure_decls.contains(&span) || self.in_closures.contains(name)
    }

    /// The declaring spans of the locals holding contained closures that assign the local `name`
    /// declared at `span`.
    pub fn runners(&self, name: &str, span: Span) -> Vec<(Span, String)> {
        let mut out = self.runner_decls.get(&span).cloned().unwrap_or_default();
        out.extend(self.runner_names.get(name).into_iter().flatten().cloned());
        out
    }
}

/// A declaration the scan saw: a local declared by an assignment (`Some(span)`), or one it does
/// not track precisely (a parameter, a pattern binding, a closure parameter: `None`).
struct Decl {
    name: String,
    span: Option<Span>,
    /// Closure nesting depth of the declaration (0: the scanned body itself).
    depth: u32,
    /// How many `for` bodies enclose the declaration: a `for` over a user type runs its body as
    /// a block, which `each` may keep.
    fors: u32,
    /// Its value is used other than by a direct call at its own depth, outside any further
    /// `for` body.
    escaped: bool,
}

/// A closure literal (or `spawn` block) and what it assigns outside itself.
struct ClosureLit {
    /// The local whose assigned value this closure literal is, if any.
    holder: Option<usize>,
    /// Depth of the closure's own body.
    depth: u32,
    /// Outer locals assigned in this closure's own body, declared right outside it.
    targets: Vec<usize>,
    /// Outer locals assigned from deeper inside it, or declared further out.
    far_targets: Vec<usize>,
}

#[derive(Default)]
struct Resolver {
    decls: Vec<Decl>,
    /// Innermost last: the declarations of each scope.
    scopes: Vec<Vec<usize>>,
    depth: u32,
    /// Indices into `lits` of the closures being walked, innermost last.
    open: Vec<usize>,
    lits: Vec<ClosureLit>,
    counts: FxHashMap<String, u32>,
    /// `for` bodies around the current point.
    fors: u32,
}

impl Resolver {
    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rev().flat_map(|s| s.iter().rev()).copied().find(|&d| self.decls[d].name == name)
    }

    fn declare(&mut self, name: &str, span: Option<Span>) -> usize {
        let d = self.decls.len();
        self.decls.push(Decl { name: name.to_string(), span, depth: self.depth, fors: self.fors, escaped: false });
        self.scopes.last_mut().unwrap().push(d);
        d
    }

    fn declare_pattern(&mut self, p: &Pattern) {
        let mut names = Vec::new();
        pattern_names(p, &mut names);
        for n in names {
            self.declare(&n, None);
        }
    }

    fn block(&mut self, b: &Block) {
        self.scopes.push(Vec::new());
        for s in &b.stmts {
            self.walk(s);
        }
        self.scopes.pop();
    }

    /// Walks a closure literal (or `spawn` block) held by `holder`.
    fn closure(&mut self, params: &[&str], body: &Block, holder: Option<usize>) {
        self.depth += 1;
        let lit = self.lits.len();
        self.lits.push(ClosureLit { holder, depth: self.depth, targets: Vec::new(), far_targets: Vec::new() });
        self.open.push(lit);
        self.scopes.push(Vec::new());
        for p in params {
            self.declare(p, None);
        }
        self.block(body);
        self.scopes.pop();
        self.open.pop();
        self.depth -= 1;
    }

    /// Walks `value`, the value assigned to the local `holder`: a closure literal there is held
    /// by it.
    fn value(&mut self, value: &Expr, holder: Option<usize>) {
        match &value.kind {
            ExprKind::Closure { params, body, .. } => {
                let names: Vec<&str> = params.iter().map(|p| p.name.name.as_str()).collect();
                self.closure(&names, body, holder);
            }
            _ => self.walk(value),
        }
    }

    /// An assignment to the declaration `d` here.
    fn assigned(&mut self, d: usize) {
        let dd = self.decls[d].depth;
        if dd >= self.depth {
            return;
        }
        // Assigned from inside a closure: attribute it to the innermost one.
        let Some(&lit) = self.open.last() else { return };
        if dd + 1 == self.lits[lit].depth {
            self.lits[lit].targets.push(d);
        } else {
            self.lits[lit].far_targets.push(d);
        }
    }

    fn walk(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Assign { target, op, value, ty } => {
                let ExprKind::Ident(n) = &target.kind else {
                    self.walk(target);
                    self.walk(value);
                    return;
                };
                *self.counts.entry(n.clone()).or_default() += 1;
                if ty.is_some() {
                    // A declaration; a closure value can refer to the variable itself.
                    if matches!(value.kind, ExprKind::Closure { .. }) {
                        let d = self.declare(n, Some(target.span));
                        self.value(value, Some(d));
                    } else {
                        self.walk(value);
                        self.declare(n, Some(target.span));
                    }
                    return;
                }
                match self.lookup(n) {
                    Some(d) => {
                        self.value(value, if op.is_none() { Some(d) } else { None });
                        self.assigned(d);
                    }
                    None => {
                        // The first assignment declares the variable after its value.
                        let lit = self.lits.len();
                        self.walk(value);
                        if op.is_some() {
                            return;
                        }
                        let d = self.declare(n, Some(target.span));
                        if matches!(value.kind, ExprKind::Closure { .. }) && lit < self.lits.len() {
                            self.lits[lit].holder = Some(d);
                        }
                    }
                }
            }
            ExprKind::Ident(n) => {
                if let Some(d) = self.lookup(n) {
                    self.decls[d].escaped = true;
                }
            }
            ExprKind::Call { callee, args, block } => {
                match &callee.kind {
                    // A direct call of a local at its own depth does not let the value escape.
                    ExprKind::Ident(n) => {
                        if let Some(d) = self.lookup(n) {
                            if self.decls[d].depth != self.depth || self.decls[d].fors != self.fors {
                                self.decls[d].escaped = true;
                            }
                        }
                    }
                    _ => self.walk(callee),
                }
                for a in args {
                    self.walk(&a.value);
                }
                if let Some(b) = block {
                    self.walk(b);
                }
            }
            ExprKind::Closure { params, body, .. } => {
                let names: Vec<&str> = params.iter().map(|p| p.name.name.as_str()).collect();
                self.closure(&names, body, None);
            }
            ExprKind::Spawn { body, .. } => self.closure(&[], body, None),
            ExprKind::Block(b) | ExprKind::Loop { body: b } => self.block(b),
            ExprKind::If { cond, then, else_ } => {
                self.walk(cond);
                self.block(then);
                if let Some(x) = else_ {
                    self.walk(x);
                }
            }
            ExprKind::IfLet { pattern, value, then, else_ } => {
                self.walk(value);
                self.scopes.push(Vec::new());
                self.declare_pattern(pattern);
                self.block(then);
                self.scopes.pop();
                if let Some(x) = else_ {
                    self.walk(x);
                }
            }
            ExprKind::While { cond, body } => {
                self.walk(cond);
                self.block(body);
            }
            ExprKind::For { pattern, iter, body } => {
                self.walk(iter);
                self.fors += 1;
                self.scopes.push(Vec::new());
                self.declare_pattern(pattern);
                self.block(body);
                self.scopes.pop();
                self.fors -= 1;
            }
            ExprKind::Match { subject, arms } => {
                self.walk(subject);
                for a in arms {
                    self.scopes.push(Vec::new());
                    self.declare_pattern(&a.pattern);
                    if let Some(g) = &a.guard {
                        self.walk(g);
                    }
                    self.walk(&a.body);
                    self.scopes.pop();
                }
            }
            ExprKind::Let { pattern, value } => {
                self.walk(value);
                self.declare_pattern(pattern);
            }
            ExprKind::WithLock { obj, body } => {
                self.walk(obj);
                self.block(body);
            }
            _ => {
                for c in crate::query::children(e) {
                    self.walk(c);
                }
            }
        }
    }

    fn finish(self) -> AssignScan {
        let mut scan = AssignScan { counts: self.counts, ..Default::default() };
        for lit in &self.lits {
            // Contained: held by a local declared right outside it whose value never escapes.
            let holder = lit.holder.filter(|&h| {
                let d = &self.decls[h];
                d.span.is_some() && !d.escaped && d.depth + 1 == lit.depth
            });
            for (i, &t) in lit.targets.iter().chain(&lit.far_targets).enumerate() {
                let d = &self.decls[t];
                // The holder is called where the target is declared, not in a `for` body inside
                // that (which a user `each` may keep and run at any call).
                let runner = holder
                    .filter(|&h| i < lit.targets.len() && self.decls[h].fors == d.fors)
                    .and_then(|h| Some((self.decls[h].span?, self.decls[h].name.clone())));
                match (runner, d.span) {
                    (Some(r), Some(s)) => scan.runner_decls.entry(s).or_default().push(r),
                    (Some(r), None) => scan.runner_names.entry(d.name.clone()).or_default().push(r),
                    (None, Some(s)) => {
                        scan.closure_decls.insert(s);
                    }
                    (None, None) => {
                        scan.in_closures.insert(d.name.clone());
                    }
                }
            }
        }
        scan
    }
}

/// The names a pattern binds.
pub fn pattern_names_of(p: &Pattern) -> Vec<String> {
    let mut out = Vec::new();
    pattern_names(p, &mut out);
    out
}

fn pattern_names(p: &Pattern, out: &mut Vec<String>) {
    match &p.kind {
        PatternKind::Bind(n) | PatternKind::Typed { name: n, .. } => out.push(n.name.clone()),
        PatternKind::Tuple(ps) | PatternKind::Or(ps) | PatternKind::Ctor { args: ps, .. } => {
            ps.iter().for_each(|q| pattern_names(q, out))
        }
        PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::Range { .. } => {}
    }
}
