//! Expression checking.

use rustc_hash::FxHashMap;

use super::caps::Cap;
use super::lookup::MethodRef;
use super::*;
use crate::ast::{Arg, BinOp, Block, ClosureParam, Expr, ExprKind, StrPart, TypeExpr, TypeExprKind, UnOp};
use crate::check::collect::TypeScope;

/// A static path in expression position (types, modules, functions).
#[derive(Clone, Debug)]
pub enum PathRes {
    Class(ClassId, Option<Vec<TyId>>),
    Enum(EnumId, Option<Vec<TyId>>),
    Iface(IfaceId),
    Module(ModId),
    Fn(FnId),
    Global(GlobalId),
    Variant(EnumId, u32, Option<Vec<TyId>>),
}

/// Narrowings implied by a condition being true / false.
pub type Narrowings = (Vec<(NKey, TyId)>, Vec<(NKey, TyId)>);

impl Checker {
    pub fn check_against(&mut self, e: &Expr, ty: TyId, what: &str) -> TyId {
        let t = self.check_expr(e, Some(ty));
        self.require_assignable(e, t, ty, what);
        ty
    }

    /// Reports a mismatch if `t` (the type of `e`) is not assignable to `ty`.
    pub fn require_assignable(&mut self, e: &Expr, t: TyId, ty: TyId, what: &str) -> bool {
        if self.assignable(t, ty) {
            if matches!(self.types.get(self.types.shallow_resolve(ty)), Ty::Dyn) && self.suppress_diags == 0 {
                // A collection converted to `Dyn` keeps its element types as run-time tags.
                self.dyn_conversions.insert(e.id, t);
            }
            return true;
        }
        if self.try_int_to_float(t, ty) {
            self.coerce.insert(e.id, Coercion::IntToFloat);
            return true;
        }
        let (exp, got) = (self.show(ty), self.show(t));
        let mut d =
            Diag::error("E0300", format!("mismatched types in {what}: expected `{exp}`, found `{got}`"), e.span);
        let tr = self.types.shallow_resolve(t);
        if let Some(inner) = self.types.strip_nil(tr) {
            if self.trial(|c| c.assignable(inner, ty)) {
                let note = self.loop_dropped_note(e).unwrap_or_else(|| "the value may be nil".to_string());
                d = d.note(note).fix("assert it is not nil", e.span.after(), "!").suggest(
                    "provide a default",
                    e.span.after(),
                    " ?? <default>",
                );
            }
        }
        if tr == self.types.float && self.types.shallow_resolve(ty) == self.types.int {
            d = d.fix("convert to Int", e.span.after(), ".to_i");
        }
        if let Ty::Iface(..) = self.types.get(self.types.shallow_resolve(ty)).clone() {
            for n in self.conformance_notes(t, ty) {
                d = d.note(n);
            }
        }
        if matches!(self.types.get(tr), Ty::Dyn) {
            d = d.fix("convert with a checked cast", e.span.after(), format!(" as? {exp}"));
        }
        self.emit(d);
        false
    }

    pub fn check_expr(&mut self, e: &Expr, expected: Option<TyId>) -> TyId {
        let outer = std::mem::replace(&mut self.expr_span, e.span);
        let t = self.check_expr_inner(e, expected);
        self.expr_span = outer;
        self.record_ty(e.id, t);
        if self.may_mutate(e) {
            self.clear_unstable_paths();
        }
        if let Some(span) = self.runner_called(e) {
            self.clear_runner(span);
        }
        t
    }

    /// For a direct call `f(...)` of a local closure, the span declaring `f`.
    fn runner_called(&self, e: &Expr) -> Option<Span> {
        let ExprKind::Call { callee, .. } = &e.kind else { return None };
        match (&callee.kind, self.names.get(&callee.id)) {
            (ExprKind::Ident(_), Some(NameRes::Local(f))) => Some(self.locals[*f as usize].span),
            _ => None,
        }
    }

    /// Before a loop: what its body may do on one iteration happens before the next, so field
    /// paths it may change lose their narrowing for the whole loop (the checker visits the body
    /// once). A call anywhere drops the unstable ones, an assignment to a field drops paths
    /// through that field name, and an assignment to a local drops paths rooted at it.
    fn loop_invalidate(&mut self, body: &Block) {
        if self
            .ctxs
            .last()
            .is_none_or(|c| c.path_narrow.iter().all(|l| l.is_empty()) && c.narrow.iter().all(|l| l.is_empty()))
        {
            return;
        }
        let calls = self.block_may_call(body, None);
        let mut fields: Vec<String> = Vec::new();
        let mut locals: Vec<String> = Vec::new();
        let mut stack: Vec<&Expr> = body.stmts.iter().collect();
        while let Some(x) = stack.pop() {
            if let ExprKind::Assign { target, .. } = &x.kind {
                match &target.kind {
                    ExprKind::Member { name, .. } => fields.push(name.name.clone()),
                    ExprKind::Ident(n) => locals.push(n.clone()),
                    _ => {}
                }
            }
            stack.extend(crate::query::children(x));
        }
        if calls {
            self.clear_unstable_paths();
        }
        // Locals that contained closures assign, when the body calls one of them.
        let narrowed: Vec<LocalId> = self.ctxs.last().map_or(Vec::new(), |c| {
            let mut v: Vec<LocalId> = c.narrow.iter().flat_map(|l| l.keys().copied()).collect();
            v.extend(c.path_narrow.iter().flat_map(|l| l.keys().map(|p| p.0)));
            v
        });
        let run: Vec<LocalId> = narrowed.into_iter().filter(|&l| self.loop_calls_runner(l, body, None)).collect();
        for &l in &run {
            self.clear_narrowing(l);
        }
        let roots: Vec<LocalId> = locals.iter().filter_map(|n| self.lookup_local_quiet(n)).collect();
        self.clear_paths(|p, _| roots.contains(&p.0) || p.1.iter().any(|f| fields.contains(f)));
    }

    /// Before a loop: a narrowing of a local holds in the body only if it holds on every path into
    /// it, that is before the first iteration and after each one (at the end of the body or a
    /// `next`). The body is checked once, so it is scanned first. A local narrowed here that the
    /// loop (its condition included) assigns keeps its narrowing only if every such assignment
    /// visibly keeps the value within it; otherwise the narrowing is dropped for the loop. A kept
    /// narrowing is recorded in `loop_facts`, so each assignment is checked against it, and holds
    /// again after the loop. Returns the mark to pass to `loop_exit`.
    fn loop_enter(&mut self, cond: Option<&Expr>, body: &Block, iter_calls: bool) -> (usize, usize) {
        let mark = (self.loop_facts.len(), self.loop_dropped.len());
        // Whether an iteration may run a closure: a call in the body or condition, or a user
        // `each` driving a `for`.
        let calls = iter_calls || self.block_may_call(body, cond);
        let mut assigns: Vec<(&str, Option<BinOp>, &Expr, bool)> = Vec::new();
        let mut stack: Vec<(&Expr, bool)> = body.stmts.iter().chain(cond).map(|x| (x, false)).collect();
        while let Some((x, deferred)) = stack.pop() {
            if let ExprKind::Assign { target, op, value, ty: None } = &x.kind {
                if let ExprKind::Ident(n) = &target.kind {
                    assigns.push((n.as_str(), *op, value, deferred));
                }
            }
            // Code that may run later, or on another thread, can assign at any time.
            let deferred = deferred || matches!(x.kind, ExprKind::Closure { .. } | ExprKind::Spawn { .. });
            stack.extend(crate::query::children(x).into_iter().map(|c| (c, deferred)));
        }
        let mut seen: Vec<LocalId> = Vec::new();
        for &(name, ..) in &assigns {
            let Some(l) = self.lookup_local_quiet(name) else { continue };
            if seen.contains(&l) {
                continue;
            }
            seen.push(l);
            if (self.dies_at_calls(l) && calls) || self.loop_calls_runner(l, body, cond) {
                // Already dropped at each call; a loop fact would outlive those.
                if self.local_ty(l) != self.locals[l as usize].ty {
                    self.clear_narrowing(l);
                    self.loop_dropped.push(l);
                }
                continue;
            }
            let declared = self.locals[l as usize].ty;
            let narrowed = self.local_ty(l);
            // Only narrowings to a subtype (not, say, the frozen view after `freeze`).
            if narrowed == declared || !self.trial(|c| c.assignable(narrowed, declared)) {
                continue;
            }
            // An assignment in a closure is to `l` only if the pre-scan found that some closure
            // assigns `l` (by binding); otherwise it is to a local of the closure.
            let by_closure = self.locals[l as usize].closure_assigned || !self.locals[l as usize].runners.is_empty();
            let mine: Vec<_> = assigns
                .iter()
                .filter(|a| self.lookup_local_quiet(a.0) == Some(l) && (!a.3 || by_closure))
                .copied()
                .collect();
            let keeps = mine
                .into_iter()
                .all(|(_, op, value, deferred)| !deferred && self.assign_keeps(op, value, declared, narrowed));
            if keeps {
                self.loop_facts.push((l, narrowed, self.ctxs.len()));
            } else {
                self.clear_narrowing(l);
                self.loop_dropped.push(l);
            }
        }
        mark
    }

    /// After a loop: the narrowings `loop_enter` kept held on every iteration, so they hold after it.
    fn loop_exit(&mut self, mark: (usize, usize)) {
        let kept = self.loop_facts.split_off(mark.0);
        self.loop_dropped.truncate(mark.1);
        for (l, t, _) in kept {
            self.narrow(l, t);
        }
    }

    /// Whether `x op= value` (or `x = value`) visibly leaves `x`, declared `declared`, within
    /// `narrowed`, before `value` is checked. Unknown values do not.
    fn assign_keeps(&mut self, op: Option<BinOp>, value: &Expr, declared: TyId, narrowed: TyId) -> bool {
        let non_nil = self.types.strip_nil(self.types.shallow_resolve(declared)) == Some(narrowed);
        let guess = match op {
            // `x ??= v` is never nil afterwards.
            Some(BinOp::Coalesce) => return non_nil,
            Some(BinOp::And | BinOp::Or) => Some(self.types.bool),
            Some(_) => return non_nil,
            None => match self.guess_value(value) {
                ValueGuess::Ty(t) => Some(t),
                ValueGuess::NonNil => return non_nil,
                ValueGuess::Unknown => None,
            },
        };
        guess.is_some_and(|t| self.trial(|c| c.assignable(t, narrowed)))
    }

    /// What an expression evaluates to, judged from its form alone (it is not checked yet).
    fn guess_value(&mut self, e: &Expr) -> ValueGuess {
        match &e.kind {
            ExprKind::Int(_) => ValueGuess::Ty(self.types.int),
            ExprKind::Float(_) => ValueGuess::Ty(self.types.float),
            ExprKind::Str(_) => ValueGuess::Ty(self.types.string),
            ExprKind::Bool(_) | ExprKind::Is { .. } | ExprKind::Unary(UnOp::Not, _) => ValueGuess::Ty(self.types.bool),
            ExprKind::Nil => ValueGuess::Ty(self.types.nil),
            ExprKind::Binary(BinOp::Coalesce, _, d) => match self.guess_value(d) {
                ValueGuess::Ty(t) if !self.types.is_nilable(t) => ValueGuess::NonNil,
                ValueGuess::NonNil => ValueGuess::NonNil,
                _ => ValueGuess::Unknown,
            },
            // Operators (an operator method's result type is checked at the assignment), literals
            // of collections, `!` and constructors are never nil.
            ExprKind::Binary(..)
            | ExprKind::Unary(..)
            | ExprKind::List(_)
            | ExprKind::Dict(_)
            | ExprKind::Tuple(_)
            | ExprKind::Range { .. }
            | ExprKind::Unwrap(_) => ValueGuess::NonNil,
            ExprKind::Member { name, .. } if name.name == "new" => ValueGuess::NonNil,
            // A function with a declared, non-generic result.
            ExprKind::Call { callee, .. } => match self.resolve_path(callee) {
                Some(PathRes::Fn(f)) => {
                    let f = &self.fns[f as usize];
                    if f.ret_annotated && f.type_params.is_empty() {
                        ValueGuess::Ty(f.ret)
                    } else {
                        ValueGuess::Unknown
                    }
                }
                _ => ValueGuess::Unknown,
            },
            _ => ValueGuess::Unknown,
        }
    }

    /// A note for a nil error on `e`, when `e` is a local whose narrowing an enclosing loop dropped,
    /// or one that a call may change (`dies_at_calls`).
    fn loop_dropped_note(&self, e: &Expr) -> Option<String> {
        let ExprKind::Ident(n) = &e.kind else { return None };
        let l = match self.names.get(&e.id) {
            Some(NameRes::Local(l)) => *l,
            _ => self.lookup_local_quiet(n)?,
        };
        if self.loop_dropped.contains(&l) {
            return Some(format!(
                "`{n}` is not nil before this loop, but the loop assigns it a value that may be nil, so it may be nil here on a later iteration; check it inside the loop"
            ));
        }
        if !self.dies_at_calls(l) {
            let runners = &self.locals[l as usize].runners;
            let mut names: Vec<String> = runners.iter().map(|r| format!("`{}`", r.1)).collect();
            names.dedup();
            if names.is_empty() {
                return None;
            }
            return Some(format!(
                "the closure in {} assigns `{n}`, so a nil test on it holds only until that closure is called; bind the value instead: `if let v = {n} {{ ... }}`",
                names.join(" or ")
            ));
        }
        let why = if self.locals[l as usize].closure_assigned {
            format!("a closure assigns `{n}`, so a nil test on it holds only until the next call, which may run the closure")
        } else {
            format!(
                "`{n}` is assigned elsewhere in its function, so in this closure a nil test outside it does not apply, and one inside it holds only until the next call"
            )
        };
        Some(format!("{why}; bind the value instead: `if let v = {n} {{ ... }}`"))
    }

    /// For an assignment to local `l` of a value of type `vt` inside loops: each narrowing of `l`
    /// that a loop kept (`loop_enter`) must still hold, since the next iteration relies on it.
    fn check_loop_facts(&mut self, l: LocalId, vt: TyId, value: &Expr) {
        let facts: Vec<TyId> = self.loop_facts.iter().filter(|f| f.0 == l).map(|f| f.1).collect();
        for n in facts {
            if self.trial(|c| c.assignable(vt, n)) {
                continue;
            }
            let name = self.locals[l as usize].name.clone();
            let msg = format!(
                "cannot assign `{}` to `{name}` here: the enclosing loop treats `{name}` as `{}`",
                self.show(vt),
                self.show(n)
            );
            let note = format!(
                "`{name}` was narrowed to `{}` before the loop, and every assignment to it in the loop must keep it so, because the next iteration relies on it; check it again inside the loop instead",
                self.show(n)
            );
            self.emit(Diag::error("E0324", msg, value.span).note(note));
            return;
        }
    }

    fn check_expr_inner(&mut self, e: &Expr, expected: Option<TyId>) -> TyId {
        match &e.kind {
            ExprKind::Int(s) => {
                if let Some(exp) = expected {
                    let exp = self.types.shallow_resolve(exp);
                    let float = self.types.float;
                    if exp == float || self.types.strip_nil(exp) == Some(float) {
                        self.coerce.insert(e.id, Coercion::IntToFloat);
                        return float;
                    }
                }
                if s.parse::<num_bigint::BigInt>().is_err() {
                    self.err("E0002", "invalid integer literal", e.span);
                }
                self.types.int
            }
            ExprKind::Float(_) => self.types.float,
            ExprKind::Bool(_) => self.types.bool,
            ExprKind::Nil => {
                if let Some(exp) = expected {
                    if self.types.is_nilable(exp) {
                        return exp;
                    }
                }
                self.types.nil
            }
            ExprKind::Str(parts) => {
                for p in parts {
                    if let StrPart::Expr(x) | StrPart::Fmt(x, ..) = p {
                        let t = self.check_expr(x, None);
                        if self.is_never(t) {
                            self.err("E0304", "this interpolation never produces a value", x.span);
                        } else if let StrPart::Fmt(_, spec, span) = p {
                            self.check_format_spec(t, spec, *span);
                        }
                    }
                }
                self.types.string
            }
            ExprKind::Ident(name) => self.check_ident(e, name),
            ExprKind::SelfValue => match self.self_local().and_then(|_| self.lookup_local("self")) {
                Some(l) => {
                    self.names.insert(e.id, NameRes::Local(l));
                    self.local_ty(l)
                }
                None => {
                    self.err("E0305", "`self` is only available inside methods", e.span);
                    self.types.error
                }
            },
            ExprKind::List(items) => self.check_list(items, expected),
            ExprKind::Dict(entries) => self.check_dict(entries, expected),
            ExprKind::Tuple(items) => {
                let exp: Option<Vec<TyId>> =
                    expected.and_then(|t| match self.types.get(self.types.shallow_resolve(t)) {
                        Ty::Tuple(ts) if ts.len() == items.len() => Some(ts.clone()),
                        _ => None,
                    });
                let ts: Vec<TyId> = items
                    .iter()
                    .enumerate()
                    .map(|(i, x)| match &exp {
                        Some(ts) => {
                            let t = self.check_expr(x, Some(ts[i]));
                            if self.trial(|c| c.assignable(t, ts[i])) {
                                ts[i]
                            } else {
                                t
                            }
                        }
                        None => self.check_expr(x, None),
                    })
                    .collect();
                self.types.tuple(ts)
            }
            ExprKind::Unary(op, x) => self.check_unary(e, *op, x),
            ExprKind::Binary(op, l, r) => self.check_binary(e, *op, l, r, expected),
            ExprKind::Range { lo, hi, .. } => {
                let int = self.types.int;
                self.check_against(lo, int, "range bound");
                self.check_against(hi, int, "range bound");
                self.types.range
            }
            ExprKind::Call { callee, args, block } => self.check_call(e, callee, args, block.as_deref(), expected),
            ExprKind::Member { recv, name, args, block, parens, safe } => {
                let t = self.check_member(e, recv, name, args, block.as_deref(), *parens, *safe, expected);
                self.check_protected_escape(recv, name, args, block.as_deref(), t);
                t
            }
            ExprKind::Index { recv, args } => self.check_index(e, recv, args),
            ExprKind::Closure { params, ret, body, .. } => {
                self.check_closure(e, params, ret.as_ref(), body, expected, None)
            }
            ExprKind::If { cond, then, else_ } => self.check_if(cond, then, else_.as_deref(), expected),
            ExprKind::IfLet { pattern, value, then, else_ } => {
                self.check_if_let(pattern, value, then, else_.as_deref(), expected)
            }
            ExprKind::While { cond, body } => {
                self.loop_invalidate(body);
                let mark = self.loop_enter(Some(cond), body, false);
                let (tn, _) = self.check_cond(cond);
                self.ctx().loops.push(LoopCtx { breaks: Vec::new(), is_foreach: false });
                self.push_scope();
                for (l, t) in tn {
                    self.narrow(l, t);
                }
                self.check_block(body, None);
                self.pop_scope();
                self.ctx().loops.pop();
                self.loop_exit(mark);
                self.types.nil
            }
            ExprKind::Loop { body } => {
                self.loop_invalidate(body);
                let mark = self.loop_enter(None, body, false);
                self.ctx().loops.push(LoopCtx { breaks: Vec::new(), is_foreach: false });
                self.check_block(body, None);
                let lp = self.ctx().loops.pop().unwrap();
                self.loop_exit(mark);
                if lp.breaks.is_empty() {
                    self.types.never
                } else {
                    self.join(&lp.breaks)
                }
            }
            ExprKind::For { pattern, iter, body } => self.check_for(e, pattern, iter, body),
            ExprKind::Match { subject, arms } => self.check_match(e, subject, arms, expected),
            ExprKind::Block(b) => self.check_block(b, expected),
            ExprKind::Return(v) => {
                self.check_return(e, v.as_deref());
                self.types.never
            }
            ExprKind::Break(v) => {
                self.check_break(e, v.as_deref());
                self.types.never
            }
            ExprKind::Continue => {
                self.check_continue(e);
                self.types.never
            }
            ExprKind::Assign { target, op, value, ty } => {
                self.check_assign(e, target, *op, value, ty.as_ref());
                self.types.nil
            }
            ExprKind::Let { pattern, value } => {
                let t = self.check_expr(value, None);
                self.bind_irrefutable(pattern, t);
                self.types.nil
            }
            ExprKind::Is { expr, ty } => {
                let src = self.check_expr(expr, None);
                let t = self.resolve_test_type(ty);
                self.check_type_test(src, t, Some(ty), ty.span);
                let rt = self.rt_of(t.ty);
                let idx = self.rt_type(rt);
                self.pats.insert(e.id, PatRes::Type { rt: idx, bind: None });
                self.types.bool
            }
            ExprKind::AsCast { expr, ty } => {
                let src = self.check_expr(expr, None);
                let t = self.resolve_test_type(ty);
                self.check_type_test(src, t, Some(ty), ty.span);
                let rt = self.rt_of(t.ty);
                let idx = self.rt_type(rt);
                self.pats.insert(e.id, PatRes::Type { rt: idx, bind: None });
                // The erased form keeps the type arguments the value's type implies.
                let res = if t.erased { self.split_by_test(src, t).0 } else { t.ty };
                self.types.optional(res)
            }
            ExprKind::Unwrap(x) => {
                let t = self.check_expr(x, None);
                match self.types.strip_nil(t) {
                    Some(inner) => inner,
                    None => {
                        if !matches!(self.types.get(self.types.shallow_resolve(t)), Ty::Dyn | Ty::Error | Ty::Open(_)) {
                            let msg = format!("`!` on a value of type `{}`, which is never nil", self.show(t));
                            self.emit(Diag::warning("W0001", msg, e.span).fix(
                                "remove `!`",
                                Span::new(e.span.file, e.span.end - 1, e.span.end),
                                "",
                            ));
                        }
                        t
                    }
                }
            }
            ExprKind::Try(x) => self.check_try(e, x),
            ExprKind::Spawn { is_move, body } => self.check_spawn(e, *is_move, body),
            ExprKind::Move(x) => {
                let t = self.check_expr(x, expected);
                match &x.kind {
                    ExprKind::Ident(_) => {
                        if let Some(NameRes::Local(l)) = self.names.get(&x.id).copied() {
                            self.mark_moved(l, e.span);
                        }
                    }
                    // A fresh literal has no other reference: moving it is a no-op.
                    ExprKind::List(_) | ExprKind::Dict(_) | ExprKind::Tuple(_) => {}
                    _ => self.err("E0606", "only local variables (or fresh literals) can be moved", x.span),
                }
                t
            }
            ExprKind::WithLock { obj, body } => {
                let t = self.check_expr(obj, None);
                if self.cap_of(t) != Cap::Shareable
                    && !matches!(self.types.get(self.types.shallow_resolve(t)), Ty::Error | Ty::Dyn)
                {
                    let msg = format!("`with lock` needs an instance of a `shared class`, found `{}`", self.show(t));
                    self.err("E0607", msg, obj.span);
                }
                self.check_block(body, expected)
            }
            ExprKind::Error => self.types.error,
        }
    }

    /// Checks a `#{x:spec}` spec against `x`'s static type. `Dyn`, generic, union and not yet
    /// inferred types are checked at run time instead.
    fn check_format_spec(&mut self, t: TyId, spec: &crate::ast::FormatSpec, span: Span) {
        use catallaxy_vm::fmtspec::ValueKind;
        let mut r = self.types.shallow_resolve(t);
        while let Ty::Frozen(inner) = self.types.get(r) {
            r = self.types.shallow_resolve(*inner);
        }
        let kind = if r == self.types.int {
            ValueKind::Int
        } else if r == self.types.float {
            ValueKind::Float
        } else if matches!(self.types.get(r), Ty::Class(..) | Ty::Enum(..) | Ty::Tuple(_) | Ty::Fn(..)) {
            ValueKind::Text
        } else {
            return;
        };
        if let Err(msg) = spec.check(kind, &self.show(t)) {
            self.err("E0380", format!("format spec does not apply: {msg}"), span);
        }
    }

    pub fn type_scope(&self) -> TypeScope {
        let mut sc = TypeScope::default();
        // Type parameters of the enclosing function and its owner.
        if let Some(c) = self.ctxs.iter().rev().find(|c| c.kind == CtxKind::Function) {
            let f = &self.fns[c.fn_id as usize];
            for &p in f.owner_params.iter().chain(f.type_params.iter()) {
                sc.params.insert(self.params[p as usize].name.clone(), p);
            }
            sc.self_ty = Some(f.self_ty).filter(|&t| !matches!(self.types.get(t), Ty::Error));
        }
        sc
    }

    pub fn self_local(&self) -> Option<LocalId> {
        for c in self.ctxs.iter().rev() {
            if let Some(s) = c.self_local {
                return Some(s);
            }
            if c.kind == CtxKind::Function {
                break;
            }
        }
        None
    }

    fn capture_self(&mut self) -> Option<LocalId> {
        let s = self.self_local()?;
        // Record captures through closure contexts.
        self.lookup_local("self").or(Some(s))
    }

    // ---- names -------------------------------------------------------------------------------

    fn check_ident(&mut self, e: &Expr, name: &str) -> TyId {
        if let Some(l) = self.lookup_local(name) {
            self.names.insert(e.id, NameRes::Local(l));
            self.locals[l as usize].used = true;
            if self.ctxs.iter().any(|c| c.moved.contains(&l)) {
                let msg = format!("`{name}` was moved and can no longer be used");
                self.emit(Diag::error("E0608", msg, e.span).note("reassign it before using it again"));
            }
            return self.local_ty(l);
        }
        match self.lookup_name(self.cur_module, name) {
            Some(ScopeItem::Global(g)) => {
                self.names.insert(e.id, NameRes::Global(g));
                self.ensure_global(g);
                self.globals[g as usize].ty
            }
            Some(ScopeItem::Fn(f)) => {
                self.names.insert(e.id, NameRes::Fn(f));
                self.fn_value_ty(f, e.span)
            }
            Some(ScopeItem::Variant(en, tag)) => {
                let v = self.enums[en as usize].variants[tag as usize].clone();
                if !v.fields.is_empty() {
                    let msg = format!("`{}` needs arguments: `{}(...)`", name, name);
                    self.err("E0306", msg, e.span);
                    return self.types.error;
                }
                self.calls.insert(e.id, CallRes::Variant { class: v.class_id, args: vec![] });
                let n = self.enums[en as usize].params.len();
                let args = (0..n).map(|_| self.types.fresh_var()).collect();
                self.types.intern(Ty::Enum(en, args))
            }
            Some(ScopeItem::Class(_) | ScopeItem::Enum(_) | ScopeItem::Iface(_)) => {
                let msg = format!("`{name}` is a type, not a value");
                let mut d = Diag::error("E0307", msg, e.span);
                if matches!(self.lookup_name(self.cur_module, name), Some(ScopeItem::Class(_))) {
                    d = d.fix("construct an instance", e.span.after(), ".new()");
                }
                self.emit(d);
                self.types.error
            }
            Some(ScopeItem::Module(_)) => {
                self.err(
                    "E0308",
                    format!("module `{name}` is not a value; access its items with `{name}.item`"),
                    e.span,
                );
                self.types.error
            }
            None => {
                let mut cands: Vec<String> = Vec::new();
                for c in &self.ctxs {
                    for s in &c.scopes {
                        cands.extend(s.keys().cloned());
                    }
                }
                cands.extend(self.mods[self.cur_module as usize].items.keys().cloned());
                for p in self.mods.iter().filter(|p| p.is_prelude) {
                    cands.extend(p.items.keys().cloned());
                }
                let mut d = Diag::error("E0303", format!("cannot find `{name}` in this scope"), e.span);
                if let Some(s) = crate::util::suggest(name, cands.iter().map(|s| s.as_str())) {
                    d = d.suggest(format!("did you mean `{s}`?"), e.span, s);
                }
                self.emit(d);
                self.types.error
            }
        }
    }

    /// The type of a named function used as a value.
    fn fn_value_ty(&mut self, f: FnId, span: Span) -> TyId {
        if self.fns[f as usize].has_untyped_params {
            self.err(
                "E0309",
                "a function with untyped parameters cannot be used as a value; annotate its parameters",
                span,
            );
            return self.types.error;
        }
        let m = MethodRef {
            fid: f,
            subst: FxHashMap::default(),
            self_ty: self.types.error,
            dispatch: Dispatch::Static(f),
            frozen: false,
            alts: Vec::new(),
        };
        let (ps, r) = self.instantiate_sig(&m);
        let pts = ps.iter().map(|p| p.ty).collect();
        self.types.func(pts, r)
    }

    /// Records that local `l` now holds the value of `value`: if that is another local holding an
    /// owned object, the two are aliases. Any earlier alias of `l` ends.
    fn note_alias(&mut self, l: LocalId, value: &Expr) {
        self.alias_group.remove(&l);
        let ExprKind::Ident(name) = &value.kind else { return };
        let Some(a) = self.lookup_local(name) else { return };
        if a == l || self.cap_of(self.locals[a as usize].ty) != Cap::Owned {
            return;
        }
        let g = match self.alias_group.get(&a) {
            Some(&g) => g,
            None => {
                let g = self.next_alias_group;
                self.next_alias_group += 1;
                self.alias_group.insert(a, g);
                g
            }
        };
        self.alias_group.insert(l, g);
    }

    /// `l` and every local known to alias it.
    pub fn aliases_of(&self, l: LocalId) -> Vec<LocalId> {
        match self.alias_group.get(&l) {
            Some(&g) => self.alias_group.iter().filter(|&(_, &h)| h == g).map(|(&m, _)| m).collect(),
            None => vec![l],
        }
    }

    pub fn mark_moved(&mut self, l: LocalId, span: Span) {
        // Moving a value moves it out of every variable that refers to it.
        for m in self.aliases_of(l) {
            self.mark_moved_one(m, span);
        }
    }

    fn mark_moved_one(&mut self, l: LocalId, span: Span) {
        // The move is recorded in the variable's owning context.
        let owner = self.locals[l as usize].ctx as usize;
        if self.ctxs.len() > owner {
            // Moving inside a loop that the variable outlives is an error.
            let in_loop = self.ctxs[owner..].iter().enumerate().any(|(i, c)| {
                let loops = &c.loops;
                if i == 0 {
                    // Only loops entered after the declaration would matter; be conservative only
                    // when the variable is not declared inside the loop (approximated by scope depth).
                    !loops.is_empty() && self.locals[l as usize].span.start < c.span.start
                } else {
                    !loops.is_empty()
                }
            });
            let _ = in_loop;
            self.ctxs[owner].moved.insert(l);
        }
        let _ = span;
    }

    // ---- collections -------------------------------------------------------------------------

    fn check_list(&mut self, items: &[Expr], expected: Option<TyId>) -> TyId {
        let exp_elem = expected.and_then(|t| {
            let t = self.types.shallow_resolve(t);
            let t = self.types.strip_nil(t).unwrap_or(t);
            match self.types.get(t) {
                Ty::Class(builtin::LIST, a) => Some(a[0]),
                _ => None,
            }
        });
        if let Some(et) = exp_elem {
            for x in items {
                self.check_against(x, et, "list element");
            }
            return self.types.list_of(et);
        }
        if items.is_empty() {
            let v = self.types.fresh_var();
            return self.types.list_of(v);
        }
        let tys: Vec<TyId> = items.iter().map(|x| self.check_expr(x, None)).collect();
        let et = self.join(&tys);
        // Literal ints in a list with floats become floats.
        let et = self.int_float_elem(items, &tys, et);
        self.types.list_of(et)
    }

    fn int_float_elem(&mut self, items: &[Expr], tys: &[TyId], et: TyId) -> TyId {
        let (int, float) = (self.types.int, self.types.float);
        if tys.contains(&float) && tys.iter().all(|&t| t == int || t == float) {
            for (x, &t) in items.iter().zip(tys) {
                if t == int {
                    self.coerce.insert(x.id, Coercion::IntToFloat);
                }
            }
            return float;
        }
        et
    }

    /// W0005: a bare-identifier dict key (`[src: 0]`, the string `"src"`) that names a variable
    /// in scope, which was likely meant as the key.
    fn lint_bare_key(&mut self, k: &Expr) {
        let ExprKind::Str(parts) = &k.kind else { return };
        let [StrPart::Lit(name)] = parts.as_slice() else { return };
        // A quoted key's span includes its quotes.
        if (k.span.end - k.span.start) as usize != name.len() {
            return;
        }
        let visible = {
            let mut found = false;
            for c in self.ctxs.iter().rev() {
                if c.scopes.iter().any(|s| s.contains_key(name.as_str())) {
                    found = true;
                    break;
                }
                if c.kind == CtxKind::Function {
                    break;
                }
            }
            found
        };
        if !visible {
            return;
        }
        let msg = format!("the key `{name}:` is the string \"{name}\", not the value of the variable `{name}`");
        self.emit(
            Diag::warning("W0005", msg, k.span)
                .note("a bare identifier before `:` in a dict literal is a string key")
                .suggest("use the variable's value as the key", k.span, format!("({name})"))
                .suggest("quote the key to keep the string", k.span, format!("\"{name}\"")),
        );
    }

    fn check_dict(&mut self, entries: &[(Expr, Expr)], expected: Option<TyId>) -> TyId {
        for (k, _) in entries {
            self.lint_bare_key(k);
        }
        let exp = expected.and_then(|t| {
            let t = self.types.shallow_resolve(t);
            let t = self.types.strip_nil(t).unwrap_or(t);
            match self.types.get(t) {
                Ty::Class(builtin::DICT, a) => Some((a[0], a[1])),
                _ => None,
            }
        });
        if let Some((kt, vt)) = exp {
            for (k, v) in entries {
                self.check_against(k, kt, "dict key");
                self.check_against(v, vt, "dict value");
            }
            return self.types.dict_of(kt, vt);
        }
        if entries.is_empty() {
            let k = self.types.fresh_var();
            let v = self.types.fresh_var();
            return self.types.dict_of(k, v);
        }
        let kts: Vec<TyId> = entries.iter().map(|(k, _)| self.check_expr(k, None)).collect();
        let vts: Vec<TyId> = entries.iter().map(|(_, v)| self.check_expr(v, None)).collect();
        let kt = self.join(&kts);
        let vt = self.join(&vts);
        let vals: Vec<Expr> = entries.iter().map(|(_, v)| v.clone()).collect();
        let vt = self.int_float_elem(&vals, &vts, vt);
        self.types.dict_of(kt, vt)
    }

    // ---- operators ---------------------------------------------------------------------------

    fn numeric_kind(&self, t: TyId) -> Option<ArithKind> {
        let t = self.types.shallow_resolve(t);
        if t == self.types.int {
            Some(ArithKind::Int)
        } else if t == self.types.float {
            Some(ArithKind::Float)
        } else {
            None
        }
    }

    fn is_dynamic(&self, t: TyId) -> bool {
        matches!(self.types.get(self.types.shallow_resolve(t)), Ty::Dyn | Ty::Open(_) | Ty::Error)
    }

    fn check_unary(&mut self, e: &Expr, op: UnOp, x: &Expr) -> TyId {
        match op {
            UnOp::Not => {
                let bool_ = self.types.bool;
                self.check_against(x, bool_, "`not` operand");
                bool_
            }
            UnOp::Neg | UnOp::BitNot => {
                let t = self.check_expr(x, None);
                if let Ty::Open(o) = self.types.get(self.types.shallow_resolve(t)).clone() {
                    let ret = self.new_open(o);
                    self.push_req(o, OpenReq::Unary { op, ret, span: e.span });
                    self.bins.insert(e.id, BinRes::Arith(ArithKind::Generic));
                    return ret;
                }
                match (op, self.numeric_kind(t)) {
                    (UnOp::Neg, Some(k)) => {
                        self.bins.insert(e.id, BinRes::Arith(k));
                        t
                    }
                    (UnOp::BitNot, Some(ArithKind::Int)) => {
                        self.bins.insert(e.id, BinRes::Bitwise);
                        t
                    }
                    _ if self.is_dynamic(t) => {
                        self.bins.insert(e.id, BinRes::Arith(ArithKind::Generic));
                        t
                    }
                    _ => {
                        let sym = if op == UnOp::Neg { "-" } else { "~" };
                        let msg = format!("cannot apply unary `{sym}` to `{}`", self.show(t));
                        self.err("E0312", msg, e.span);
                        self.types.error
                    }
                }
            }
        }
    }

    fn new_open(&mut self, from: OpenId) -> TyId {
        let name = self.opens[from as usize].param_name.clone();
        let o = self.opens.len() as OpenId;
        self.opens.push(OpenInfo { param_name: name, label: None, reqs: Vec::new(), uppers: Vec::new() });
        self.types.intern(Ty::Open(o))
    }

    /// Records a requirement on open `o`, naming the open that stands for its result.
    fn push_req(&mut self, o: OpenId, req: OpenReq) {
        if let Some(r) = req.result() {
            if let Ty::Open(ro) = self.types.get(self.types.shallow_resolve(r)).clone() {
                if ro != o && self.opens[ro as usize].label.is_none() {
                    let p = self.open_label(o);
                    let label = match &req {
                        OpenReq::Method { name, .. } => format!("{p}.{name}"),
                        OpenReq::BinOp { op, .. } => format!("{p} {} _", op.symbol()),
                        OpenReq::BinOpRhs { op, .. } => format!("_ {} {p}", op.symbol()),
                        OpenReq::Unary { op, .. } => format!("{}{p}", if *op == UnOp::Neg { "-" } else { "~" }),
                        OpenReq::Index { .. } => format!("{p}[_]"),
                        OpenReq::Call { .. } => format!("{p}(_)"),
                        OpenReq::Iter { .. } => format!("element of {p}"),
                        OpenReq::SetIndex { .. } => p,
                    };
                    self.opens[ro as usize].label = Some(label);
                }
            }
        }
        self.opens[o as usize].reqs.push(req);
    }

    /// How diagnostics name open `o`: the parameter, or the expression derived from it.
    pub fn open_label(&self, o: OpenId) -> String {
        let info = &self.opens[o as usize];
        info.label.clone().unwrap_or_else(|| info.param_name.clone())
    }

    fn check_binary(&mut self, e: &Expr, op: BinOp, l: &Expr, r: &Expr, expected: Option<TyId>) -> TyId {
        match op {
            BinOp::And | BinOp::Or => {
                self.check_cond(e);
                return self.types.bool;
            }
            BinOp::Eq | BinOp::Ne => {
                if self.as_nil_test(e).is_some() {
                    self.check_cond(e);
                    return self.types.bool;
                }
            }
            BinOp::Coalesce => {
                let lt = self.check_expr(l, None);
                if self.types.shallow_resolve(lt) == self.types.nil {
                    // Always nil (a literal, or a variable narrowed to `Nil`): the result is the
                    // right side.
                    let rt = self.check_expr(r, expected);
                    self.bins.insert(e.id, BinRes::Coalesce);
                    return rt;
                }
                let inner = match self.types.strip_nil(lt) {
                    Some(i) => i,
                    None => {
                        if !self.is_dynamic(lt) {
                            let msg = format!("left side of `??` has type `{}`, which is never nil", self.show(lt));
                            self.emit(Diag::warning("W0002", msg, l.span));
                        }
                        lt
                    }
                };
                let rt = self.check_expr(r, Some(expected.unwrap_or(inner)));
                self.bins.insert(e.id, BinRes::Coalesce);
                if self.trial(|c| c.assignable(rt, inner)) {
                    return inner;
                }
                if self.try_int_to_float(rt, inner) {
                    self.coerce.insert(r.id, Coercion::IntToFloat);
                    return inner;
                }
                return self.join(&[inner, rt]);
            }
            _ => {}
        }
        let lt = self.check_expr(l, None);
        let rt = self.check_expr(r, if self.numeric_kind(lt).is_some() { None } else { Some(lt) });
        self.binary_types(e, op, l, r, lt, rt)
    }

    /// Types a binary operator application and records how to evaluate it.
    pub fn binary_types(&mut self, e: &Expr, op: BinOp, l: &Expr, r: &Expr, lt: TyId, rt: TyId) -> TyId {
        let (bool_, int, float, string) = (self.types.bool, self.types.int, self.types.float, self.types.string);
        if let Ty::Open(o) = self.types.get(self.types.shallow_resolve(lt)).clone() {
            let ret = match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::In => bool_,
                _ => self.new_open(o),
            };
            if !matches!(op, BinOp::Eq | BinOp::Ne) {
                self.push_req(o, OpenReq::BinOp { op, rhs: rt, ret, span: e.span });
            }
            let res = match op {
                BinOp::Eq | BinOp::Ne => BinRes::Eq,
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Cmp => BinRes::Compare(ArithKind::Generic),
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => BinRes::Bitwise,
                BinOp::In => BinRes::Method(Dispatch::Dynamic("contains".into(), None), true),
                _ => BinRes::Arith(ArithKind::Generic),
            };
            self.bins.insert(e.id, res);
            return ret;
        }
        // An open right side with a known left side: evaluated generically, so each call site must
        // show the operator is defined for the argument's type.
        if let Ty::Open(o) = self.types.get(self.types.shallow_resolve(rt)).clone() {
            let lr = self.types.shallow_resolve(lt);
            if !matches!(self.types.get(lr), Ty::Dyn | Ty::Error) && !matches!(op, BinOp::Eq | BinOp::Ne) {
                if op == BinOp::In {
                    // `a in x` is `x.contains(a)`.
                    let ret = self.new_open(o);
                    self.push_req(
                        o,
                        OpenReq::Method {
                            name: "contains".into(),
                            args: vec![lt],
                            named: vec![None],
                            block: false,
                            ret,
                            span: e.span,
                        },
                    );
                    self.bins.insert(e.id, BinRes::Method(Dispatch::Dynamic("contains".into(), None), true));
                    return ret;
                }
                let ret = match op {
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => bool_,
                    _ => self.new_open(o),
                };
                self.push_req(o, OpenReq::BinOpRhs { op, lhs: lt, ret, span: e.span });
                let res = match op {
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Cmp => BinRes::Compare(ArithKind::Generic),
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => BinRes::Bitwise,
                    _ => BinRes::Arith(ArithKind::Generic),
                };
                self.bins.insert(e.id, res);
                return ret;
            }
        }
        let lk = self.numeric_kind(lt);
        let rk = self.numeric_kind(rt);
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                if let (Some(a), Some(b)) = (lk, rk) {
                    if a == ArithKind::Int && b == ArithKind::Int {
                        self.bins.insert(e.id, BinRes::Arith(ArithKind::Int));
                        return int;
                    }
                    if a == ArithKind::Int {
                        self.coerce.insert(l.id, Coercion::IntToFloat);
                    }
                    if b == ArithKind::Int {
                        self.coerce.insert(r.id, Coercion::IntToFloat);
                    }
                    self.bins.insert(e.id, BinRes::Arith(ArithKind::Float));
                    return float;
                }
                if op == BinOp::Add
                    && self.types.shallow_resolve(lt) == string
                    && self.types.shallow_resolve(rt) == string
                {
                    self.bins.insert(e.id, BinRes::Concat);
                    return string;
                }
                if self.is_dynamic(lt) || self.is_dynamic(rt) {
                    self.bins.insert(e.id, BinRes::Arith(ArithKind::Generic));
                    return if matches!(self.types.get(self.types.shallow_resolve(lt)), Ty::Error)
                        || matches!(self.types.get(self.types.shallow_resolve(rt)), Ty::Error)
                    {
                        self.types.error
                    } else {
                        self.types.dyn_
                    };
                }
                self.operator_method(e, op, l, r, lt, rt)
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if let (Some(a), Some(b)) = (lk, rk) {
                    let k = if a == ArithKind::Int && b == ArithKind::Int {
                        ArithKind::Int
                    } else {
                        if a == ArithKind::Int {
                            self.coerce.insert(l.id, Coercion::IntToFloat);
                        }
                        if b == ArithKind::Int {
                            self.coerce.insert(r.id, Coercion::IntToFloat);
                        }
                        ArithKind::Float
                    };
                    self.bins.insert(e.id, BinRes::Compare(k));
                    return bool_;
                }
                let (ls, rs) = (self.types.shallow_resolve(lt), self.types.shallow_resolve(rt));
                if (ls == string && rs == string) || self.is_dynamic(lt) || self.is_dynamic(rt) {
                    self.bins.insert(e.id, BinRes::Compare(ArithKind::Generic));
                    return bool_;
                }
                if self.builtin_compare(e, op, r, lt, rt) {
                    return bool_;
                }
                self.operator_method(e, op, l, r, lt, rt)
            }
            BinOp::Cmp => {
                if lk.is_some() && rk.is_some() {
                    if lk != rk {
                        if lk == Some(ArithKind::Int) {
                            self.coerce.insert(l.id, Coercion::IntToFloat);
                        } else {
                            self.coerce.insert(r.id, Coercion::IntToFloat);
                        }
                    }
                    self.bins.insert(e.id, BinRes::Compare(ArithKind::Generic));
                    return int;
                }
                let (ls, rs) = (self.types.shallow_resolve(lt), self.types.shallow_resolve(rt));
                if (ls == string && rs == string) || self.is_dynamic(lt) || self.is_dynamic(rt) {
                    self.bins.insert(e.id, BinRes::Compare(ArithKind::Generic));
                    return int;
                }
                if self.builtin_compare(e, op, r, lt, rt) {
                    return int;
                }
                self.operator_method(e, op, l, r, lt, rt)
            }
            BinOp::Eq | BinOp::Ne => {
                if let (Some(a), Some(b)) = (lk, rk) {
                    let k = if a == b { a } else { ArithKind::Generic };
                    self.bins.insert(e.id, BinRes::Compare(k));
                    return bool_;
                }
                if !self.comparable(lt, rt) {
                    let msg = format!(
                        "comparing `{}` with `{}` is always {}",
                        self.show(lt),
                        self.show(rt),
                        if op == BinOp::Eq { "false" } else { "true" }
                    );
                    self.err("E0313", msg, e.span);
                }
                self.bins.insert(e.id, BinRes::Eq);
                bool_
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                if lk == Some(ArithKind::Int) && rk == Some(ArithKind::Int) {
                    self.bins.insert(e.id, BinRes::Bitwise);
                    return int;
                }
                if self.is_dynamic(lt) || self.is_dynamic(rt) {
                    self.bins.insert(e.id, BinRes::Bitwise);
                    return int;
                }
                self.operator_method(e, op, l, r, lt, rt)
            }
            BinOp::In => {
                // `a in b` is `b.contains(a)`.
                match self.lookup_method(rt, "contains", false) {
                    Some(m) => {
                        let (ps, ret) = self.instantiate_sig(&m);
                        if let Some(p) = ps.first() {
                            self.require_assignable(l, lt, p.ty, "`in` operand");
                        }
                        self.bins.insert(e.id, BinRes::Method(m.dispatch.clone(), true));
                        ret
                    }
                    None if self.is_dynamic(rt) => {
                        self.bins.insert(e.id, BinRes::Method(Dispatch::Dynamic("contains".into(), None), true));
                        bool_
                    }
                    None => {
                        let msg = format!("`in` needs a collection with `contains`, found `{}`", self.show(rt));
                        self.err("E0314", msg, r.span);
                        bool_
                    }
                }
            }
            BinOp::And | BinOp::Or | BinOp::Coalesce => unreachable!(),
        }
    }

    /// Whether `==` between the two types can ever be true.
    fn comparable(&mut self, a: TyId, b: TyId) -> bool {
        if self.is_dynamic(a) || self.is_dynamic(b) {
            return true;
        }
        let a2 = self.types.shallow_resolve(a);
        let b2 = self.types.shallow_resolve(b);
        if matches!(self.types.get(a2), Ty::Param(_) | Ty::Iface(..) | Ty::Inter(_) | Ty::Var(_))
            || matches!(self.types.get(b2), Ty::Param(_) | Ty::Iface(..) | Ty::Inter(_) | Ty::Var(_))
        {
            return true;
        }
        self.trial(|c| c.assignable(a, b)) || self.trial(|c| c.assignable(b, a)) || {
            // Overlapping unions.
            let am: Vec<TyId> = match self.types.get(a2) {
                Ty::Union(p) => p.clone(),
                _ => vec![a2],
            };
            let bm: Vec<TyId> = match self.types.get(b2) {
                Ty::Union(p) => p.clone(),
                _ => vec![b2],
            };
            am.iter().any(|&x| bm.iter().any(|&y| x == y || self.trial(|c| c.assignable(x, y) || c.assignable(y, x))))
        }
    }

    /// `<`, `<=>`, ... on tuples and lists of comparable elements, which compare
    /// lexicographically at run time. Returns false (nothing recorded) for other types.
    fn builtin_compare(&mut self, e: &Expr, op: BinOp, r: &Expr, lt: TyId, rt: TyId) -> bool {
        let lt_r = self.types.shallow_resolve(lt);
        let is_seq = |c: &Self, t: TyId| match c.types.get(t) {
            Ty::Tuple(_) | Ty::Class(builtin::LIST, _) => true,
            Ty::Frozen(i) => {
                matches!(c.types.get(c.types.shallow_resolve(*i)), Ty::Tuple(_) | Ty::Class(builtin::LIST, _))
            }
            _ => false,
        };
        if !is_seq(self, lt_r) {
            return false;
        }
        if self.builtin_comparable(lt_r) != Some(true) {
            let msg = format!(
                "operator `{}` is not defined for `{}`: its elements are not `Comparable`",
                op.symbol(),
                self.show(lt)
            );
            self.err("E0316", msg, e.span);
        } else if !self.trial(|c| c.assignable(rt, lt)) && !self.trial(|c| c.assignable(lt, rt)) && {
            // Comparison only reads: a frozen side compares with an owned one.
            let (fl, fr) = (self.freeze_ty(lt), self.freeze_ty(rt));
            !self.trial(|c| c.assignable(fr, fl)) && !self.assignable(fl, fr)
        } {
            let msg = format!(
                "`{}` {} `{}`: right side must be `{}`",
                self.show(lt),
                op.symbol(),
                self.show(rt),
                self.show(lt)
            );
            self.err("E0315", msg, r.span);
        }
        self.bins.insert(e.id, BinRes::Compare(ArithKind::Generic));
        true
    }

    /// An operator implemented by a method (`+` on a class, `<` from Comparable, ...).
    fn operator_method(&mut self, e: &Expr, op: BinOp, l: &Expr, r: &Expr, lt: TyId, rt: TyId) -> TyId {
        let name = op.symbol();
        let lt_r = self.types.shallow_resolve(lt);
        if self.types.is_nilable(lt_r) && !matches!(self.types.get(lt_r), Ty::Class(builtin::NIL, _)) {
            let msg = format!("left side of `{name}` may be nil (type `{}`)", self.show(lt));
            let mut d = Diag::error("E0310", msg, l.span).fix("assert it is not nil", l.span.after(), "!");
            if let Some(n) = self.loop_dropped_note(l) {
                d = d.note(n);
            }
            self.emit(d);
            return self.types.error;
        }
        match self.lookup_method(lt, name, false) {
            Some(m) => {
                let (ps, ret) = self.instantiate_sig(&m);
                if let Some(p) = ps.first() {
                    let pt = p.ty;
                    if !self.assignable(rt, pt) {
                        if self.try_int_to_float(rt, pt) {
                            self.coerce.insert(r.id, Coercion::IntToFloat);
                        } else {
                            let msg = format!(
                                "`{}` {name} `{}`: right side must be `{}`",
                                self.show(lt),
                                self.show(rt),
                                self.show(pt)
                            );
                            self.err("E0315", msg, r.span);
                        }
                    }
                }
                self.bins.insert(e.id, BinRes::Method(m.dispatch.clone(), false));
                self.note_self_call(&m, l);
                ret
            }
            None => {
                let msg = format!("operator `{name}` is not defined for `{}` and `{}`", self.show(lt), self.show(rt));
                let mut d = Diag::error("E0316", msg, e.span);
                if matches!(op, BinOp::Add) && self.types.shallow_resolve(lt) == self.types.string {
                    d = d.note("use string interpolation to combine text and values: \"#{a}#{b}\"");
                }
                self.emit(d);
                self.types.error
            }
        }
    }

    /// Recognizes `x == nil` / `x != nil` (either order) on a local.
    fn as_nil_test(&self, e: &Expr) -> Option<()> {
        if let ExprKind::Binary(BinOp::Eq | BinOp::Ne, l, r) = &e.kind {
            if matches!(r.kind, ExprKind::Nil) || matches!(l.kind, ExprKind::Nil) {
                return Some(());
            }
        }
        None
    }

    /// Checks a Bool condition and computes the narrowings it implies.
    pub fn check_cond(&mut self, e: &Expr) -> Narrowings {
        let bool_ = self.types.bool;

        match &e.kind {
            ExprKind::Unary(UnOp::Not, x) => {
                let (t, f) = self.check_cond(x);
                self.record_ty(e.id, bool_);
                (f, t)
            }
            ExprKind::Binary(op @ (BinOp::And | BinOp::Or), l, r) => {
                let (mut lt, mut lf) = self.check_cond(l);
                self.push_scope();
                for (id, t) in if *op == BinOp::And { lt.clone() } else { lf.clone() } {
                    self.narrow(id, t);
                }
                let epoch = self.path_epoch;
                let ran = self.runner_log.len();
                let (rt, rf) = self.check_cond(r);
                self.pop_scope();
                if self.path_epoch != epoch {
                    // A call on the right may have changed what the left found.
                    self.retain_past_calls(&mut lt);
                    self.retain_past_calls(&mut lf);
                }
                if self.runner_log.len() > ran {
                    // So may a contained closure called on the right.
                    let called: Vec<Span> = self.runner_log[ran..].to_vec();
                    let hit = |c: &Self, l: LocalId| c.locals[l as usize].runners.iter().any(|r| called.contains(&r.0));
                    let keep = |c: &Self, k: &NKey| match k {
                        NKey::Local(l) => !hit(c, *l),
                        NKey::Path(p, _) => !hit(c, p.0),
                    };
                    lt.retain(|(k, _)| keep(self, k));
                    lf.retain(|(k, _)| keep(self, k));
                }
                self.bins.insert(e.id, BinRes::Logic);
                self.record_ty(e.id, bool_);
                if *op == BinOp::And {
                    (lt.into_iter().chain(rt).collect(), Vec::new())
                } else {
                    (Vec::new(), lf.into_iter().chain(rf).collect())
                }
            }
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r)
                if matches!(r.kind, ExprKind::Nil) || matches!(l.kind, ExprKind::Nil) =>
            {
                let x = if matches!(r.kind, ExprKind::Nil) { l } else { r };
                let nil_side = if matches!(r.kind, ExprKind::Nil) { r } else { l };
                let t = self.check_expr(x, None);
                self.record_ty(nil_side.id, self.types.nil);
                self.bins.insert(e.id, BinRes::Eq);
                self.record_ty(e.id, bool_);
                let mut nonnil = Vec::new();
                let mut isnil = Vec::new();
                let key = match self.narrowable_local(x) {
                    Some(l) => Some(NKey::Local(l)),
                    None => self.field_path(x).map(|(p, stable)| NKey::Path(p, stable)),
                };
                if let Some(key) = key {
                    if let Some(inner) = self.types.strip_nil(t) {
                        nonnil.push((key.clone(), inner));
                        isnil.push((key, self.types.nil));
                    }
                }
                if *op == BinOp::Ne {
                    (nonnil, isnil)
                } else {
                    (isnil, nonnil)
                }
            }
            ExprKind::Is { expr, ty } => {
                let t = self.check_expr(e, None);
                let _ = t;
                let mut tn = Vec::new();
                let mut fne = Vec::new();
                if let Some(l) = self.narrowable_local(expr) {
                    self.suppress_diags += 1;
                    let target = self.resolve_test_type(ty);
                    self.suppress_diags -= 1;
                    let cur = self.local_ty(l);
                    let (yes, no, _) = self.split_by_test(cur, target);
                    tn.push((l.into(), yes));
                    if let Some(no) = no {
                        fne.push((l.into(), no));
                    }
                }
                (tn, fne)
            }
            _ => {
                self.check_against(e, bool_, "condition");
                (Vec::new(), Vec::new())
            }
        }
    }

    /// The field path `e` reads (`n.next`, `self.head.next`) when it is a plain field read on a
    /// local or `self`, with whether it is stable (see `NKey::Path`). Fields of shared classes,
    /// which other threads may change, are never paths.
    pub fn field_path(&self, e: &Expr) -> Option<(FieldPath, bool)> {
        let ExprKind::Member { recv, name, args, block: None, parens: false, safe: false } = &e.kind else {
            return None;
        };
        if !args.is_empty() || !matches!(self.calls.get(&e.id), Some(CallRes::Field { safe: false, .. })) {
            return None;
        }
        let ((root, mut fields), stable) = match &recv.kind {
            ExprKind::Ident(_) => ((self.narrowable_local(recv)?, Vec::new()), true),
            ExprKind::SelfValue => ((self.ctxs.last()?.self_local?, Vec::new()), true),
            _ => self.field_path(recv)?,
        };
        let rt = self.types.shallow_resolve(self.ty_of(recv.id));
        let frozen_here = match self.types.get(rt) {
            Ty::Frozen(_) => true,
            Ty::Class(c, _) => match self.classes.get(c).map(|ci| ci.modifier) {
                Some(ast::ClassModifier::Shared) => return None,
                Some(ast::ClassModifier::Frozen) => true,
                _ => false,
            },
            _ => false,
        };
        fields.push(name.name.clone());
        Some(((root, fields), stable && frozen_here))
    }

    /// Keeps the narrowings that still hold after a call (see `clear_unstable_paths`).
    fn retain_past_calls(&self, ns: &mut Vec<(NKey, TyId)>) {
        ns.retain(|(k, _)| match k {
            NKey::Path(p, stable) => *stable && !self.dies_at_calls(p.0),
            NKey::Local(l) => !self.dies_at_calls(*l),
        });
    }

    fn narrowable_local(&self, e: &Expr) -> Option<LocalId> {
        match (&e.kind, self.names.get(&e.id)) {
            (ExprKind::Ident(_), Some(NameRes::Local(l))) => Some(*l),
            (ExprKind::SelfValue, _) => None,
            _ => None,
        }
    }

    // ---- control flow ------------------------------------------------------------------------

    fn check_if(&mut self, cond: &Expr, then: &Block, else_: Option<&Expr>, expected: Option<TyId>) -> TyId {
        let (tn, fne) = self.check_cond(cond);
        // Each branch starts from the narrowings here. After the `if`, what every branch that
        // falls through ends with holds (`joined_narrowing`); this also makes guard clauses
        // (`if x == nil { return }`) narrow the rest of the block.
        let start = self.save_narrowing();
        let mut ends = Vec::new();
        let (tt, end) = self.check_branch(tn, |c| {
            c.capture_block_end = true;
            c.check_block(then, expected)
        });
        if !self.is_never(tt) {
            ends.push(end);
        }
        self.restore_narrowing(start.clone());
        let et = match else_ {
            Some(x) => {
                let (t, end) = self.check_branch(fne, |c| c.check_branch_expr(x, expected));
                if !self.is_never(t) {
                    ends.push(end);
                }
                Some(t)
            }
            None => {
                let mut end = Self::flat_narrowing(&start);
                for (k, t) in fne {
                    match k {
                        NKey::Local(l) => {
                            end.0.insert(l, t);
                        }
                        NKey::Path(p, stable) => {
                            end.1.insert(p, (t, stable));
                        }
                    }
                }
                ends.push(end);
                None
            }
        };
        let joined = self.joined_narrowing(&start, &ends);
        self.restore_narrowing(joined);
        match et {
            None => {
                if self.is_never(tt) || self.types.shallow_resolve(tt) == self.types.nil {
                    self.types.nil
                } else {
                    self.types.optional(tt)
                }
            }
            Some(et) => {
                if let Some(exp) = expected {
                    if self.trial(|c| c.assignable(tt, exp) && c.assignable(et, exp)) {
                        return exp;
                    }
                }
                self.join(&[tt, et])
            }
        }
    }

    fn check_if_let(
        &mut self,
        pattern: &ast::Pattern,
        value: &Expr,
        then: &Block,
        else_: Option<&Expr>,
        expected: Option<TyId>,
    ) -> TyId {
        let vt = self.check_expr(value, None);
        let start = self.save_narrowing();
        let mut ends = Vec::new();
        let (tt, end) = self.check_branch(Vec::new(), |c| {
            // A plain binding unwraps an optional.
            let bind_ty = match &pattern.kind {
                ast::PatternKind::Bind(_) => match c.types.strip_nil(vt) {
                    Some(inner) => inner,
                    None => {
                        if !c.is_dynamic(vt) {
                            let msg = format!("`if let` needs an optional value, found `{}`", c.show(vt));
                            c.err("E0317", msg, value.span);
                        }
                        vt
                    }
                },
                _ => vt,
            };
            c.check_pattern(pattern, bind_ty, false);
            c.capture_block_end = true;
            c.check_block(then, expected)
        });
        if !self.is_never(tt) {
            ends.push(end);
        }
        self.restore_narrowing(start.clone());
        let et = match else_ {
            Some(x) => {
                let (t, end) = self.check_branch(Vec::new(), |c| c.check_branch_expr(x, expected));
                if !self.is_never(t) {
                    ends.push(end);
                }
                Some(t)
            }
            None => {
                ends.push(Self::flat_narrowing(&start));
                None
            }
        };
        let joined = self.joined_narrowing(&start, &ends);
        self.restore_narrowing(joined);
        match et {
            None => {
                if self.is_never(tt) || self.types.shallow_resolve(tt) == self.types.nil {
                    self.types.nil
                } else {
                    self.types.optional(tt)
                }
            }
            Some(et) => self.join(&[tt, et]),
        }
    }

    /// The innermost context that is a real function or closure (the target of `return`).
    fn return_ctx(&self) -> usize {
        for i in (0..self.ctxs.len()).rev() {
            if self.ctxs[i].kind != CtxKind::ForBody {
                return i;
            }
        }
        0
    }

    fn frame_local_of(&mut self, ci: usize) -> LocalId {
        if let Some(l) = self.ctxs[ci].frame_local {
            return l;
        }
        let fid = self.ctxs[ci].fn_id;
        let int = self.types.int;
        self.locals.push(LocalInfo {
            name: "$frame".into(),
            ty: int,
            ctx: ci as u32,
            captured: false,
            assigns: 1,
            closure_assigned: false,
            runners: Vec::new(),
            reassigned: false,
            is_param: false,
            span: Span::default(),
            used: true,
        });
        let id = self.locals.len() as LocalId - 1;
        self.fn_locals.entry(fid).or_default().push(id);
        self.ctxs[ci].frame_local = Some(id);
        self.frame_locals.insert(fid, id);
        id
    }

    /// Makes `l` (owned by context `owner`) visible in every context above it.
    fn capture_through(&mut self, l: LocalId, owner: usize) {
        let n = self.ctxs.len();
        for cj in owner + 1..n {
            if !self.ctxs[cj].captures.contains(&l) {
                self.ctxs[cj].captures.push(l);
            }
        }
        if owner + 1 < n {
            self.locals[l as usize].captured = true;
        }
    }

    fn check_return(&mut self, e: &Expr, v: Option<&Expr>) {
        let ci = self.return_ctx();
        let ret = self.ctxs[ci].ret;
        let ret_known = !matches!(self.types.get(self.types.shallow_resolve(ret)), Ty::Var(_));
        let t = match v {
            Some(x) => {
                if ret_known {
                    self.check_against(x, ret, "return value")
                } else {
                    self.check_expr(x, None)
                }
            }
            None => {
                let nil = self.types.nil;
                if ret_known && !self.assignable(nil, ret) {
                    let msg = format!("`return` without a value in a function returning `{}`", self.show(ret));
                    self.err("E0318", msg, e.span);
                }
                nil
            }
        };
        if !ret_known {
            self.ctxs[ci].returns.push(t);
        }
        if ci + 1 < self.ctxs.len() {
            // Inside a for-body block: unwind to the function's frame.
            let fl = self.frame_local_of(ci);
            self.capture_through(fl, ci);
            self.control.insert(e.id, Control::ReturnUnwind(fl));
        }
    }

    fn check_break(&mut self, e: &Expr, v: Option<&Expr>) {
        let t = match v {
            Some(x) => self.check_expr(x, None),
            None => self.types.nil,
        };
        let n = self.ctxs.len();
        let c = &mut self.ctxs[n - 1];
        match c.loops.last_mut() {
            Some(lp) if lp.is_foreach => {
                lp.breaks.push(t);
                // Unwind to the frame that called `each`.
                let parent = n - 2;
                let fl = self.frame_local_of(parent);
                self.capture_through(fl, parent);
                self.control.insert(e.id, Control::BreakUnwind(fl));
            }
            Some(lp) => lp.breaks.push(t),
            None => self.err("E0319", "`break` outside of a loop", e.span),
        }
    }

    fn check_continue(&mut self, e: &Expr) {
        let c = self.ctx();
        match c.loops.last() {
            Some(lp) if lp.is_foreach => {
                self.control.insert(e.id, Control::ContinueReturn);
            }
            Some(_) => {}
            None => self.err("E0319", "`continue` outside of a loop", e.span),
        }
    }

    fn check_try(&mut self, e: &Expr, x: &Expr) -> TyId {
        let t = self.check_expr(x, None);
        let ci = self.return_ctx();
        let ret = self.types.shallow_resolve(self.ctxs[ci].ret);
        let tr = self.types.shallow_resolve(t);
        if let Ty::Open(o) = self.types.get(tr).clone() {
            let pname = self.opens[o as usize].param_name.clone();
            let msg = format!("`?` on a value derived from untyped parameter `{pname}`; annotate its type");
            self.err("E0369", msg, e.span);
            return self.types.error;
        }
        if let Ty::Enum(eid, args) = self.types.get(tr).clone() {
            if Some(eid) == self.well_known.result {
                let (ok_t, err_t) = (args[0], args[1]);
                // The enclosing function must return a Result with a compatible error type.
                let ret_ok = match self.types.get(ret).clone() {
                    Ty::Enum(e2, a2) if Some(e2) == self.well_known.result => {
                        if !self.assignable(err_t, a2[1]) {
                            let msg = format!(
                                "`?` propagates `{}`, but the function's error type is `{}`",
                                self.show(err_t),
                                self.show(a2[1])
                            );
                            self.err("E0320", msg, e.span);
                        }
                        true
                    }
                    Ty::Var(_) => {
                        let v = self.types.fresh_var();
                        let rt = self.types.intern(Ty::Enum(eid, vec![v, err_t]));
                        self.ctxs[ci].returns.push(rt);
                        true
                    }
                    _ => false,
                };
                if !ret_ok {
                    let msg = format!(
                        "`?` on a Result requires the function to return a `Result`, but it returns `{}`",
                        self.show(ret)
                    );
                    self.err("E0321", msg, e.span);
                }
                self.calls.insert(e.id, CallRes::None);
                self.mark_try_unwind(e, ci);
                return ok_t;
            }
        }
        if let Some(inner) = self.types.strip_nil(tr) {
            if !self.types.is_nilable(ret) && !matches!(self.types.get(ret), Ty::Var(_)) {
                let msg = format!(
                    "`?` on an optional requires the function to return an optional, but it returns `{}`",
                    self.show(ret)
                );
                let mut d = Diag::error("E0321", msg, e.span);
                if matches!(self.types.get(ret), Ty::Enum(r, _) if Some(*r) == self.well_known.result) {
                    d = d.note("turn the missing value into an error with `.ok_or(error)?`").suggest(
                        "convert it to a Result",
                        x.span.after(),
                        ".ok_or(<error>)",
                    );
                }
                self.emit(d);
            }
            if matches!(self.types.get(ret), Ty::Var(_)) {
                let nil = self.types.nil;
                self.ctxs[ci].returns.push(nil);
            }
            self.mark_try_unwind(e, ci);
            return inner;
        }
        if !self.is_dynamic(t) {
            let msg = format!("`?` needs a `Result` or optional value, found `{}`", self.show(t));
            self.err("E0322", msg, e.span);
        }
        self.types.error
    }

    fn mark_try_unwind(&mut self, e: &Expr, ci: usize) {
        if ci + 1 < self.ctxs.len() {
            let fl = self.frame_local_of(ci);
            self.capture_through(fl, ci);
            self.control.insert(e.id, Control::ReturnUnwind(fl));
        }
    }

    // ---- assignment --------------------------------------------------------------------------

    /// `x = { ... }` runs the block now and stores its value; a block with no value (it ends in
    /// a statement such as `count += 1`) was almost certainly meant to be a closure.
    fn check_block_value(&mut self, value: &Expr, vt: TyId) {
        if matches!(value.kind, ExprKind::Block(_)) && self.types.shallow_resolve(vt) == self.types.nil {
            let msg = "this block runs now and its value is `nil`".to_string();
            let open = Span::new(value.span.file, value.span.start + 1, value.span.start + 1);
            self.emit(
                Diag::error("E0372", msg, value.span)
                    .note("to store code to run later, write a closure: `{ || ... }` or `fn() { ... }`")
                    .suggest("make it a closure", open, " ||"),
            );
        }
    }

    fn check_assign(&mut self, e: &Expr, target: &Expr, op: Option<BinOp>, value: &Expr, annot: Option<&TypeExpr>) {
        match &target.kind {
            ExprKind::Ident(name) => {
                if let Some(te) = annot {
                    let ty = self.resolve_type(te, &mut self.type_scope());
                    if self.ctx().scopes.last().unwrap().contains_key(name) {
                        let msg = format!("`{name}` is already declared in this scope");
                        self.err("E0323", msg, target.span);
                    }
                    // Declared before the value is checked so closures can refer to themselves.
                    let is_closure = matches!(value.kind, ExprKind::Closure { .. });
                    let l = if is_closure {
                        let l = self.declare_local(name, ty, target.span);
                        self.locals[l as usize].assigns = 2;
                        self.check_against(value, ty, "assignment");
                        l
                    } else {
                        self.check_against(value, ty, "assignment");
                        self.declare_local(name, ty, target.span)
                    };
                    self.decl_locals.insert(e.id, l);
                    self.names.insert(target.id, NameRes::Local(l));
                    self.assigns.insert(e.id, AssignRes::Local(l));
                    return;
                }
                if let Some(l) = self.lookup_local(name) {
                    self.names.insert(target.id, NameRes::Local(l));
                    // Assigning a captured variable from a parallel block is not allowed.
                    let owner = self.locals[l as usize].ctx as usize;
                    if self.ctxs[owner + 1..].iter().any(|c| matches!(c.kind, CtxKind::Spawn { .. })) {
                        let msg = format!("cannot assign to captured variable `{name}` inside a parallel block");
                        self.emit(
                            Diag::error("E0609", msg, target.span)
                                .note("parallel blocks receive copies of captured variables"),
                        );
                    }
                    let declared = self.locals[l as usize].ty;
                    let vt = match op {
                        None => self.check_expr(value, Some(declared)),
                        // The read half sees the narrowed type, as a plain read would.
                        Some(o) => {
                            let cur = self.local_ty(l);
                            self.compound_rhs(e, o, target, value, cur)
                        }
                    };
                    if op.is_none() {
                        self.check_block_value(value, vt);
                    }
                    if !self.assignable(vt, declared) {
                        if self.try_int_to_float(vt, declared) {
                            self.coerce.insert(value.id, Coercion::IntToFloat);
                        } else {
                            let msg = format!(
                                "cannot assign `{}` to `{name}`, which has type `{}`",
                                self.show(vt),
                                self.show(declared)
                            );
                            let mut d = Diag::error("E0324", msg, value.span);
                            if op.is_none() {
                                d = d.note(format!(
                                    "a variable keeps the type of its first assignment; declare it with a wider type, e.g. `{name}: {} = ...`",
                                    self.show_join(declared, vt)
                                ));
                            }
                            self.emit(d);
                        }
                    }
                    if !self.loop_facts.is_empty() {
                        let stored = match self.coerce.get(&value.id) {
                            Some(Coercion::IntToFloat) => self.types.float,
                            None => vt,
                        };
                        self.check_loop_facts(l, stored, value);
                    }
                    self.locals[l as usize].assigns += 1;
                    // From a closure or a `for` body over a user type. The pre-scan has seen the
                    // closures: those that run only at direct calls of their local keep it so.
                    let info = &self.locals[l as usize];
                    let contained = !info.runners.is_empty()
                        && self.ctxs[owner + 1..].iter().all(|c| matches!(c.kind, CtxKind::Closure));
                    if owner + 1 < self.ctxs.len() && !contained {
                        self.locals[l as usize].closure_assigned = true;
                    }
                    for c in self.ctxs.iter_mut() {
                        c.moved.remove(&l);
                    }
                    self.clear_narrowing(l);
                    if op.is_none() {
                        self.note_alias(l, value);
                    } else {
                        self.alias_group.remove(&l);
                    }
                    let declared_r = self.types.shallow_resolve(declared);
                    if self.types.is_nilable(declared_r)
                        && !self.types.is_nilable(vt)
                        && !self.is_never(vt)
                        && !self.types.has_vars(vt)
                    {
                        self.narrow(l, vt);
                    }
                    self.assigns.insert(e.id, AssignRes::Local(l));
                    return;
                }
                if let Some(ScopeItem::Global(g)) = self.lookup_name(self.cur_module, name) {
                    self.ensure_global(g);
                    self.names.insert(target.id, NameRes::Global(g));
                    if self.globals[g as usize].is_const {
                        let msg = format!("cannot assign to constant `{name}`");
                        self.err("E0325", msg, target.span);
                    }
                    let ty = self.globals[g as usize].ty;
                    match op {
                        None => {
                            self.check_against(value, ty, "assignment");
                        }
                        Some(o) => {
                            let vt = self.compound_rhs(e, o, target, value, ty);
                            self.require_assignable(value, vt, ty, "assignment");
                        }
                    }
                    self.assigns.insert(e.id, AssignRes::Global(g));
                    return;
                }
                if op.is_some() {
                    let msg = format!("cannot find `{name}` in this scope");
                    self.err("E0303", msg, target.span);
                    return;
                }
                // First assignment declares a local.
                let vt = self.check_expr(value, None);
                self.check_block_value(value, vt);
                let ty = if self.types.shallow_resolve(vt) == self.types.nil {
                    let v = self.types.fresh_var();
                    self.types.optional(v)
                } else if self.is_never(vt) {
                    let msg = format!("`{name}` is assigned a value that never exists");
                    self.err("E0326", msg, value.span);
                    self.types.error
                } else {
                    vt
                };
                let l = self.declare_local(name, ty, target.span);
                self.decl_locals.insert(e.id, l);
                self.names.insert(target.id, NameRes::Local(l));
                self.assigns.insert(e.id, AssignRes::Local(l));
                self.note_alias(l, value);
            }
            ExprKind::Member { recv, name, .. } => {
                let rt = self.check_expr(recv, None);
                if let Ty::Open(o) = self.types.get(self.types.shallow_resolve(rt)).clone() {
                    let pname = self.opens[o as usize].param_name.clone();
                    let msg = format!(
                        "cannot assign field `{}` through untyped parameter `{pname}`; annotate its type",
                        name.name
                    );
                    self.err("E0369", msg, target.span);
                    self.check_expr(value, None);
                    return;
                }
                if matches!(self.types.get(self.types.shallow_resolve(rt)), Ty::Frozen(_)) {
                    let msg = format!("cannot assign field `{}` of a frozen value", name.name);
                    self.err("E0610", msg, target.span);
                }
                if let Ty::Class(cid, _) = self.types.get(self.types.shallow_resolve(rt)).clone() {
                    if self.classes.get(&cid).is_some_and(|c| c.modifier == ast::ClassModifier::Frozen)
                        && !self.in_constructor_of(cid)
                    {
                        let msg = format!("cannot assign field `{}`: `{}` is a frozen class", name.name, self.show(rt));
                        self.err("E0611", msg, target.span);
                    }
                }
                match self.lookup_field(rt, &name.name) {
                    Some((idx, fty)) => {
                        let vt = match op {
                            None => self.check_expr(value, Some(fty)),
                            Some(o) => self.compound_rhs(e, o, target, value, fty),
                        };
                        self.require_assignable(value, vt, fty, "field assignment");
                        self.assigns.insert(e.id, AssignRes::Field(idx));
                        self.calls.insert(target.id, CallRes::Field { index: idx, safe: false });
                        self.record_ty(target.id, fty);
                        // Any object's field of this name may be the one assigned (aliases).
                        self.clear_paths(|p, _| p.1.contains(&name.name));
                        if let Some((p, stable)) = self.field_path(target) {
                            let fty_r = self.types.shallow_resolve(fty);
                            if self.types.is_nilable(fty_r)
                                && !self.types.is_nilable(vt)
                                && !self.is_never(vt)
                                && !self.types.has_vars(vt)
                            {
                                self.narrow(NKey::Path(p, stable), vt);
                            }
                        }
                        if matches!(recv.kind, ExprKind::SelfValue) {
                            let fid = self.ctxs[self.return_ctx()].fn_id;
                            self.fns[fid as usize].mutating = true;
                        }
                    }
                    None => {
                        if self.is_dynamic(rt) {
                            self.check_expr(value, None);
                            self.assigns.insert(e.id, AssignRes::IndexGeneric);
                            return;
                        }
                        let msg = format!("`{}` has no field `{}`", self.show(rt), name.name);
                        self.err("E0327", msg, name.span);
                        self.check_expr(value, None);
                    }
                }
            }
            ExprKind::Index { recv, args } => {
                let rt = self.check_expr(recv, None);
                if args.len() != 1 {
                    self.err("E0328", "index assignment takes one index", target.span);
                    return;
                }
                let it = self.check_expr(&args[0], None);
                if let Ty::Open(o) = self.types.get(self.types.shallow_resolve(rt)).clone() {
                    let vt = self.check_expr(value, None);
                    if let Some(op) = op {
                        let res = match op {
                            BinOp::Or | BinOp::And => BinRes::Logic,
                            BinOp::Coalesce => BinRes::Coalesce,
                            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => BinRes::Bitwise,
                            _ => BinRes::Arith(ArithKind::Generic),
                        };
                        self.bins.insert(e.id, res);
                    }
                    self.push_req(o, OpenReq::SetIndex { index: it, value: vt, op, span: e.span });
                    self.assigns.insert(e.id, AssignRes::IndexGeneric);
                    return;
                }
                if matches!(self.types.get(self.types.shallow_resolve(rt)), Ty::Frozen(_)) {
                    self.err("E0610", "cannot modify a frozen collection", target.span);
                }
                if self.is_dynamic(rt) {
                    self.check_expr(value, None);
                    self.assigns.insert(e.id, AssignRes::IndexGeneric);
                    return;
                }
                match self.lookup_method(rt, "[]=", false) {
                    Some(m) => {
                        let (ps, _) = self.instantiate_sig(&m);
                        if ps.len() == 2 {
                            self.require_assignable(&args[0], it, ps[0].ty, "index");
                            let vt = match op {
                                None => self.check_expr(value, Some(ps[1].ty)),
                                Some(o) => {
                                    // a[i] op= v reads a[i] first.
                                    let elem = self.index_result_ty(rt, it, target.span);
                                    self.compound_rhs(e, o, target, value, elem)
                                }
                            };
                            self.require_assignable(value, vt, ps[1].ty, "element assignment");
                        }
                        let rtr = self.types.shallow_resolve(rt);
                        let generic = matches!(self.types.get(rtr), Ty::Class(builtin::LIST | builtin::DICT, _));
                        self.assigns.insert(
                            e.id,
                            if generic { AssignRes::IndexGeneric } else { AssignRes::Index(m.dispatch.clone()) },
                        );
                        self.note_self_call(&m, recv);
                    }
                    None => {
                        let msg = format!("`{}` does not support index assignment", self.show(rt));
                        self.err("E0329", msg, target.span);
                    }
                }
            }
            _ => self.err("E0105", "invalid assignment target", target.span),
        }
    }

    fn show_join(&mut self, a: TyId, b: TyId) -> String {
        let j = self.types.union(vec![a, b]);
        self.show(j)
    }

    fn in_constructor_of(&self, cid: ClassId) -> bool {
        self.ctxs.iter().any(|c| {
            let f = &self.fns[c.fn_id as usize];
            f.name == "init" && f.owner == Owner::Class(cid) || matches!(f.synth, Synth::Constructor(x) if x == cid)
        })
    }

    /// The right-hand side of `target op= value`, typed as `target op value`.
    fn compound_rhs(&mut self, e: &Expr, op: BinOp, target: &Expr, value: &Expr, target_ty: TyId) -> TyId {
        match op {
            BinOp::Or | BinOp::And => {
                let b = self.types.bool;
                self.check_against(value, b, "logical assignment");
                self.bins.insert(e.id, BinRes::Logic);
                b
            }
            BinOp::Coalesce => {
                // `x ??= v` leaves `x` non-nil: the result (what `x` holds afterwards) is the
                // unwrapped type, so the target is narrowed like after `x = v`.
                let inner = self.types.strip_nil(target_ty).unwrap_or(target_ty);
                self.check_against(value, inner, "`??=`");
                self.bins.insert(e.id, BinRes::Coalesce);
                inner
            }
            _ => {
                let vt = self.check_expr(value, None);
                // Record the target's type for the read half.
                self.record_ty(target.id, target_ty);
                let lt = target_ty;
                let lt_inner = self.types.shallow_resolve(lt);
                self.binary_types(e, op, target, value, lt_inner, vt)
            }
        }
    }

    // ---- calls -------------------------------------------------------------------------------

    /// Interprets an expression as a static path (type, module, function), if it is one.
    pub fn resolve_path(&mut self, e: &Expr) -> Option<PathRes> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if self.ctxs.iter().any(|c| c.scopes.iter().any(|s| s.contains_key(name))) {
                    return None;
                }
                if name == "Self" {
                    let sc = self.type_scope();
                    if let Some(st) = sc.self_ty {
                        return match self.types.get(st).clone() {
                            Ty::Class(c, a) => Some(PathRes::Class(c, Some(a))),
                            Ty::Enum(en, a) => Some(PathRes::Enum(en, Some(a))),
                            _ => None,
                        };
                    }
                }
                let item = self.lookup_name(self.cur_module, name)?;
                // Record type and module names used as path prefixes for tooling (hover,
                // references); values (functions, globals) are recorded where they are checked.
                let res = match item {
                    ScopeItem::Class(c) => Some(NameRes::Class(c)),
                    ScopeItem::Enum(en) => Some(NameRes::Enum(en)),
                    ScopeItem::Iface(i) => Some(NameRes::Iface(i)),
                    ScopeItem::Module(m) => Some(NameRes::Module(m)),
                    ScopeItem::Variant(en, t) => Some(NameRes::Variant(en, t)),
                    ScopeItem::Fn(_) | ScopeItem::Global(_) => None,
                };
                if let Some(r) = res {
                    if e.id < u32::MAX - 2 {
                        self.names.insert(e.id, r);
                    }
                }
                match item {
                    ScopeItem::Class(c) => Some(PathRes::Class(c, None)),
                    ScopeItem::Enum(en) => Some(PathRes::Enum(en, None)),
                    ScopeItem::Iface(i) => Some(PathRes::Iface(i)),
                    ScopeItem::Module(m) => Some(PathRes::Module(m)),
                    ScopeItem::Fn(f) => Some(PathRes::Fn(f)),
                    ScopeItem::Global(g) => Some(PathRes::Global(g)),
                    ScopeItem::Variant(en, t) => Some(PathRes::Variant(en, t, None)),
                }
            }
            ExprKind::Member { recv, name, args, block: None, parens: false, safe: false } if args.is_empty() => {
                match self.resolve_path(recv)? {
                    PathRes::Module(m) => {
                        let si = self.mods[m as usize].items.get(&name.name).copied()?;
                        if !self.item_is_pub(si) {
                            let msg = format!("`{}` is private to module `{}`", name.name, self.mods[m as usize].name);
                            self.err("E0202", msg, name.span);
                        }
                        // Record `module.Type` for tooling, like a bare type name above.
                        let res = match si {
                            ScopeItem::Class(c) => Some(NameRes::Class(c)),
                            ScopeItem::Enum(en) => Some(NameRes::Enum(en)),
                            ScopeItem::Iface(i) => Some(NameRes::Iface(i)),
                            _ => None,
                        };
                        if let Some(r) = res {
                            if e.id < u32::MAX - 2 {
                                self.names.insert(e.id, r);
                            }
                        }
                        Some(match si {
                            ScopeItem::Class(c) => PathRes::Class(c, None),
                            ScopeItem::Enum(en) => PathRes::Enum(en, None),
                            ScopeItem::Iface(i) => PathRes::Iface(i),
                            ScopeItem::Module(m) => PathRes::Module(m),
                            ScopeItem::Fn(f) => PathRes::Fn(f),
                            ScopeItem::Global(g) => PathRes::Global(g),
                            ScopeItem::Variant(en, t) => PathRes::Variant(en, t, None),
                        })
                    }
                    PathRes::Enum(en, args) => {
                        let tag = self.enums[en as usize].variants.iter().position(|v| v.name == name.name)?;
                        Some(PathRes::Variant(en, tag as u32, args))
                    }
                    PathRes::Class(c, _) => self.classes[&c].consts.get(&name.name).map(|&g| PathRes::Global(g)),
                    _ => None,
                }
            }
            ExprKind::Index { recv, args } => {
                let base = self.resolve_path(recv)?;
                let tys: Vec<TyId> = args.iter().map(|a| self.expr_as_type(a)).collect();
                match base {
                    PathRes::Class(c, None) => Some(PathRes::Class(c, Some(tys))),
                    PathRes::Enum(en, None) => Some(PathRes::Enum(en, Some(tys))),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Reads an expression written in type position (`List[Int]` as a value prefix).
    pub fn expr_as_type(&mut self, e: &Expr) -> TyId {
        match expr_to_type_expr(e) {
            Some(te) => self.resolve_type(&te, &mut self.type_scope()),
            None => {
                self.err("E0330", "expected a type here", e.span);
                self.types.error
            }
        }
    }

    fn check_call(
        &mut self,
        e: &Expr,
        callee: &Expr,
        args: &[Arg],
        block: Option<&Expr>,
        expected: Option<TyId>,
    ) -> TyId {
        if let ExprKind::Ident(n) = &callee.kind {
            if n == "freeze" && self.lookup_local(n).is_none() && self.lookup_name(self.cur_module, n).is_none() {
                if args.len() != 1 || block.is_some() {
                    self.err("E0332", "`freeze` takes exactly one argument", e.span);
                    self.check_args_loose(args, block);
                    return self.types.error;
                }
                let t = self.check_expr(&args[0].value, None);
                self.calls.insert(e.id, CallRes::Freeze);
                // The object is frozen in place: the variable (and its aliases) now hold a frozen
                // value until reassigned, so mutating through them is a compile error.
                if let ExprKind::Ident(name) = &args[0].value.kind {
                    if let Some(l) = self.lookup_local(name) {
                        if self.cap_of(self.locals[l as usize].ty) == Cap::Owned {
                            for m in self.aliases_of(l) {
                                let ft = self.freeze_ty(self.local_ty(m));
                                self.narrow(m, ft);
                            }
                        }
                    }
                }
                return self.freeze_ty(t);
            }
        }
        if let Some(path) = self.resolve_path(callee) {
            match path {
                PathRes::Fn(f) => {
                    self.names.insert(callee.id, NameRes::Fn(f));
                    if self.fns[f as usize].has_untyped_params {
                        return self.call_implicit(e, f, args, block);
                    }
                    let m = MethodRef {
                        fid: f,
                        subst: FxHashMap::default(),
                        self_ty: self.types.error,
                        dispatch: Dispatch::Static(f),
                        frozen: false,
                        alts: Vec::new(),
                    };
                    let (slots, ret) = self.check_args(e, &m, args, block, expected, false);
                    self.calls.insert(
                        e.id,
                        CallRes::Fn { dispatch: Dispatch::Static(f), recv: false, args: slots, safe: false },
                    );
                    return ret;
                }
                PathRes::Variant(en, tag, targs) => return self.construct_variant(e, en, tag, targs, args, expected),
                PathRes::Class(c, _) => {
                    let name = self.classes[&c].name.clone();
                    let msg = format!("`{name}` is a class; construct it with `{name}.new(...)`");
                    let fix_span = callee.span.after();
                    self.emit(Diag::error("E0331", msg, callee.span).fix("call `new`", fix_span, ".new"));
                    for a in args {
                        self.check_expr(&a.value, None);
                    }
                    return self.types.error;
                }
                _ => {}
            }
        }
        // Calling a closure value.
        let ct = self.check_expr(callee, None);
        let ctr = self.types.shallow_resolve(ct);
        match self.types.get(ctr).clone() {
            Ty::Fn(ps, ret) => {
                let mut slots = Vec::new();
                let n = args.len() + block.is_some() as usize;
                if n != ps.len() {
                    let msg = format!("this function takes {} argument(s), but {} were given", ps.len(), n);
                    self.err("E0332", msg, e.span);
                }
                for (i, a) in args.iter().enumerate() {
                    if a.name.is_some() {
                        self.err("E0333", "closures do not take named arguments", a.value.span);
                    }
                    match ps.get(i) {
                        Some(&p) => {
                            self.check_against(&a.value, p, "argument");
                        }
                        None => {
                            self.check_expr(&a.value, None);
                        }
                    }
                    slots.push(ArgSlot::Arg(i));
                }
                if let Some(b) = block {
                    if let Some(&p) = ps.get(args.len()) {
                        self.check_against(b, p, "block argument");
                    }
                    slots.push(ArgSlot::Block);
                }
                self.calls.insert(e.id, CallRes::Closure { args: slots });
                ret
            }
            Ty::Dyn | Ty::Error => {
                for a in args {
                    self.check_expr(&a.value, None);
                }
                if let Some(b) = block {
                    self.check_expr(b, None);
                }
                let slots = (0..args.len()).map(ArgSlot::Arg).chain(block.map(|_| ArgSlot::Block)).collect();
                self.calls.insert(e.id, CallRes::Closure { args: slots });
                ctr
            }
            Ty::Open(o) => {
                if let Some(a) = args.iter().find(|a| a.name.is_some()) {
                    self.err("E0336", "a function value takes positional arguments only", a.value.span);
                }
                let mut tys: Vec<TyId> = args.iter().map(|a| self.check_expr(&a.value, None)).collect();
                if let Some(b) = block {
                    tys.push(self.check_expr(b, None));
                }
                let ret = self.new_open(o);
                self.push_req(o, OpenReq::Call { args: tys, ret, span: e.span });
                let slots = (0..args.len()).map(ArgSlot::Arg).chain(block.map(|_| ArgSlot::Block)).collect();
                self.calls.insert(e.id, CallRes::Closure { args: slots });
                ret
            }
            _ => {
                let msg = format!("`{}` is not a function", self.show(ct));
                self.err("E0334", msg, callee.span);
                self.types.error
            }
        }
    }

    /// Checks arguments against a method's signature. Returns the slots and the result type.
    pub fn check_args(
        &mut self,
        call: &Expr,
        m: &MethodRef,
        args: &[Arg],
        block: Option<&Expr>,
        expected: Option<TyId>,
        parallel: bool,
    ) -> (Vec<ArgSlot>, TyId) {
        let (params, ret) = self.instantiate_sig(m);
        let inst = std::mem::take(&mut self.last_inst);
        let inst_subst = std::mem::take(&mut self.last_inst_subst);
        let sigs = std::mem::take(&mut self.last_sigs);
        let fname = self.fns[m.fid as usize].name.clone();
        // A union receiver: arguments are matched and checked against the first member's
        // parameters, then against each other member's below.
        let params = match sigs.first() {
            Some((_, ps, _)) => ps.clone(),
            None => params,
        };
        // Early: use the expected result type to inform inference.
        if let Some(exp) = expected {
            if self.types.has_vars(ret) {
                self.trial(|c| c.assignable(ret, exp));
            }
        }
        let mut slots: Vec<Option<ArgSlot>> = vec![None; params.len()];
        let mut ok = true;
        let mut pos = 0;
        for (i, a) in args.iter().enumerate() {
            match &a.name {
                None => {
                    while pos < params.len() && slots[pos].is_some() {
                        pos += 1;
                    }
                    if pos >= params.len() {
                        let msg = format!("`{fname}` takes {} argument(s), but more were given", params.len());
                        self.err("E0332", msg, a.value.span);
                        ok = false;
                        self.check_expr(&a.value, None);
                        continue;
                    }
                    slots[pos] = Some(ArgSlot::Arg(i));
                }
                Some(n) => match params.iter().position(|p| p.name == n.name) {
                    Some(j) if slots[j].is_none() => slots[j] = Some(ArgSlot::Arg(i)),
                    Some(_) => {
                        let msg = format!("argument `{}` given twice", n.name);
                        self.err("E0335", msg, n.span);
                        ok = false;
                    }
                    None => {
                        let mut d =
                            Diag::error("E0336", format!("`{fname}` has no parameter named `{}`", n.name), n.span);
                        if let Some(s) = crate::util::suggest(&n.name, params.iter().map(|p| p.name.as_str())) {
                            d = d.suggest(format!("did you mean `{s}`?"), n.span, s);
                        }
                        self.emit(d);
                        ok = false;
                        self.check_expr(&a.value, None);
                    }
                },
            }
        }
        if let Some(blk) = block {
            // The block fills the last unfilled parameter.
            match (0..params.len()).rev().find(|&j| slots[j].is_none()) {
                Some(j) => slots[j] = Some(ArgSlot::Block),
                None => {
                    let msg = format!("`{fname}` does not take a block");
                    self.err("E0337", msg, blk.span);
                    ok = false;
                    self.check_expr(blk, None);
                }
            }
        }
        for (j, p) in params.iter().enumerate() {
            if slots[j].is_none() {
                if p.has_default {
                    slots[j] = Some(ArgSlot::Missing);
                } else {
                    let msg = format!("missing argument `{}` for `{fname}`", p.name);
                    self.emit(Diag::error("E0338", msg, call.span).suggest(
                        format!("add `{}`", p.name),
                        call.span.after(),
                        String::new(),
                    ));
                    ok = false;
                }
            }
        }
        if ok && sigs.len() > 1 {
            let omitted: Vec<bool> = slots.iter().map(|s| matches!(s, Some(ArgSlot::Missing))).collect();
            if !self.union_sigs_agree(call, m, &sigs, &omitted) {
                self.check_args_loose(args, block);
                return (Vec::new(), self.types.error);
            }
        }
        // Each member's parameters, when the call matched the first member's.
        let union_sigs = ok && sigs.len() > 1;
        // Check closures last so their parameter types are known.
        let order: Vec<usize> = {
            let mut o: Vec<usize> = (0..params.len()).collect();
            o.sort_by_key(|&j| {
                let is_closure = match &slots[j] {
                    Some(ArgSlot::Block) => true,
                    Some(ArgSlot::Arg(i)) => matches!(args[*i].value.kind, ExprKind::Closure { .. }),
                    _ => false,
                };
                is_closure as u8
            });
            o
        };
        for j in order {
            let p = &params[j];
            let (expr, is_block) = match &slots[j] {
                Some(ArgSlot::Arg(i)) => (&args[*i].value, false),
                Some(ArgSlot::Block) => (block.unwrap(), true),
                _ => continue,
            };
            let _ = is_block;
            let pt = self.types.shallow_resolve(p.ty);
            let t = if parallel {
                if let ExprKind::Closure { params: cps, ret: cret, body, .. } = &expr.kind {
                    let t = self.check_closure(expr, cps, cret.as_ref(), body, Some(pt), Some(false));
                    self.record_ty(expr.id, t);
                    t
                } else {
                    self.check_expr(expr, Some(pt))
                }
            } else {
                self.check_expr(expr, Some(pt))
            };
            let fits = self.require_assignable(expr, t, pt, &format!("argument `{}`", p.name))
                && (!union_sigs || self.union_arg_fits(expr, t, j, m, &sigs));
            ok &= fits;
            if p.is_move {
                self.check_move_arg(expr, t);
            }
        }
        // Bounds of the function's own type parameters.
        if ok {
            for (p, v) in inst {
                let bounds = self.params[p as usize].bounds.clone();
                let actual = self.types.resolve(v);
                if matches!(self.types.get(actual), Ty::Var(_) | Ty::Error) {
                    continue;
                }
                for b in bounds {
                    let b = self.types.subst(b, &inst_subst, Some(m.self_ty));
                    if !self.assignable(actual, b) {
                        let pname = self.params[p as usize].name.clone();
                        let msg = format!(
                            "`{}` does not satisfy the bound `{}` of type parameter `{pname}` in `{fname}`",
                            self.show(actual),
                            self.show(b)
                        );
                        let mut d = Diag::error("E0368", msg, call.span);
                        for n in self.conformance_notes(actual, b) {
                            d = d.note(n);
                        }
                        self.emit(d);
                    }
                }
            }
        }
        let slots = slots.into_iter().map(|s| s.unwrap_or(ArgSlot::Missing)).collect();
        (slots, ret)
    }

    /// For a member reached on a union receiver: each member's method must be an instance method
    /// that the by-name call can reach at runtime, and not a mutating one on a frozen member.
    fn check_union_method(&mut self, m: &MethodRef, rt: TyId, name: &ast::Ident) -> bool {
        let fallback = match &m.dispatch {
            Dispatch::Dynamic(_, f) => *f,
            _ => None,
        };
        let mut ok = true;
        for a in &m.alts {
            let f = self.fns[a.fid as usize].clone();
            let part = format!("`{}` (part of `{}`)", self.show(a.self_ty), self.show(rt));
            if f.self_kind == ast::SelfKind::None {
                let msg = format!("{part} has `{}` only as a static function", name.name);
                self.err("E0346", msg, name.span);
                ok = false;
                continue;
            }
            let needs = self.union_fallback(a, &name.name);
            if needs.is_some() && needs != fallback {
                let msg = format!(
                    "{part} has `{}` only as an extension or interface default method, and another member needs a different one; a call on a union value can fall back to only one",
                    name.name
                );
                self.err("E0311", msg, name.span);
                ok = false;
                continue;
            }
            if a.frozen {
                if f.mutating {
                    let msg = format!("cannot call mutating method `{}` on frozen {part}", name.name);
                    self.emit(
                        Diag::error("E0614", msg, name.span)
                            .note("frozen values are deeply immutable; use `.dup()` for a mutable copy"),
                    );
                    ok = false;
                } else if !f.is_native {
                    let shown = self.show(a.self_ty);
                    self.frozen_calls.push((name.span, a.fid, shown));
                }
            }
        }
        ok
    }

    /// For a call on a union receiver: whether each member's method takes the parameters of the
    /// first member's (the same names in the same order), so the arguments, matched once by name
    /// and position, suit every member, and has a default for each `omitted` argument.
    fn union_sigs_agree(
        &mut self,
        call: &Expr,
        m: &MethodRef,
        sigs: &[(MethodRef, Vec<PInfo>, TyId)],
        omitted: &[bool],
    ) -> bool {
        let (first, ps0, _) = &sigs[0];
        let shape = |ps: &[PInfo]| {
            ps.iter()
                .map(|p| if p.has_default { format!("{} = ...", p.name) } else { p.name.clone() })
                .collect::<Vec<_>>()
                .join(", ")
        };
        for (a, ps, _) in &sigs[1..] {
            let agree = ps.len() == ps0.len()
                && ps.iter().zip(ps0).zip(omitted).all(|((p, q), &o)| p.name == q.name && (p.has_default || !o));
            if agree {
                continue;
            }
            let name = &self.fns[m.fid as usize].name;
            let msg = format!(
                "`{name}` takes different parameters on the members of `{}`: `({})` on `{}`, `({})` on `{}`",
                self.show(m.self_ty),
                shape(ps0),
                self.show(first.self_ty),
                shape(ps),
                self.show(a.self_ty)
            );
            self.emit(
                Diag::error("E0332", msg, call.span)
                    .note("a call on a union value must suit the method of every member"),
            );
            return false;
        }
        true
    }

    /// For a call on a union receiver: whether argument `t`, already checked against parameter `j`
    /// of the first member's method, also suits parameter `j` of every other member's.
    fn union_arg_fits(
        &mut self,
        expr: &Expr,
        t: TyId,
        j: usize,
        m: &MethodRef,
        sigs: &[(MethodRef, Vec<PInfo>, TyId)],
    ) -> bool {
        // The value passed after any conversion made for the first member's parameter: the call is
        // dispatched at runtime and passes the same value to every member.
        let passed = if self.coerce.get(&expr.id) == Some(&Coercion::IntToFloat) { self.types.float } else { t };
        for (a, ps, _) in &sigs[1..] {
            let pt = ps[j].ty;
            if self.assignable(passed, pt) {
                continue;
            }
            let name = self.fns[m.fid as usize].name.clone();
            let msg = format!(
                "mismatched types in argument `{}`: `{name}` on `{}` expects `{}`, found `{}`",
                ps[j].name,
                self.show(a.self_ty),
                self.show(pt),
                self.show(passed)
            );
            let note = format!(
                "the receiver is `{}`, so the argument must suit `{name}` on every member",
                self.show(m.self_ty)
            );
            self.emit(Diag::error("E0300", msg, expr.span).note(note));
            return false;
        }
        true
    }

    /// Whether a value of type `t` is or holds a function (a closure), which stays callable only
    /// unfrozen.
    fn holds_fn(&self, t: TyId) -> bool {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t) {
            Ty::Fn(..) => true,
            Ty::Union(ts) | Ty::Tuple(ts) => ts.iter().any(|&x| self.holds_fn(x)),
            Ty::Class(_, args) => args.iter().any(|&x| self.holds_fn(x)),
            _ => false,
        }
    }

    /// Owned values passed to a `move` parameter must be fresh or explicitly moved.
    fn check_move_arg(&mut self, expr: &Expr, t: TyId) {
        if !self.check_not_pinned(t, "a value", expr.span) {
            return;
        }
        if self.cap_of(t) != Cap::Owned {
            return;
        }
        match &expr.kind {
            ExprKind::Move(_) => {}
            ExprKind::Ident(name) => {
                let msg = if self.holds_fn(t) {
                    // A frozen function cannot be called, so freezing is no way to share it.
                    format!("`{name}` is owned by this thread; pass `move {name}` to transfer it")
                } else {
                    format!("`{name}` is owned by this thread; pass `move {name}` to transfer it, or `freeze({name})` to share it read-only")
                };
                self.emit(Diag::error("E0612", msg, expr.span).fix("move it", expr.span.empty_at(), "move "));
            }
            ExprKind::Member { .. } | ExprKind::Index { .. } => {
                let msg = "an owned value reachable from another variable cannot be sent; copy it (`.dup()`) or restructure so a local can be moved";
                self.err("E0613", msg, expr.span);
            }
            _ => {}
        }
    }

    fn construct_variant(
        &mut self,
        e: &Expr,
        en: EnumId,
        tag: u32,
        targs: Option<Vec<TyId>>,
        args: &[Arg],
        expected: Option<TyId>,
    ) -> TyId {
        let info = self.enums[en as usize].clone();
        // Type arguments written here or inferred from the payload are checked against the
        // enum's bounds; ones taken from the expected type are checked where that is written.
        let mut from_expected = false;
        let targs = targs.unwrap_or_else(|| {
            // Use the expected enum's arguments when available.
            if let Some(exp) = expected {
                if let Ty::Enum(e2, a) = self.types.get(self.types.shallow_resolve(exp)).clone() {
                    if e2 == en {
                        from_expected = true;
                        return a;
                    }
                }
                if let Some(inner) = self.types.strip_nil(exp) {
                    if let Ty::Enum(e2, a) = self.types.get(inner).clone() {
                        if e2 == en {
                            from_expected = true;
                            return a;
                        }
                    }
                }
            }
            info.params.iter().map(|_| self.types.fresh_var()).collect()
        });
        let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(targs.iter().copied()).collect();
        let v = &info.variants[tag as usize];
        if args.len() != v.fields.len() {
            let msg = format!("`{}` takes {} value(s), but {} were given", v.name, v.fields.len(), args.len());
            self.err("E0339", msg, e.span);
        }
        let mut slots = Vec::new();
        for (i, a) in args.iter().enumerate() {
            match v.fields.get(i) {
                Some(&(_, ft)) => {
                    let ft = self.types.subst(ft, &subst, None);
                    self.check_against(&a.value, ft, "variant field");
                }
                None => {
                    self.check_expr(&a.value, None);
                }
            }
            slots.push(ArgSlot::Arg(i));
        }
        self.calls.insert(e.id, CallRes::Variant { class: v.class_id, args: slots });
        if !from_expected {
            self.note_instance(&info.name, &info.params, &targs, e.span);
        }
        self.types.intern(Ty::Enum(en, targs))
    }

    /// Instantiates a function with untyped parameters at one call site.
    fn call_implicit(&mut self, e: &Expr, f: FnId, args: &[Arg], block: Option<&Expr>) -> TyId {
        // The requirements come from the body, so it is checked first even when the result type
        // is annotated.
        if self.fns[f as usize].state == CheckState::Unchecked {
            self.check_fn_body(f);
        }
        if self.fns[f as usize].state == CheckState::InProgress {
            let name = self.fns[f as usize].name.clone();
            let msg = format!("recursive call to `{name}`, whose parameters are untyped; annotate its parameters");
            self.err("E0340", msg, e.span);
            return self.types.error;
        }
        let sig = self.fns[f as usize].implicit.clone().unwrap_or_default();
        let params = self.fns[f as usize].params.clone();
        let fname = self.fns[f as usize].name.clone();
        let mut map: FxHashMap<OpenId, TyId> = FxHashMap::default();
        let mut slots: Vec<Option<ArgSlot>> = vec![None; params.len()];
        let mut arg_tys: Vec<Option<TyId>> = vec![None; params.len()];
        let mut pos = 0;
        for (i, a) in args.iter().enumerate() {
            let j = match &a.name {
                None => {
                    while pos < params.len() && slots[pos].is_some() {
                        pos += 1;
                    }
                    pos
                }
                Some(n) => params.iter().position(|p| p.name == n.name).unwrap_or(params.len()),
            };
            if j >= params.len() {
                self.err("E0332", format!("too many arguments for `{fname}`"), a.value.span);
                continue;
            }
            slots[j] = Some(ArgSlot::Arg(i));
            let pt = sig.params.get(j).copied().unwrap_or(self.types.error);
            let t = if matches!(self.types.get(pt), Ty::Open(_)) {
                self.check_expr(&a.value, None)
            } else {
                self.check_against(&a.value, pt, "argument")
            };
            arg_tys[j] = Some(t);
        }
        if let Some(b) = block {
            if let Some(j) = (0..params.len()).rev().find(|&j| slots[j].is_none()) {
                slots[j] = Some(ArgSlot::Block);
                let t = self.check_expr(b, None);
                arg_tys[j] = Some(t);
            }
        }
        for (j, p) in params.iter().enumerate() {
            if slots[j].is_none() {
                if p.has_default {
                    slots[j] = Some(ArgSlot::Missing);
                    arg_tys[j] = Some(sig.params.get(j).copied().unwrap_or(self.types.error));
                } else {
                    self.err("E0338", format!("missing argument `{}` for `{fname}`", p.name), e.span);
                }
            }
        }
        for (j, &pt) in sig.params.iter().enumerate() {
            if let (Ty::Open(o), Some(t)) = (self.types.get(pt).clone(), arg_tys[j]) {
                map.insert(o, t);
            }
        }
        let ret = self.apply_opens(e.span, &fname, &map, sig.ret);
        let slots = slots.into_iter().map(|s| s.unwrap_or(ArgSlot::Missing)).collect();
        self.calls.insert(e.id, CallRes::Fn { dispatch: Dispatch::Static(f), recv: false, args: slots, safe: false });
        ret
    }

    /// Checks recorded open requirements against concrete argument types, returning the
    /// instantiated result type.
    ///
    /// Requirements are checked once the types they mention are known: a requirement such as
    /// `a + b.n` on `a` waits until `b.n` has been resolved from `b`'s argument type.
    fn apply_opens(&mut self, span: Span, fname: &str, map: &FxHashMap<OpenId, TyId>, ret: TyId) -> TyId {
        #[derive(Clone, Copy)]
        enum Item {
            Req(OpenId, usize),
            Upper(OpenId, usize),
        }
        let mut map = map.clone();
        let mut pending: Vec<Item> = Vec::new();
        let add = |c: &Checker, o: OpenId, pending: &mut Vec<Item>| {
            let info = &c.opens[o as usize];
            pending.extend((0..info.reqs.len()).map(|i| Item::Req(o, i)));
            pending.extend((0..info.uppers.len()).map(|i| Item::Upper(o, i)));
        };
        let mut roots: Vec<OpenId> = map.keys().copied().collect();
        roots.sort_unstable();
        for o in roots {
            add(self, o, &mut pending);
        }
        while !pending.is_empty() {
            // The first item whose inputs are all known; if none (not expected), the first item.
            let pick = pending
                .iter()
                .position(|it| {
                    let deps = match *it {
                        Item::Req(o, i) => self.opens[o as usize].reqs[i].inputs(),
                        Item::Upper(o, i) => vec![self.opens[o as usize].uppers[i].0],
                    };
                    let mut opens = Vec::new();
                    for d in deps {
                        self.collect_opens(d, &mut opens);
                    }
                    opens.iter().all(|o| map.contains_key(o))
                })
                .unwrap_or(0);
            let item = pending.remove(pick);
            match item {
                Item::Req(o, i) => {
                    let concrete = map[&o];
                    let req = self.opens[o as usize].reqs[i].clone();
                    let produced = self.apply_req(span, fname, o, concrete, &req, &map);
                    if let Some(ro) = req.result() {
                        if let Ty::Open(ro) = self.types.get(ro).clone() {
                            if let std::collections::hash_map::Entry::Vacant(v) = map.entry(ro) {
                                v.insert(produced);
                                add(self, ro, &mut pending);
                            }
                        }
                    }
                }
                Item::Upper(o, i) => {
                    let concrete = map[&o];
                    let (up, usp) = self.opens[o as usize].uppers[i];
                    let up = self.subst_opens(up, &map);
                    if !self.assignable(concrete, up) {
                        let msg = match &self.opens[o as usize].label {
                            None => format!(
                                "`{}` passed to `{fname}` as `{}` is used as `{}`",
                                self.show(concrete),
                                self.opens[o as usize].param_name,
                                self.show(up)
                            ),
                            Some(l) => format!(
                                "in this call to `{fname}`, `{l}` is `{}`, which is used as `{}`",
                                self.show(concrete),
                                self.show(up)
                            ),
                        };
                        self.emit(Diag::error("E0341", msg, span).note_at("required here", usp));
                    }
                }
            }
        }
        self.subst_opens(ret, &map)
    }

    /// Open types mentioned in `t`.
    fn collect_opens(&self, t: TyId, out: &mut Vec<OpenId>) {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t) {
            Ty::Open(o) => out.push(*o),
            Ty::Class(_, a) | Ty::Enum(_, a) | Ty::Iface(_, a) | Ty::Tuple(a) | Ty::Union(a) | Ty::Inter(a) => {
                for &x in a {
                    self.collect_opens(x, out);
                }
            }
            Ty::Fn(p, r) => {
                for &x in p {
                    self.collect_opens(x, out);
                }
                self.collect_opens(*r, out);
            }
            Ty::Frozen(i) => self.collect_opens(*i, out),
            _ => {}
        }
    }

    fn subst_opens(&mut self, t: TyId, map: &FxHashMap<OpenId, TyId>) -> TyId {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t).clone() {
            Ty::Open(o) => map.get(&o).copied().unwrap_or(self.types.dyn_),
            Ty::Class(c, a) => {
                let a = a.into_iter().map(|x| self.subst_opens(x, map)).collect();
                self.types.class(c, a)
            }
            Ty::Enum(e, a) => {
                let a = a.into_iter().map(|x| self.subst_opens(x, map)).collect();
                self.types.intern(Ty::Enum(e, a))
            }
            Ty::Iface(i, a) => {
                let a = a.into_iter().map(|x| self.subst_opens(x, map)).collect();
                self.types.intern(Ty::Iface(i, a))
            }
            Ty::Tuple(a) => {
                let a = a.into_iter().map(|x| self.subst_opens(x, map)).collect();
                self.types.tuple(a)
            }
            Ty::Union(a) => {
                let a = a.into_iter().map(|x| self.subst_opens(x, map)).collect();
                self.types.union(a)
            }
            Ty::Inter(a) => {
                let a = a.into_iter().map(|x| self.subst_opens(x, map)).collect();
                self.types.inter(a)
            }
            Ty::Fn(p, r) => {
                let p = p.into_iter().map(|x| self.subst_opens(x, map)).collect();
                let r = self.subst_opens(r, map);
                self.types.func(p, r)
            }
            Ty::Frozen(i) => {
                let i = self.subst_opens(i, map);
                self.freeze_ty(i)
            }
            _ => t,
        }
    }

    /// Checks one requirement against the type `recv` passed for an open, returning the type it
    /// produces.
    fn apply_req(
        &mut self,
        span: Span,
        fname: &str,
        open: OpenId,
        recv: TyId,
        req: &OpenReq,
        map: &FxHashMap<OpenId, TyId>,
    ) -> TyId {
        let recv = self.types.shallow_resolve(recv);
        // A requirement on an open of the calling function propagates to it.
        if let Ty::Open(co) = self.types.get(recv).clone() {
            let ret = self.new_open(co);
            let inputs: Vec<TyId> = req.inputs().into_iter().map(|t| self.subst_opens(t, map)).collect();
            let r = req.with(&inputs, ret);
            self.push_req(co, r);
            return ret;
        }
        match self.types.get(recv).clone() {
            Ty::Dyn => self.types.dyn_,
            Ty::Error => self.types.error,
            Ty::Never => self.types.never,
            // Erased generics dispatch on the runtime value: each member of a union must satisfy the
            // requirement, and the result is the union of what each produces.
            Ty::Union(parts) => {
                let mut outs = Vec::new();
                for p in parts {
                    let t = self.req_on(span, fname, open, p, recv, req, map);
                    if self.types.is_error(t) {
                        return t;
                    }
                    outs.push(t);
                }
                self.types.union(outs)
            }
            _ => self.req_on(span, fname, open, recv, recv, req, map),
        }
    }

    /// `apply_req` for one non-union type `recv` (a member of the argument type `whole`).
    #[allow(clippy::too_many_arguments)]
    fn req_on(
        &mut self,
        span: Span,
        fname: &str,
        open: OpenId,
        recv: TyId,
        whole: TyId,
        req: &OpenReq,
        map: &FxHashMap<OpenId, TyId>,
    ) -> TyId {
        let err = self.types.error;
        let fail = |c: &mut Checker, what: String, at: Span| {
            let shown = if recv == whole {
                format!("`{}`", c.show(whole))
            } else {
                format!("`{}` (part of `{}`)", c.show(recv), c.show(whole))
            };
            let msg = match &c.opens[open as usize].label {
                None => format!("{shown} passed to `{fname}` as `{}` {what}", c.opens[open as usize].param_name),
                Some(l) => format!("in this call to `{fname}`, `{l}` is {shown}, which {what}"),
            };
            c.emit(Diag::error("E0342", msg, span).note_at("required by this use", at));
        };
        let inputs: Vec<TyId> = req.inputs().into_iter().map(|t| self.subst_opens(t, map)).collect();
        match req {
            OpenReq::Method { name, named, block, span: rs, .. } => {
                let args = inputs;
                if args.is_empty() {
                    if let Some((_, ft)) = self.lookup_field(recv, name) {
                        return ft;
                    }
                }
                let Some(m) = self.lookup_method(recv, name, true) else {
                    fail(self, format!("has no method or field `{name}`"), *rs);
                    return err;
                };
                let f = self.fns[m.fid as usize].clone();
                if f.self_kind == ast::SelfKind::None {
                    fail(self, format!("has `{name}` only as a static function"), *rs);
                    return err;
                }
                if !self.runtime_dispatchable(recv, name, &m.dispatch) {
                    let msg = format!(
                        "has `{name}` only as an extension or interface default method, which a call through an untyped parameter cannot reach"
                    );
                    fail(self, msg, *rs);
                    return err;
                }
                if m.frozen {
                    if f.mutating {
                        fail(self, format!("is frozen, but `{name}` mutates it"), *rs);
                        return err;
                    } else if !f.is_native {
                        let shown = self.show(recv);
                        self.frozen_calls.push((*rs, m.fid, shown));
                    }
                }
                let (ps, ret) = self.instantiate_sig(&m);
                let inst = std::mem::take(&mut self.last_inst);
                let inst_subst = std::mem::take(&mut self.last_inst_subst);
                // Match arguments to parameters as a call does: positional, named, trailing block.
                let mut slots: Vec<Option<usize>> = vec![None; ps.len()];
                let mut pos = 0;
                for k in 0..args.len() {
                    let is_block = *block && k + 1 == args.len();
                    let j = if is_block {
                        match (0..ps.len()).rev().find(|&j| slots[j].is_none()) {
                            Some(j) => j,
                            None => {
                                fail(self, format!("has `{name}`, but it does not take a block"), *rs);
                                return err;
                            }
                        }
                    } else if let Some(Some(n)) = named.get(k) {
                        match ps.iter().position(|p| &p.name == n) {
                            Some(j) if slots[j].is_none() => j,
                            _ => {
                                fail(self, format!("has `{name}`, but it has no parameter named `{n}`"), *rs);
                                return err;
                            }
                        }
                    } else {
                        while pos < ps.len() && slots[pos].is_some() {
                            pos += 1;
                        }
                        if pos >= ps.len() {
                            fail(self, format!("has `{name}`, but it takes {} argument(s)", ps.len()), *rs);
                            return err;
                        }
                        pos
                    };
                    slots[j] = Some(k);
                    // The body is compiled once and passes arguments by position.
                    if j != k {
                        let msg = format!(
                            "has `{name}`, but a call through an untyped parameter passes arguments by position, so they must follow the order of `{name}`'s parameters"
                        );
                        fail(self, msg, *rs);
                        return err;
                    }
                }
                for (j, p) in ps.iter().enumerate() {
                    match slots[j] {
                        None if !p.has_default => {
                            fail(self, format!("has `{name}`, but it needs argument `{}`", p.name), *rs);
                            return err;
                        }
                        Some(k) if !self.assignable(args[k], p.ty) => {
                            let msg = format!(
                                "has `{name}`, but its parameter `{}` is `{}`, not `{}`",
                                p.name,
                                self.show(p.ty),
                                self.show(args[k])
                            );
                            fail(self, msg, *rs);
                            return err;
                        }
                        _ => {}
                    }
                }
                for (tp, v) in inst {
                    let actual = self.types.resolve(v);
                    if matches!(self.types.get(actual), Ty::Var(_) | Ty::Error) {
                        continue;
                    }
                    for b in self.params[tp as usize].bounds.clone() {
                        let b = self.types.subst(b, &inst_subst, Some(m.self_ty));
                        if !self.assignable(actual, b) {
                            let msg = format!(
                                "has `{name}`, but `{}` does not satisfy its bound `{}`",
                                self.show(actual),
                                self.show(b)
                            );
                            fail(self, msg, *rs);
                            return err;
                        }
                    }
                }
                self.types.resolve(ret)
            }
            OpenReq::BinOp { op, span: rs, .. } => match self.trial_binary(*op, recv, inputs[0]) {
                Ok(t) => t,
                Err(typed) => {
                    let msg = format!(
                        "does not support `{}` with `{}`{}",
                        op.symbol(),
                        self.show(inputs[0]),
                        generic_op_hint(*op, typed)
                    );
                    fail(self, msg, *rs);
                    err
                }
            },
            OpenReq::BinOpRhs { op, span: rs, .. } => match self.trial_binary(*op, inputs[0], recv) {
                Ok(t) => t,
                Err(typed) => {
                    let msg = format!(
                        "cannot be the right side of `{}` `{}`{}",
                        self.show(inputs[0]),
                        op.symbol(),
                        generic_op_hint(*op, typed)
                    );
                    fail(self, msg, *rs);
                    err
                }
            },
            OpenReq::Unary { op, span: rs, .. } => {
                if self.numeric_kind(recv).is_some() {
                    recv
                } else {
                    fail(self, format!("does not support unary `{}`", if *op == UnOp::Neg { "-" } else { "~" }), *rs);
                    err
                }
            }
            OpenReq::Index { span: rs, .. } => {
                if inputs.len() != 1 {
                    fail(self, "cannot be indexed with these arguments".into(), *rs);
                    return err;
                }
                match self.trial_index(recv, inputs[0], *rs) {
                    Some(t) => t,
                    None => {
                        fail(self, format!("cannot be indexed with `{}`", self.show(inputs[0])), *rs);
                        err
                    }
                }
            }
            OpenReq::SetIndex { op, span: rs, .. } => {
                let (index, value) = (inputs[0], inputs[1]);
                if matches!(self.types.get(recv), Ty::Frozen(_)) {
                    fail(self, "is frozen, but is assigned an element".into(), *rs);
                    return err;
                }
                let Some(m) = self.lookup_method(recv, "[]=", false) else {
                    fail(self, "does not support index assignment".into(), *rs);
                    return err;
                };
                if !self.runtime_dispatchable(recv, "[]=", &m.dispatch) {
                    let msg = "has `[]=` only as an extension or interface default method".to_string();
                    fail(self, msg, *rs);
                    return err;
                }
                let (ps, _) = self.instantiate_sig(&m);
                if ps.len() != 2 || !self.assignable(index, ps[0].ty) {
                    fail(self, format!("cannot be indexed with `{}`", self.show(index)), *rs);
                    return err;
                }
                let stored = match op {
                    None => Some(value),
                    Some(BinOp::Or | BinOp::And) => {
                        let b = self.types.bool;
                        self.assignable(value, b).then_some(b)
                    }
                    Some(BinOp::Coalesce) => match self.trial_index(recv, index, *rs) {
                        Some(c) => {
                            let inner = self.types.strip_nil(c).unwrap_or(c);
                            self.assignable(value, inner).then_some(inner)
                        }
                        None => None,
                    },
                    Some(o) => match self.trial_index(recv, index, *rs) {
                        Some(c) => self.trial_binary(*o, c, value).ok(),
                        None => None,
                    },
                };
                match stored {
                    Some(t) if self.assignable(t, ps[1].ty) => self.types.nil,
                    _ => {
                        fail(self, format!("cannot store `{}` as an element", self.show(value)), *rs);
                        err
                    }
                }
            }
            OpenReq::Iter { span: rs, .. } => match self.elem_type(recv) {
                Some(t) => t,
                None => {
                    fail(self, "is not iterable".into(), *rs);
                    err
                }
            },
            OpenReq::Call { span: rs, .. } => match self.types.get(recv).clone() {
                Ty::Fn(ps, r) if ps.len() == inputs.len() => {
                    for (k, &a) in inputs.iter().enumerate() {
                        if !self.assignable(a, ps[k]) {
                            let msg = format!(
                                "is a function whose parameter {} is `{}`, not `{}`",
                                k + 1,
                                self.show(ps[k]),
                                self.show(a)
                            );
                            fail(self, msg, *rs);
                            return err;
                        }
                    }
                    r
                }
                Ty::Fn(ps, _) => {
                    let msg = format!("is a function of {} argument(s), called with {}", ps.len(), inputs.len());
                    fail(self, msg, *rs);
                    err
                }
                _ => {
                    fail(self, "is not callable".into(), *rs);
                    err
                }
            },
        }
    }

    /// The result of `l op r` if typed code accepts it and the generic evaluation in an
    /// implicit-generic body can perform it (no diagnostics are emitted). `Err(true)` means typed
    /// code accepts it but the generic evaluation cannot.
    fn trial_binary(&mut self, op: BinOp, l: TyId, r: TyId) -> Result<TyId, bool> {
        let dummy = Expr { id: u32::MAX, kind: ExprKind::Error, span: Span::default() };
        let before = self.suppressed_errors;
        self.suppress_diags += 1;
        let t = self.binary_types(&dummy, op, &dummy, &dummy, l, r);
        self.suppress_diags -= 1;
        let res = self.bins.remove(&u32::MAX);
        self.coerce.remove(&u32::MAX);
        let failed = self.suppressed_errors != before || self.types.is_error(t);
        if failed {
            return Err(false);
        }
        // The body evaluates the operator generically: bitwise operators on integers only,
        // comparisons through `<=>`, and other operators by method name on the value's class.
        let reachable = match (op, &res) {
            (BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr, r) => {
                matches!(r, Some(BinRes::Bitwise))
            }
            (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, Some(BinRes::Method(..))) => {
                // Typed code may use a `<` method; the generic comparison needs `<=>`.
                self.trial_binary(BinOp::Cmp, l, r).is_ok()
            }
            (_, Some(BinRes::Method(d, swapped))) => {
                let name = if op == BinOp::In { "contains" } else { op.symbol() };
                self.runtime_dispatchable(if *swapped { r } else { l }, name, d)
            }
            _ => true,
        };
        if reachable {
            Ok(t)
        } else {
            Err(true)
        }
    }

    /// The element type of `recv[index]` if typed code would accept it.
    fn trial_index(&mut self, recv: TyId, index: TyId, span: Span) -> Option<TyId> {
        let before = self.suppressed_errors;
        self.suppress_diags += 1;
        let t = self.index_result_ty(recv, index, span);
        self.suppress_diags -= 1;
        (self.suppressed_errors == before && !self.types.is_error(t)).then_some(t)
    }

    /// Whether the method that typed code resolves for `recv.name` (dispatched as `d`) is also
    /// the one found by name on the value's class at runtime. An implicit-generic body is compiled
    /// once and calls methods by name, so it reaches a class's own methods and getters, and an
    /// extension of a class or enum when it is the only extension method of that name for it;
    /// interface default methods and extensions of other types are resolved statically only.
    pub fn runtime_dispatchable(&self, recv: TyId, name: &str, d: &Dispatch) -> bool {
        let f = match d {
            Dispatch::Virtual(..) | Dispatch::Dynamic(_, None) => return true,
            Dispatch::Dynamic(_, Some(_)) => return false,
            Dispatch::Static(f) => *f,
        };
        if self.fns[f as usize].is_default {
            return false;
        }
        if !self.is_ext_fn(f) {
            return true;
        }
        let mut t = self.types.shallow_resolve(recv);
        if let Ty::Frozen(i) = self.types.get(t) {
            t = self.types.shallow_resolve(*i);
        }
        let same_target = |target: TyId| match (self.types.get(t), self.types.get(target)) {
            (Ty::Class(a, _), Ty::Class(b, _)) => a == b || self.upcast_class(*a, *b),
            (Ty::Enum(a, _), Ty::Enum(b, _)) => a == b,
            _ => false,
        };
        let owners: Vec<FnId> = self
            .exts
            .iter()
            .filter(|x| !x.is_class_methods && same_target(x.target))
            .filter_map(|x| x.methods.get(name).copied())
            .collect();
        owners == [f]
    }

    /// Whether class `sub` is `sup` or inherits from it.
    fn upcast_class(&self, sub: ClassId, sup: ClassId) -> bool {
        let mut cur = Some(sub);
        while let Some(c) = cur {
            if c == sup {
                return true;
            }
            cur = self.classes.get(&c).and_then(|ci| ci.superclass).and_then(|t| match self.types.get(t) {
                Ty::Class(sc, _) => Some(*sc),
                _ => None,
            });
        }
        false
    }

    /// Element type of an iterable type (for `for` and implicit generics).
    pub fn elem_type(&mut self, t: TyId) -> Option<TyId> {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t).clone() {
            Ty::Class(builtin::LIST | builtin::SET | builtin::CHANNEL, a) => Some(a[0]),
            Ty::Class(builtin::DICT, a) => Some(self.types.tuple(vec![a[0], a[1]])),
            Ty::Class(builtin::RANGE, _) => Some(self.types.int),
            Ty::Class(builtin::STRING, _) => Some(self.types.string),
            Ty::Frozen(inner) => {
                let e = self.elem_type(inner)?;
                Some(self.freeze_ty(e))
            }
            Ty::Dyn => Some(self.types.dyn_),
            _ => {
                let m = self.lookup_method(t, "each", false)?;
                let (ps, _) = self.instantiate_sig(&m);
                match ps.first().map(|p| self.types.get(self.types.shallow_resolve(p.ty)).clone()) {
                    Some(Ty::Fn(fps, _)) if fps.len() == 1 => Some(fps[0]),
                    _ => None,
                }
            }
        }
    }

    // ---- members -----------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn check_member(
        &mut self,
        e: &Expr,
        recv: &Expr,
        name: &ast::Ident,
        args: &[Arg],
        block: Option<&Expr>,
        parens: bool,
        safe: bool,
        expected: Option<TyId>,
    ) -> TyId {
        // `.Variant` shorthand.
        if let ExprKind::Ident(n) = &recv.kind {
            if n == "_" {
                return self.check_shorthand_variant(e, name, args, expected);
            }
            if n == "super" && self.lookup_local("super").is_none() {
                return self.check_super_call(e, recv, name, args, block, expected);
            }
        }
        if let Some(path) = self.resolve_path(recv) {
            match path {
                PathRes::Class(c, targs) => {
                    return self.check_static_member(e, c, targs, name, args, block, parens, expected)
                }
                PathRes::Enum(en, targs) => {
                    if let Some(tag) = self.enums[en as usize].variants.iter().position(|v| v.name == name.name) {
                        if !parens && self.enums[en as usize].variants[tag].fields.is_empty() {
                            return self.construct_variant(e, en, tag as u32, targs, &[], expected);
                        }
                        return self.construct_variant(e, en, tag as u32, targs, args, expected);
                    }
                    if let Some(&f) = self.enums[en as usize].methods.get(&name.name) {
                        if self.fns[f as usize].self_kind == ast::SelfKind::None {
                            let n = self.enums[en as usize].params.len();
                            let ta = targs.unwrap_or_else(|| (0..n).map(|_| self.types.fresh_var()).collect());
                            let params = self.enums[en as usize].params.clone();
                            let subst = params.into_iter().zip(ta.iter().copied()).collect();
                            let self_ty = self.types.intern(Ty::Enum(en, ta));
                            let m = MethodRef {
                                fid: f,
                                subst,
                                self_ty,
                                dispatch: Dispatch::Static(f),
                                frozen: false,
                                alts: Vec::new(),
                            };
                            let (slots, ret) = self.check_args(e, &m, args, block, expected, false);
                            self.calls.insert(
                                e.id,
                                CallRes::Fn { dispatch: Dispatch::Static(f), recv: false, args: slots, safe: false },
                            );
                            return ret;
                        }
                    }
                    // `E.values()`: every variant, when none has a payload (an enum's own static
                    // `values` takes precedence, above).
                    if name.name == "values" && args.is_empty() && block.is_none() {
                        let ei = &self.enums[en as usize];
                        if let Some(v) = ei.variants.iter().find(|v| !v.fields.is_empty()) {
                            let msg = format!(
                                "`{}.values()` needs payload-less variants, but `{}` has a payload",
                                ei.name, v.name
                            );
                            self.err("E0367", msg, name.span);
                            return self.types.error;
                        }
                        let classes: Vec<ClassId> = ei.variants.iter().map(|v| v.class_id).collect();
                        let n = ei.params.len();
                        let ta = targs.unwrap_or_else(|| (0..n).map(|_| self.types.fresh_var()).collect());
                        let et = self.types.intern(Ty::Enum(en, ta));
                        self.calls.insert(e.id, CallRes::EnumValues(classes));
                        return self.types.intern(Ty::Class(builtin::LIST, vec![et]));
                    }
                    let msg = format!("enum `{}` has no variant `{}`", self.enums[en as usize].name, name.name);
                    let mut d = Diag::error("E0343", msg, name.span);
                    let names: Vec<String> = self.enums[en as usize].variants.iter().map(|v| v.name.clone()).collect();
                    if let Some(s) = crate::util::suggest(&name.name, names.iter().map(|s| s.as_str())) {
                        d = d.suggest(format!("did you mean `{s}`?"), name.span, s);
                    }
                    self.emit(d);
                    return self.types.error;
                }
                PathRes::Module(m) => {
                    let Some(si) = self.mods[m as usize].items.get(&name.name).copied() else {
                        let msg = format!("module `{}` has no item `{}`", self.mods[m as usize].name, name.name);
                        self.err("E0203", msg, name.span);
                        return self.types.error;
                    };
                    if !self.item_is_pub(si) {
                        let msg = format!("`{}` is private to module `{}`", name.name, self.mods[m as usize].name);
                        self.err("E0202", msg, name.span);
                    }
                    match si {
                        ScopeItem::Fn(f) => {
                            if !parens && args.is_empty() && block.is_none() {
                                self.calls.insert(e.id, CallRes::FnValue(f));
                                return self.fn_value_ty(f, e.span);
                            }
                            if self.fns[f as usize].has_untyped_params {
                                return self.call_implicit(e, f, args, block);
                            }
                            let m = MethodRef {
                                fid: f,
                                subst: FxHashMap::default(),
                                self_ty: self.types.error,
                                dispatch: Dispatch::Static(f),
                                frozen: false,
                                alts: Vec::new(),
                            };
                            let (slots, ret) = self.check_args(e, &m, args, block, expected, false);
                            self.calls.insert(
                                e.id,
                                CallRes::Fn { dispatch: Dispatch::Static(f), recv: false, args: slots, safe: false },
                            );
                            return ret;
                        }
                        ScopeItem::Global(g) => {
                            self.ensure_global(g);
                            self.calls.insert(e.id, CallRes::Global(g));
                            return self.globals[g as usize].ty;
                        }
                        ScopeItem::Variant(en, tag) => return self.construct_variant(e, en, tag, None, args, expected),
                        _ => {
                            self.err("E0307", format!("`{}` is a type, not a value", name.name), name.span);
                            return self.types.error;
                        }
                    }
                }
                PathRes::Global(g) => {
                    // `Class.CONST` reached through resolve_path on the member itself is handled
                    // below; here the receiver is a global value.
                    let _ = g;
                }
                PathRes::Fn(_) | PathRes::Variant(..) | PathRes::Iface(_) => {}
            }
        }
        let rt0 = self.check_expr(recv, None);
        let mut rt = self.types.shallow_resolve(rt0);
        let mut optional_result = false;
        if safe {
            match self.types.strip_nil(rt) {
                Some(inner) => {
                    rt = inner;
                    optional_result = true;
                }
                None => {
                    if !self.is_dynamic(rt) {
                        let msg = format!("`?.` on a value of type `{}`, which is never nil", self.show(rt));
                        self.emit(Diag::warning("W0003", msg, e.span));
                    }
                }
            }
        } else if self.types.is_nilable(rt) && rt != self.types.nil && !self.has_optional_method(rt, &name.name) {
            let msg = format!("value of type `{}` may be nil", self.show(rt));
            let dot = Span::new(e.span.file, recv.span.end, recv.span.end);
            let note = match self.field_path(recv) {
                Some(((root, fields), _)) => {
                    let path = format!("{}.{}", self.locals[root as usize].name, fields.join("."));
                    let var = fields.last().cloned().unwrap_or_default();
                    format!(
                        "`!= nil` narrows a field only until a call, a loop or a closure could change it; \
                         bind it instead: `if let {var} = {path} {{ ... }}` (or use `?.`, `??` or `!`)"
                    )
                }
                None => self
                    .loop_dropped_note(recv)
                    .unwrap_or_else(|| "check `!= nil`, use `if let`, `?.`, `??` or `!`".to_string()),
            };
            self.emit(
                Diag::error("E0310", msg, recv.span)
                    .fix("use safe navigation", dot, "?")
                    .fix("assert it is not nil", dot, "!")
                    .note(note),
            );
            self.check_args_loose(args, block);
            return self.types.error;
        }
        let t = self.member_on(e, recv, rt, name, args, block, parens, expected);
        // A field path narrowed by an earlier test (`if n.next != nil { n.next.v }`).
        if !optional_result {
            if let Some((p, _)) = self.field_path(e) {
                if let Some(nt) = self.path_ty(&p) {
                    return nt;
                }
            }
        }
        if optional_result {
            if let Some(CallRes::Fn { safe, .. }) | Some(CallRes::Field { safe, .. }) = self.calls.get_mut(&e.id) {
                *safe = true;
            }
            if self.is_never(t) || self.types.is_nilable(t) {
                return t;
            }
            return self.types.optional(t);
        }
        t
    }

    fn check_args_loose(&mut self, args: &[Arg], block: Option<&Expr>) {
        for a in args {
            self.check_expr(&a.value, None);
        }
        if let Some(b) = block {
            self.check_expr(b, None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn member_on(
        &mut self,
        e: &Expr,
        recv: &Expr,
        rt: TyId,
        name: &ast::Ident,
        args: &[Arg],
        block: Option<&Expr>,
        parens: bool,
        expected: Option<TyId>,
    ) -> TyId {
        let plain = !parens && args.is_empty() && block.is_none();
        match self.types.get(rt).clone() {
            Ty::Dyn => {
                self.check_args_loose(args, block);
                if plain {
                    self.calls.insert(e.id, CallRes::DynMember(name.name.clone()));
                } else {
                    let slots = (0..args.len()).map(ArgSlot::Arg).chain(block.map(|_| ArgSlot::Block)).collect();
                    self.calls.insert(
                        e.id,
                        CallRes::Fn {
                            dispatch: Dispatch::Dynamic(name.name.clone(), None),
                            recv: true,
                            args: slots,
                            safe: false,
                        },
                    );
                }
                return self.types.dyn_;
            }
            Ty::Error => {
                self.check_args_loose(args, block);
                return self.types.error;
            }
            Ty::Frozen(inner) if matches!(self.types.get(self.types.shallow_resolve(inner)), Ty::Open(_)) => {
                let Ty::Open(o) = self.types.get(self.types.shallow_resolve(inner)).clone() else { unreachable!() };
                let pname = self.opens[o as usize].param_name.clone();
                let msg = format!(
                    "cannot use `{}` on a frozen value derived from untyped parameter `{pname}`; annotate its type",
                    name.name
                );
                self.err("E0369", msg, name.span);
                self.check_args_loose(args, block);
                return self.types.error;
            }
            Ty::Open(o) => {
                let tys: Vec<TyId> = args.iter().map(|a| self.check_expr(&a.value, None)).collect();
                let mut tys = tys;
                if let Some(b) = block {
                    tys.push(self.check_expr(b, None));
                }
                let named = args.iter().map(|a| a.name.as_ref().map(|n| n.name.clone())).collect();
                let ret = self.new_open(o);
                self.push_req(
                    o,
                    OpenReq::Method {
                        name: name.name.clone(),
                        args: tys,
                        named,
                        block: block.is_some(),
                        ret,
                        span: e.span,
                    },
                );
                let slots = (0..args.len()).map(ArgSlot::Arg).chain(block.map(|_| ArgSlot::Block)).collect();
                self.calls.insert(
                    e.id,
                    CallRes::Fn {
                        dispatch: Dispatch::Dynamic(name.name.clone(), None),
                        recv: true,
                        args: slots,
                        safe: false,
                    },
                );
                return ret;
            }
            Ty::Tuple(ts) if plain => {
                if let Ok(i) = name.name.parse::<usize>() {
                    if i < ts.len() {
                        self.calls.insert(e.id, CallRes::TupleField(i as u32));
                        return ts[i];
                    }
                    let msg = format!("tuple has {} elements; `.{i}` is out of range", ts.len());
                    self.err("E0344", msg, name.span);
                    return self.types.error;
                }
            }
            Ty::Var(_) => {
                let msg = format!(
                    "the type of this value must be known before calling `{}`; add a type annotation",
                    name.name
                );
                self.err("E0345", msg, recv.span);
                self.check_args_loose(args, block);
                return self.types.error;
            }
            _ => {}
        }
        if plain {
            if let Some((idx, fty)) = self.lookup_field(rt, &name.name) {
                self.calls.insert(e.id, CallRes::Field { index: idx, safe: false });
                return fty;
            }
        }
        let Some(m) = self.lookup_method(rt, &name.name, true) else {
            self.no_method_error(rt, name);
            self.check_args_loose(args, block);
            return self.types.error;
        };
        if !m.alts.is_empty() && !self.check_union_method(&m, rt, name) {
            self.check_args_loose(args, block);
            return self.types.error;
        }
        let f = self.fns[m.fid as usize].clone();
        // For a union receiver, `check_union_method` did these checks for each member.
        let single = m.alts.is_empty();
        if single && f.self_kind == ast::SelfKind::None {
            let msg = format!("`{}` is a static function; call it as `{}.{}(...)`", f.name, self.show(rt), f.name);
            self.err("E0346", msg, name.span);
            self.check_args_loose(args, block);
            return self.types.error;
        }
        if single && m.frozen {
            if f.mutating {
                let msg = format!("cannot call mutating method `{}` on frozen `{}`", f.name, self.show(rt));
                self.emit(
                    Diag::error("E0614", msg, name.span)
                        .note("frozen values are deeply immutable; use `.dup()` for a mutable copy"),
                );
            } else if !f.is_native {
                self.frozen_calls.push((name.span, m.fid, self.show(rt)));
            }
        }
        let parallel = matches!(f.name.as_str(), "par_map" | "par_each" | "par_reduce" | "par_select")
            && matches!(self.types.get(rt), Ty::Class(builtin::LIST, _) | Ty::Frozen(_));
        if parallel {
            if let Some(et) = self.elem_type(rt) {
                if !self.is_sendable(et) {
                    let msg = format!(
                        "`{}` runs its block on other threads, but the elements (`{}`) are owned by this thread",
                        f.name,
                        self.show(et)
                    );
                    self.emit(Diag::error("E0615", msg, name.span).note(
                        "freeze the list first (`freeze(list).par_map {...}`) or use frozen/shared element types",
                    ));
                }
            }
        }
        let (slots, ret) = self.check_args(e, &m, args, block, expected, parallel);
        if parallel {
            // The block's results come back from the other threads.
            self.check_not_pinned(ret, "the result", e.span);
        }
        self.note_self_call(&m, recv);
        if matches!(m.dispatch, Dispatch::Dynamic(..)) && matches!(f.owner, Owner::Iface(_)) {
            self.dyn_targets.insert(e.id, m.fid);
        }
        self.calls.insert(e.id, CallRes::Fn { dispatch: m.dispatch.clone(), recv: true, args: slots, safe: false });
        ret
    }

    /// Records calls on `self` (for mutating-method inference).
    pub fn note_self_call(&mut self, m: &MethodRef, recv: &Expr) {
        let on_self = match &recv.kind {
            ExprKind::SelfValue => true,
            ExprKind::Member { recv: r, parens: false, .. } => matches!(r.kind, ExprKind::SelfValue),
            ExprKind::Index { recv: r, .. } => {
                matches!(r.kind, ExprKind::SelfValue)
                    || matches!(&r.kind, ExprKind::Member { recv: rr, .. } if matches!(rr.kind, ExprKind::SelfValue))
            }
            _ => false,
        };
        if !on_self {
            return;
        }
        let caller = self.ctxs[self.return_ctx()].fn_id;
        let is_direct_self = matches!(recv.kind, ExprKind::SelfValue);
        let callee = &self.fns[m.fid as usize];
        if callee.mutating && (!is_direct_self || callee.is_native) {
            self.fns[caller as usize].mutating = true;
        } else if is_direct_self {
            self.self_calls.push((caller, m.fid));
        } else if callee.mutating {
            self.fns[caller as usize].mutating = true;
        }
    }

    fn no_method_error(&mut self, rt: TyId, name: &ast::Ident) {
        let mut names: Vec<String> = Vec::new();
        if let Ty::Class(c, _) = self.types.get(self.types.shallow_resolve(rt)).clone() {
            let mut cur = Some(c);
            while let Some(ci) = cur.and_then(|c| self.classes.get(&c)) {
                names.extend(ci.methods.keys().cloned());
                names.extend(ci.fields.iter().map(|f| f.name.clone()));
                cur = ci.superclass.and_then(|t| match self.types.get(t) {
                    Ty::Class(c, _) => Some(*c),
                    _ => None,
                });
            }
        }
        for i in &self.ifaces {
            names.extend(i.by_name.keys().cloned());
        }
        // On a union, name the members that lack it.
        if let Ty::Union(parts) = self.types.get(self.types.shallow_resolve(rt)).clone() {
            let cp = self.checkpoint();
            self.suppress_diags += 1;
            let missing: Vec<TyId> = parts
                .into_iter()
                .filter(|&p| {
                    self.lookup_field(p, &name.name).is_none() && self.lookup_method(p, &name.name, true).is_none()
                })
                .collect();
            self.suppress_diags -= 1;
            self.rollback(cp);
            if !missing.is_empty() && !(missing.len() == 1 && missing[0] == self.types.nil) {
                let shown: Vec<String> = missing.iter().map(|&p| format!("`{}`", self.show(p))).collect();
                let msg = format!(
                    "no method or field `{}` on {}, so not on every member of `{}`",
                    name.name,
                    shown.join(" or "),
                    self.show(rt)
                );
                self.emit(Diag::error("E0311", msg, name.span).note(
                    "a member of a union value must exist on every member type; narrow it first with `match` or `as?`",
                ));
                return;
            }
        }
        let msg = format!("no method or field `{}` on `{}`", name.name, self.show(rt));
        let mut d = Diag::error("E0311", msg, name.span);
        if let Some((req, actual)) = self.unmet_ext_bound(rt, &name.name) {
            let iface = req.split_once(": ").map_or(req.as_str(), |(_, b)| b);
            d = d.note(format!("`{}` needs `{req}`, and `{actual}` is not `{iface}`", name.name));
            self.emit(d);
            return;
        }
        if let Some(s) = crate::util::suggest(&name.name, names.iter().map(|s| s.as_str())) {
            d = d.suggest(format!("did you mean `{s}`?"), name.span, s);
        }
        self.emit(d);
    }

    #[allow(clippy::too_many_arguments)]
    fn check_static_member(
        &mut self,
        e: &Expr,
        cid: ClassId,
        targs: Option<Vec<TyId>>,
        name: &ast::Ident,
        args: &[Arg],
        block: Option<&Expr>,
        parens: bool,
        expected: Option<TyId>,
    ) -> TyId {
        let ci = self.classes[&cid].clone();
        if let Some(&g) = ci.consts.get(&name.name) {
            if !parens && args.is_empty() {
                self.ensure_global(g);
                self.calls.insert(e.id, CallRes::Global(g));
                return self.globals[g as usize].ty;
            }
        }
        // Inferred type arguments are checked against the class's bounds, unless the expected
        // type (an annotation, a parameter type, a field type), which is checked where it is
        // written, already decides them.
        let from_expected = targs.is_none()
            && expected.is_some_and(|x| {
                let x = self.types.strip_nil(x).unwrap_or(x);
                matches!(self.types.get(self.types.shallow_resolve(x)), Ty::Class(c, _) if *c == cid)
            });
        let targs = match targs {
            Some(t) => {
                if t.len() != ci.params.len() {
                    let msg = format!("`{}` takes {} type arguments", ci.name, ci.params.len());
                    self.err("E0214", msg, e.span);
                    return self.types.error;
                }
                t
            }
            None => ci.params.iter().map(|_| self.types.fresh_var()).collect(),
        };
        let self_ty = self.types.class(cid, targs.clone());
        if !from_expected {
            self.note_instance(&ci.name, &ci.params, &targs, e.span);
        }
        let subst: FxHashMap<ParamId, TyId> = ci.params.iter().copied().zip(targs.iter().copied()).collect();
        if name.name == "new" && !ci.is_builtin {
            let ctor = ci.constructor.unwrap();
            if ci.modifier != ast::ClassModifier::Shared && ci.methods.contains_key("new") {
                // A user static `new` overrides the generated constructor (the standard library's
                // frozen `Regex` builds its instances natively this way).
            } else {
                let m = MethodRef {
                    fid: ctor,
                    subst,
                    self_ty,
                    dispatch: Dispatch::Static(ctor),
                    frozen: false,
                    alts: Vec::new(),
                };
                let (slots, ret) = self.check_args(e, &m, args, block, expected, false);
                self.calls.insert(e.id, CallRes::Construct { class: cid, ctor, args: slots });
                return ret;
            }
        }
        // Static method (searching superclasses too).
        let mut cur = Some(cid);
        while let Some(c) = cur {
            let info = self.classes[&c].clone();
            if let Some(&f) = info.methods.get(&name.name) {
                if self.fns[f as usize].self_kind != ast::SelfKind::None {
                    let msg = format!("`{}` is an instance method; call it on a value", name.name);
                    self.err("E0347", msg, name.span);
                    self.check_args_loose(args, block);
                    return self.types.error;
                }
                let view = self.upcast(self_ty, c).unwrap_or(self_ty);
                let sub: FxHashMap<ParamId, TyId> = match self.types.get(view).clone() {
                    Ty::Class(_, a) => info.params.iter().copied().zip(a).collect(),
                    _ => subst.clone(),
                };
                let m = MethodRef {
                    fid: f,
                    subst: sub,
                    self_ty,
                    dispatch: Dispatch::Static(f),
                    frozen: false,
                    alts: Vec::new(),
                };
                let (slots, ret) = self.check_args(e, &m, args, block, expected, false);
                self.calls
                    .insert(e.id, CallRes::Fn { dispatch: Dispatch::Static(f), recv: false, args: slots, safe: false });
                return ret;
            }
            cur = info.superclass.and_then(|t| match self.types.get(t) {
                Ty::Class(c, _) => Some(*c),
                _ => None,
            });
        }
        let msg = format!("`{}` has no static function `{}`", ci.name, name.name);
        let mut d = Diag::error("E0348", msg, name.span);
        let names: Vec<String> = ci.methods.keys().cloned().chain(std::iter::once("new".to_string())).collect();
        if let Some(s) = crate::util::suggest(&name.name, names.iter().map(|s| s.as_str())) {
            d = d.suggest(format!("did you mean `{s}`?"), name.span, s);
        }
        self.emit(d);
        self.check_args_loose(args, block);
        self.types.error
    }

    fn check_shorthand_variant(&mut self, e: &Expr, name: &ast::Ident, args: &[Arg], expected: Option<TyId>) -> TyId {
        let exp = expected.map(|t| {
            let t = self.types.shallow_resolve(t);
            self.types.strip_nil(t).unwrap_or(t)
        });
        if let Some(t) = exp {
            if let Ty::Enum(en, _) = self.types.get(t).clone() {
                if let Some(tag) = self.enums[en as usize].variants.iter().position(|v| v.name == name.name) {
                    // The type arguments come from `expected`.
                    return self.construct_variant(e, en, tag as u32, None, args, expected);
                }
                let msg = format!("enum `{}` has no variant `{}`", self.enums[en as usize].name, name.name);
                self.err("E0343", msg, name.span);
                return self.types.error;
            }
        }
        let msg = format!("cannot infer which enum `.{}` belongs to; write `EnumName.{}`", name.name, name.name);
        self.err("E0349", msg, e.span);
        self.check_args_loose(args, None);
        self.types.error
    }

    fn check_super_call(
        &mut self,
        e: &Expr,
        recv: &Expr,
        name: &ast::Ident,
        args: &[Arg],
        block: Option<&Expr>,
        expected: Option<TyId>,
    ) -> TyId {
        let ci = self.return_ctx();
        let fid = self.ctxs[ci].fn_id;
        let Owner::Class(cid) = self.fns[fid as usize].owner else {
            self.err("E0350", "`super` is only available in class methods", e.span);
            return self.types.error;
        };
        let self_ty = self.fns[fid as usize].self_ty;
        let Some(sup) = self.classes[&cid].superclass else {
            let msg = format!("`{}` has no superclass", self.classes[&cid].name);
            self.err("E0350", msg, e.span);
            return self.types.error;
        };
        let sup_cid = match self.types.get(sup) {
            Ty::Class(c, _) => *c,
            _ => return self.types.error,
        };
        let view = self.upcast(self_ty, sup_cid).unwrap_or(sup);
        let Some(mut m) = self.lookup_method(view, &name.name, false) else {
            self.no_method_error(view, name);
            return self.types.error;
        };
        // Always a direct call to the superclass implementation.
        m.dispatch = Dispatch::Static(m.fid);
        m.self_ty = self_ty;
        if let Some(s) = self.capture_self() {
            self.names.insert(recv.id, NameRes::Local(s));
        }
        let (slots, ret) = self.check_args(e, &m, args, block, expected, false);
        self.calls
            .insert(e.id, CallRes::Fn { dispatch: Dispatch::Static(m.fid), recv: true, args: slots, safe: false });
        let callee_mut = self.fns[m.fid as usize].mutating;
        if callee_mut {
            self.fns[fid as usize].mutating = true;
        } else {
            self.self_calls.push((fid, m.fid));
        }
        ret
    }

    // ---- indexing ----------------------------------------------------------------------------

    fn check_index(&mut self, e: &Expr, recv: &Expr, args: &[Expr]) -> TyId {
        if self.resolve_path(recv).is_some() {
            if let Some(PathRes::Global(_)) = self.resolve_path(recv) {
                // A constant indexed: fall through to value indexing.
            } else {
                self.err("E0351", "a type is not a value; call a static function such as `.new()`", e.span);
                return self.types.error;
            }
        }
        let rt = self.check_expr(recv, None);
        if args.len() != 1 {
            self.err("E0328", "indexing takes exactly one index", e.span);
            for a in args {
                self.check_expr(a, None);
            }
            return self.types.error;
        }
        let rtr = self.types.shallow_resolve(rt);
        if let Ty::Open(o) = self.types.get(rtr).clone() {
            let it = self.check_expr(&args[0], None);
            let ret = self.new_open(o);
            self.push_req(o, OpenReq::Index { args: vec![it], ret, span: e.span });
            self.calls.insert(e.id, CallRes::IndexGet);
            return ret;
        }
        if self.types.is_nilable(rtr) && rtr != self.types.nil {
            let msg = format!("value of type `{}` may be nil", self.show(rt));
            let mut d = Diag::error("E0310", msg, recv.span).fix("assert it is not nil", recv.span.after(), "!");
            if let Some(n) = self.loop_dropped_note(recv) {
                d = d.note(n);
            }
            self.emit(d);
            self.check_expr(&args[0], None);
            return self.types.error;
        }
        // Tuples need a literal index.
        if let Ty::Tuple(ts) = self.types.get(rtr).clone() {
            if let ExprKind::Int(s) = &args[0].kind {
                if let Ok(i) = s.parse::<usize>() {
                    if i < ts.len() {
                        self.record_ty(args[0].id, self.types.int);
                        self.calls.insert(e.id, CallRes::TupleField(i as u32));
                        return ts[i];
                    }
                }
            }
            self.err("E0352", "tuples are indexed with a literal: `t[0]` or `t.0`", args[0].span);
            return self.types.error;
        }
        let hint = self.index_param_hint(rtr);
        let it = self.check_expr(&args[0], hint);
        let t = self.index_result_ty(rtr, it, e.span);
        let int = self.types.int;
        let is_list = matches!(self.types.get(rtr), Ty::Class(builtin::LIST, _))
            || matches!(self.types.get(rtr), Ty::Frozen(i) if matches!(self.types.get(*i), Ty::Class(builtin::LIST, _)));
        if is_list && self.types.shallow_resolve(it) == int {
            self.calls.insert(e.id, CallRes::ListGet);
        } else {
            self.calls.entry(e.id).or_insert(CallRes::IndexGet);
        }
        t
    }

    fn index_param_hint(&mut self, rt: TyId) -> Option<TyId> {
        match self.types.get(rt).clone() {
            Ty::Class(builtin::DICT, a) => Some(a[0]),
            Ty::Class(builtin::LIST | builtin::STRING, _) => None,
            _ => None,
        }
    }

    /// Result type of `recv[idx]`, recording a method call for user `[]`.
    pub fn index_result_ty(&mut self, rt: TyId, it: TyId, span: Span) -> TyId {
        let rt = self.types.shallow_resolve(rt);
        let (int, range) = (self.types.int, self.types.range);
        let it_r = self.types.shallow_resolve(it);
        let (base, frozen) = match self.types.get(rt).clone() {
            Ty::Frozen(i) => (self.types.shallow_resolve(i), true),
            _ => (rt, false),
        };
        let r = match self.types.get(base).clone() {
            Ty::Class(builtin::LIST, a) => {
                if it_r == range {
                    Some(base)
                } else if self.assignable(it, int) {
                    Some(a[0])
                } else {
                    None
                }
            }
            Ty::Class(builtin::STRING, _) => {
                if it_r == range || self.assignable(it, int) {
                    Some(self.types.string)
                } else {
                    None
                }
            }
            Ty::Class(builtin::DICT, a) => {
                if self.assignable(it, a[0]) {
                    Some(self.types.optional(a[1]))
                } else {
                    let msg = format!("dict key must be `{}`, found `{}`", self.show(a[0]), self.show(it));
                    self.err("E0353", msg, span);
                    return self.types.error;
                }
            }
            Ty::Dyn => return self.types.dyn_,
            Ty::Error => return self.types.error,
            _ => match self.lookup_method(rt, "[]", false) {
                Some(m) => {
                    let (ps, ret) = self.instantiate_sig(&m);
                    if let Some(p) = ps.first() {
                        if !self.assignable(it, p.ty) {
                            let msg = format!("index must be `{}`, found `{}`", self.show(p.ty), self.show(it));
                            self.err("E0353", msg, span);
                        }
                    }
                    return ret;
                }
                None => {
                    let msg = format!("`{}` cannot be indexed", self.show(rt));
                    self.err("E0354", msg, span);
                    return self.types.error;
                }
            },
        };
        match r {
            Some(t) => {
                if frozen {
                    self.freeze_ty(t)
                } else {
                    t
                }
            }
            None => {
                let msg = format!("cannot index `{}` with `{}`", self.show(rt), self.show(it));
                self.err("E0353", msg, span);
                self.types.error
            }
        }
    }

    // ---- closures ----------------------------------------------------------------------------

    /// Checks a closure literal. `spawn_like` = Some(is_move) for parallel blocks.
    /// W0006: `return value` in a block whose result is discarded (`xs.each { |x| return x }`):
    /// it ends only this call of the block, and the value is dropped.
    fn lint_discarded_returns(&mut self, body: &Block) {
        let mut stack: Vec<&Expr> = body.stmts.iter().collect();
        let mut found = Vec::new();
        while let Some(x) = stack.pop() {
            match &x.kind {
                // Their `return`s belong to them.
                ExprKind::Closure { .. } | ExprKind::Spawn { .. } => continue,
                ExprKind::Return(Some(v)) if !matches!(v.kind, ExprKind::Nil) => found.push(x.span),
                _ => {}
            }
            stack.extend(crate::query::children(x));
        }
        found.sort_by_key(|s| s.start);
        for span in found {
            self.emit(
                Diag::warning("W0006", "`return` with a value in a block whose result is discarded", span)
                    .note("`return` in a block ends only this call of the block, and the value is dropped")
                    .note("to return from the function, use a `for` loop (where `return` leaves the function), or `find` to get the first match"),
            );
        }
    }

    pub fn check_closure(
        &mut self,
        e: &Expr,
        params: &[ClosureParam],
        ret: Option<&TypeExpr>,
        body: &Block,
        expected: Option<TyId>,
        spawn_like: Option<bool>,
    ) -> TyId {
        self.check_unique_names(params.iter().map(|p| &p.name), "block parameter");
        let exp_fn = expected.and_then(|t| match self.types.get(self.types.shallow_resolve(t)).clone() {
            Ty::Fn(ps, r) => Some((ps, r)),
            // An optional block parameter (`f: (fn(T) -> U)? = nil`) types the block as its function.
            Ty::Union(ms) => {
                let mut fns = ms.iter().filter_map(|&m| match self.types.get(self.types.shallow_resolve(m)) {
                    Ty::Fn(ps, r) => Some((ps.clone(), *r)),
                    _ => None,
                });
                let only_nil_besides =
                    ms.iter().filter(|&&m| self.types.shallow_resolve(m) != self.types.nil).count() == 1;
                match (fns.next(), fns.next()) {
                    (Some(f), None) if only_nil_besides => Some(f),
                    _ => None,
                }
            }
            _ => None,
        });
        let mut ptys: Vec<TyId> = Vec::new();
        let errors_before = self.diags.len();
        let mut destructure = false;
        let mut implicit_it = false;
        match &exp_fn {
            Some((eps, _)) => {
                if params.is_empty() && eps.len() == 1 {
                    implicit_it = true;
                    ptys.push(eps[0]);
                } else if params.len() > 1 && eps.len() == 1 {
                    match self.types.get(self.types.shallow_resolve(eps[0])).clone() {
                        Ty::Tuple(ts) if ts.len() == params.len() => {
                            destructure = true;
                            ptys = ts;
                        }
                        _ => {
                            let msg = format!("this block takes {} parameters, but 1 is expected", params.len());
                            self.err("E0355", msg, e.span);
                            ptys = vec![self.types.error; params.len()];
                        }
                    }
                } else if params.len() != eps.len() {
                    let msg = format!("this block takes {} parameter(s), but {} expected", params.len(), eps.len());
                    self.err("E0355", msg, e.span);
                    ptys = params.iter().map(|_| self.types.error).collect();
                } else {
                    ptys = eps.clone();
                }
            }
            None => {
                for p in params {
                    if p.ty.is_none() {
                        let msg = format!("cannot infer the type of block parameter `{}`", p.name.name);
                        self.emit(Diag::error("E0356", msg, p.name.span).suggest(
                            "annotate it",
                            p.name.span.after(),
                            ": Int",
                        ));
                        ptys.push(self.types.error);
                    } else {
                        ptys.push(self.types.error);
                    }
                }
            }
        }
        // Explicit annotations win (and must agree with the expected type).
        for (i, p) in params.iter().enumerate() {
            if let Some(te) = &p.ty {
                let t = self.resolve_type(te, &mut self.type_scope());
                if i < ptys.len() && !matches!(self.types.get(ptys[i]), Ty::Error) && !self.assignable(ptys[i], t) {
                    let msg = format!(
                        "block parameter `{}` is `{}` but `{}` is passed",
                        p.name.name,
                        self.show(t),
                        self.show(ptys[i])
                    );
                    self.err("E0357", msg, te.span);
                }
                if i < ptys.len() {
                    ptys[i] = t;
                }
            }
        }
        let ret_ty = match ret {
            Some(te) => self.resolve_type(te, &mut self.type_scope()),
            None => match &exp_fn {
                Some((_, r)) => *r,
                None => self.types.fresh_var(),
            },
        };
        let fid = self.new_closure_fn(e, spawn_like.map(|_| matches!(e.kind, ExprKind::Spawn { .. })));
        let kind = match spawn_like {
            Some(is_move) => CtxKind::Spawn { is_move },
            None => CtxKind::Closure,
        };
        let nil = self.types.nil;
        let ret_is_nil = self.types.shallow_resolve(ret_ty) == nil;
        let body_ret = if ret_is_nil { self.types.fresh_var() } else { ret_ty };
        self.push_ctx(fid, kind, body_ret, e.span);
        let pnames: Vec<&str> =
            if implicit_it { vec!["it"] } else { params.iter().map(|p| p.name.name.as_str()).collect() };
        self.ctx().scan = std::sync::Arc::new(AssignScan::new(&body.stmts, &pnames));
        let mut plocals = Vec::new();
        if implicit_it {
            plocals.push(self.new_local("it", ptys[0], e.span, true));
            self.ctx().scopes[0].insert("it".into(), *plocals.last().unwrap());
        } else {
            for (i, p) in params.iter().enumerate() {
                let l = self.new_local(&p.name.name, ptys[i], p.name.span, true);
                if p.name.name != "_" {
                    self.ctx().scopes[0].insert(p.name.name.clone(), l);
                }
                plocals.push(l);
            }
        }
        let bt = self.check_block(body, if ret_is_nil { None } else { Some(ret_ty) });
        if ret_is_nil && ret.is_none() {
            self.lint_discarded_returns(body);
        }
        let returns = self.ctx().returns.clone();
        let captures = self.ctx().captures.clone();
        self.ctxs.pop();
        let final_ret = if ret_is_nil {
            nil
        } else if matches!(self.types.get(self.types.shallow_resolve(ret_ty)), Ty::Var(_)) {
            let mut all = returns;
            all.push(bt);
            let j = self.join(&all);
            self.unify(ret_ty, j);
            ret_ty
        } else {
            if !self.assignable(bt, ret_ty) && !self.is_never(bt) {
                if self.try_int_to_float(bt, ret_ty) {
                    if let Some(last) = body.stmts.last() {
                        self.coerce.insert(last.id, Coercion::IntToFloat);
                    }
                } else {
                    let msg = format!("block should return `{}`, but returns `{}`", self.show(ret_ty), self.show(bt));
                    self.err("E0358", msg, body.span);
                }
            }
            ret_ty
        };
        {
            let f = &mut self.fns[fid as usize];
            f.params = ptys
                .iter()
                .zip(plocals.iter())
                .map(|(&t, _)| PInfo { name: String::new(), ty: t, has_default: false, is_move: false, span: e.span })
                .collect();
            f.ret = final_ret;
            f.state = CheckState::Done;
        }
        if let Some(is_move) = spawn_like {
            self.check_spawn_captures(e.id, &captures, is_move, e.span);
            self.spawn_nodes.insert(e.id, is_move);
        }
        let n_params = if destructure { 1 } else { plocals.len() };
        let _ = n_params;
        self.closures
            .insert(e.id, ClosureInfo { fn_id: fid, captures, params: plocals, destructure, is_for_body: false });
        let fn_params = if destructure { vec![self.types.tuple(ptys)] } else { ptys };
        // A reported parameter mismatch should not be repeated as an argument mismatch.
        if self.diags[errors_before..].iter().any(|d| matches!(d.code, "E0355" | "E0356" | "E0357")) {
            return self.types.error;
        }
        self.types.func(fn_params, final_ret)
    }

    /// A function for a closure body. `is_spawn`: None for a block, Some(true) for `spawn`,
    /// Some(false) for a parallel (`par_*`) block.
    fn new_closure_fn(&mut self, e: &Expr, is_spawn: Option<bool>) -> FnId {
        let id = self.fns.len() as FnId;
        let mut f = self.fns[self.ctxs.last().map(|c| c.fn_id).unwrap_or(0) as usize].clone();
        // Named after the enclosing function: `<block in f>`, `<spawn in main>`. A parallel
        // (`par_*`) block runs on worker threads, so its frame says so.
        let outer = match f.name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) {
            Some(inner) => inner.rsplit(" in ").next().unwrap_or(inner).to_string(),
            None => f.name.clone(),
        };
        f.name = match is_spawn {
            Some(true) => format!("<spawn in {outer}>"),
            Some(false) => format!("<parallel block in {outer}>"),
            None => format!("<block in {outer}>"),
        };
        f.owner = Owner::None;
        f.type_params = Vec::new();
        f.self_kind = ast::SelfKind::None;
        f.params = Vec::new();
        f.ret_annotated = true;
        f.is_native = false;
        f.native_name = None;
        f.decl = DeclRef::Closure(e.id);
        f.synth = Synth::None;
        f.span = e.span;
        f.implicit = None;
        f.has_untyped_params = false;
        f.state = CheckState::InProgress;
        f.func_id = id;
        f.is_default = false;
        f.module = self.cur_module;
        self.fns.push(f);
        id
    }

    /// Captured values of a parallel block must be safe to share or explicitly moved.
    fn check_spawn_captures(&mut self, node: NodeId, captures: &[LocalId], is_move: bool, span: Span) {
        for &l in captures {
            let info = self.locals[l as usize].clone();
            if info.name == "$frame" {
                continue;
            }
            let t = info.ty;
            if self.is_sendable(t) {
                continue;
            }
            if is_move {
                let what = format!("`{}`", info.name);
                self.check_not_pinned(t, &what, span);
                self.mark_moved(l, span);
                self.moved_captures.insert((node, l));
                continue;
            }
            let msg = format!(
                "`{}` (type `{}`) is owned by this thread and cannot be shared with a parallel block",
                info.name,
                self.show(t)
            );
            let help = if self.holds_fn(t) {
                // A frozen function cannot be called, and one in a `Mutex` would still run with
                // this thread's captures.
                "use `spawn move { ... }` to transfer it, or call it here and capture its result instead".to_string()
            } else {
                format!(
                    "use `spawn move {{ ... }}` to transfer it, `freeze({})` to share it read-only, or wrap it in `Mutex.new(...)`",
                    info.name
                )
            };
            self.emit(
                Diag::error("E0604", msg, span)
                    .note_at(format!("`{}` is captured here", info.name), info.span)
                    .note(help),
            );
        }
    }

    fn check_spawn(&mut self, e: &Expr, is_move: bool, body: &Block) -> TyId {
        let t = self.check_closure(e, &[], None, body, None, Some(is_move));
        let ret = match self.types.get(self.types.shallow_resolve(t)).clone() {
            Ty::Fn(_, r) => r,
            _ => self.types.error,
        };
        let ret = self.types.resolve(ret);
        // The result is handed to the thread that joins it.
        self.check_not_pinned(ret, "the thread's result", e.span);
        self.types.class(builtin::THREAD, vec![ret])
    }

    // ---- for loops ---------------------------------------------------------------------------

    fn check_for(&mut self, e: &Expr, pattern: &ast::Pattern, iter: &Expr, body: &Block) -> TyId {
        let it = self.check_expr(iter, None);
        self.loop_invalidate(body);
        // Iterating a builtin collection runs no user code; a user `each` may.
        let builtin_iter = {
            let r = self.types.shallow_resolve(it);
            let r = match self.types.get(r) {
                Ty::Frozen(i) => self.types.shallow_resolve(*i),
                _ => r,
            };
            matches!(
                self.types.get(r),
                Ty::Class(builtin::LIST | builtin::RANGE | builtin::DICT | builtin::SET | builtin::STRING, _)
                    | Ty::Error
            )
        };
        let mark = self.loop_enter(None, body, !builtin_iter);
        let itr = self.types.shallow_resolve(it);
        let (kind, elem) = match self.types.get(itr).clone() {
            Ty::Class(builtin::LIST, a) => (ForKind::List, a[0]),
            Ty::Frozen(inner)
                if matches!(self.types.get(self.types.shallow_resolve(inner)), Ty::Class(builtin::LIST, _)) =>
            {
                let et = self.elem_type(itr).unwrap();
                (ForKind::List, et)
            }
            Ty::Class(builtin::RANGE, _) => (ForKind::Range, self.types.int),
            Ty::Class(builtin::DICT, _) => {
                let m = self.lookup_method(itr, "entries", false).unwrap();
                let et = self.elem_type(itr).unwrap();
                (ForKind::Via(m.dispatch), et)
            }
            Ty::Class(builtin::SET, a) => {
                let m = self.lookup_method(itr, "to_list", false).unwrap();
                (ForKind::Via(m.dispatch), a[0])
            }
            Ty::Class(builtin::STRING, _) => {
                let m = self.lookup_method(itr, "chars", false).unwrap();
                (ForKind::Via(m.dispatch), self.types.string)
            }
            Ty::Open(o) => {
                let elem = self.new_open(o);
                self.push_req(o, OpenReq::Iter { elem, span: iter.span });
                (ForKind::Each(Dispatch::Dynamic("each".into(), None)), elem)
            }
            Ty::Dyn => (ForKind::Each(Dispatch::Dynamic("each".into(), None)), self.types.dyn_),
            Ty::Error => (ForKind::List, self.types.error),
            _ => match self.lookup_method(itr, "each", false) {
                Some(m) => {
                    let et = self.elem_type(itr).unwrap_or(self.types.error);
                    (ForKind::Each(m.dispatch), et)
                }
                None => {
                    let msg = format!("`{}` is not iterable (it has no `each` method)", self.show(it));
                    self.err("E0359", msg, iter.span);
                    (ForKind::List, self.types.error)
                }
            },
        };
        let is_each = matches!(kind, ForKind::Each(_));
        self.fors.insert(e.id, kind);
        if is_each {
            // The body becomes a block passed to `each`.
            let fid = self.new_closure_fn(e, None);
            let nil = self.types.nil;
            self.push_ctx(fid, CtxKind::ForBody, nil, e.span);
            let pnames = crate::check::scan::pattern_names_of(pattern);
            let pnames: Vec<&str> = pnames.iter().map(String::as_str).collect();
            self.ctx().scan = std::sync::Arc::new(AssignScan::new(&body.stmts, &pnames));
            self.ctx().loops.push(LoopCtx { breaks: Vec::new(), is_foreach: true });
            let param = self.new_local("$item", elem, pattern.span, true);
            self.bind_irrefutable(pattern, elem);
            self.check_block(body, None);
            let captures = self.ctx().captures.clone();
            self.ctxs.pop();
            let f = &mut self.fns[fid as usize];
            f.params = vec![PInfo { name: "$item".into(), ty: elem, has_default: false, is_move: false, span: e.span }];
            f.ret = nil;
            f.state = CheckState::Done;
            self.for_bodies.insert(
                e.id,
                ClosureInfo { fn_id: fid, captures, params: vec![param], destructure: false, is_for_body: true },
            );
        } else {
            self.ctx().loops.push(LoopCtx { breaks: Vec::new(), is_foreach: false });
            self.push_scope();
            self.bind_irrefutable(pattern, elem);
            self.check_block(body, None);
            self.pop_scope();
            self.ctx().loops.pop();
        }
        self.loop_exit(mark);
        self.types.nil
    }

    /// E0614 for each kept conversion of a frozen value to an interface whose members it
    /// implements with a mutating method: calling it through the interface would panic.
    fn check_frozen_conformances(&mut self) {
        let mut seen: FxHashSet<(Span, FnId)> = FxHashSet::default();
        for i in 0..self.conform_log.len() {
            let (s, iid) = self.conform_log[i];
            if !matches!(self.types.get(self.types.shallow_resolve(s)), Ty::Frozen(_)) {
                continue;
            }
            let span = self.conform_spans[i];
            let impls = self.conform_impls.get(&(s, iid)).cloned().unwrap_or_default();
            for (member, fid) in impls {
                if !self.fns[fid as usize].mutating || !seen.insert((span, fid)) {
                    continue;
                }
                let iface = self.ifaces[iid as usize].name.clone();
                let name = self.fns[member as usize].name.clone();
                let msg = format!("`{}` cannot be used as `{iface}`: its method `{name}` mutates it", self.show(s));
                let mut d = Diag::error("E0614", msg, span);
                if !self.mods[self.fns[fid as usize].module as usize].is_prelude {
                    d = d.note_at(format!("`{name}` is defined here"), self.fns[fid as usize].span);
                }
                self.emit(d.note("frozen values are deeply immutable; use `.dup()` for a mutable copy"));
            }
        }
    }

    /// Mutating-method inference: propagate through calls on `self`, then validate calls on
    /// frozen receivers.
    pub fn finish_mutating(&mut self) {
        // Methods that mutate only because an override does: the override.
        let mut via_override: FxHashMap<FnId, FnId> = FxHashMap::default();
        loop {
            let mut changed = false;
            for &(caller, callee) in &self.self_calls.clone() {
                if self.fns[callee as usize].mutating && !self.fns[caller as usize].mutating {
                    self.fns[caller as usize].mutating = true;
                    changed = true;
                }
            }
            // A call through the superclass may run an override that mutates.
            for &(parent, child) in &self.overrides.clone() {
                if self.fns[child as usize].mutating && !self.fns[parent as usize].mutating {
                    self.fns[parent as usize].mutating = true;
                    via_override.insert(parent, *via_override.get(&child).unwrap_or(&child));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.check_frozen_conformances();
        for (span, fid, ty) in std::mem::take(&mut self.frozen_calls) {
            if self.fns[fid as usize].mutating {
                let msg = format!("cannot call mutating method `{}` on frozen `{}`", self.fns[fid as usize].name, ty);
                let mut d = Diag::error("E0614", msg, span).note("frozen values are deeply immutable");
                if let Some(&o) = via_override.get(&fid) {
                    d = d.note_at("the call may run this override, which mutates `self`", self.fns[o as usize].span);
                }
                self.emit(d);
            }
        }
    }
}

/// Converts an expression written in type position into a type expression.
pub fn expr_to_type_expr(e: &Expr) -> Option<TypeExpr> {
    let kind = match &e.kind {
        ExprKind::Ident(n) => {
            TypeExprKind::Named { path: vec![ast::Ident { name: n.clone(), span: e.span }], args: vec![] }
        }
        ExprKind::Nil => {
            TypeExprKind::Named { path: vec![ast::Ident { name: "Nil".into(), span: e.span }], args: vec![] }
        }
        ExprKind::Index { recv, args } => {
            let base = expr_to_type_expr(recv)?;
            let TypeExprKind::Named { path, .. } = base.kind else { return None };
            let args = args.iter().map(expr_to_type_expr).collect::<Option<Vec<_>>>()?;
            TypeExprKind::Named { path, args }
        }
        ExprKind::Member { recv, name, args, parens: false, .. } if args.is_empty() => {
            let base = expr_to_type_expr(recv)?;
            let TypeExprKind::Named { mut path, args: a } = base.kind else { return None };
            if !a.is_empty() {
                return None;
            }
            path.push(name.clone());
            TypeExprKind::Named { path, args: vec![] }
        }
        ExprKind::Try(inner) => TypeExprKind::Optional(Box::new(expr_to_type_expr(inner)?)),
        ExprKind::Tuple(items) => TypeExprKind::Tuple(items.iter().map(expr_to_type_expr).collect::<Option<Vec<_>>>()?),
        ExprKind::Binary(BinOp::BitOr, l, r) => TypeExprKind::Union(vec![expr_to_type_expr(l)?, expr_to_type_expr(r)?]),
        _ => return None,
    };
    Some(TypeExpr { kind, span: e.span })
}

/// Why an operator typed code accepts is not available through an untyped parameter.
fn generic_op_hint(op: BinOp, typed: bool) -> &'static str {
    if !typed {
        return "";
    }
    match op {
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
            " (through an untyped parameter, bitwise operators apply to `Int` only)"
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            " (through an untyped parameter, comparisons use the type's own `<=>`)"
        }
        _ => " (the operator is an extension or interface default method, which an untyped parameter cannot reach)",
    }
}

/// What `Checker::guess_value` can tell about a value before it is checked.
enum ValueGuess {
    Ty(TyId),
    NonNil,
    Unknown,
}
