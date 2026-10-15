//! State protected by a `Mutex` or held by an `Atomic` must not escape to unsynchronized code
//! (issue #162). A `Mutex` guards its value only inside `lock`: a reference to an owned value
//! taken out of it lets threads change the value at the same time, and updates are silently
//! lost. The rules (reference §8.1):
//!
//! - `Mutex.get` and `Mutex.update` need a frozen or shareable `T` (E0616): `get` returns the
//!   protected value itself, and `update` stores its block's result without moving it and returns
//!   it.
//! - A `lock` block may not return the protected value, or an owned object inside it, nor
//!   assign one to a variable from outside the block (E0617). It may return frozen and shareable
//!   values, and fresh values (a literal, `dup`, `map`, `keys`...) whose owned contents are not
//!   taken from the protected value.
//! - `Atomic.new` needs a frozen or shareable value (E0618): `get` hands out the stored object.
//!
//! The `lock` rule is an alias analysis over types: a result that is computed from the block's
//! parameter can share an owned object with the protected value only if some owned type occurs
//! in both. Values typed `Dyn` are not tracked.

use super::*;
use crate::ast::{Block, Expr, ExprKind};

/// Methods of the built-in collections that return a new collection (never the receiver or one
/// of its elements); its items may still be the receiver's.
const FRESH_METHODS: &[&str] = &[
    "dup",
    "map",
    "select",
    "reject",
    "filter_map",
    "flat_map",
    "flatten",
    "sorted",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_by_keys",
    "sort_by_value",
    "sort_with",
    "reverse",
    "rotate",
    "to_list",
    "to_set",
    "keys",
    "values",
    "entries",
    "take",
    "take_last",
    "drop",
    "zip",
    "uniq",
    "tally",
    "group_by",
    "merge",
    "invert",
    "union",
    "intersection",
    "difference",
    "with_index",
    "slice",
    "chunk",
];

/// The owned types an object of some type can hold (itself included), or "any" when a part of
/// the type (a type parameter, a function, an interface) may hold any owned object.
#[derive(Default)]
struct OwnedParts {
    tys: Vec<TyId>,
    any: bool,
}

impl OwnedParts {
    fn overlaps(&self, other: &OwnedParts) -> bool {
        if (self.any && (other.any || !other.tys.is_empty())) || (other.any && !self.tys.is_empty()) {
            return true;
        }
        self.tys.iter().any(|t| other.tys.contains(t))
    }
}

impl Checker {
    /// Called after a member call `recv.name(args) { block }` was checked, with its type.
    pub fn check_protected_escape(
        &mut self,
        recv: &Expr,
        name: &ast::Ident,
        args: &[ast::Arg],
        block: Option<&Expr>,
        ret: TyId,
    ) {
        match name.name.as_str() {
            "get" | "update" | "lock" => {}
            "new" => {
                // `Atomic.new(v)`: the receiver is the class, so look at the result.
                let rt = self.types.resolve(ret);
                if let Ty::Class(builtin::ATOMIC, targs) = self.types.get(rt).clone() {
                    if let Some(&t) = targs.first() {
                        self.check_atomic_value(t, recv.span.to(name.span));
                    }
                }
                return;
            }
            _ => return,
        }
        let rt = self.types.resolve(self.ty_of(recv.id));
        let Ty::Class(builtin::MUTEX, targs) = self.types.get(rt).clone() else { return };
        let Some(&t) = targs.first() else { return };
        match name.name.as_str() {
            "get" | "update" => {
                if self.cap_of(t) == caps::Cap::Owned && !self.is_untracked(t) {
                    let shown = self.show(t);
                    let (msg, note) = if name.name == "get" {
                        (
                            format!("`Mutex.get` would return the protected `{shown}` itself, outside the lock"),
                            "a `Mutex` protects an owned value only inside `lock`: other threads could change \
                             it while this one uses it, and updates would be lost",
                        )
                    } else {
                        (
                            format!("`Mutex.update` on an owned `{shown}`: its result would escape the lock"),
                            "`update` stores the block's result without moving it, and returns it: both stay \
                             reachable outside the lock",
                        )
                    };
                    let d = Diag::error("E0616", msg, name.span).note(note).note(
                        "work on the value inside `m.lock { |v| ... }`, returning only what you need from \
                         the block (a count, a frozen value, a fresh copy), and replace it with `m.set(move v)`",
                    );
                    self.emit(d);
                }
            }
            _ => {
                let closure = block.or_else(|| args.first().map(|a| &a.value));
                self.check_lock_block(t, closure, ret, name.span);
            }
        }
    }

    /// `Dyn` values are checked at run time, not here.
    fn is_untracked(&mut self, t: TyId) -> bool {
        let t = self.types.resolve(t);
        matches!(self.types.get(t), Ty::Dyn | Ty::Error)
    }

    fn check_atomic_value(&mut self, t: TyId, span: Span) {
        if self.cap_of(t) != caps::Cap::Owned || self.is_untracked(t) {
            return;
        }
        let shown = self.show(t);
        let d = Diag::error("E0618", format!("`Atomic` of an owned `{shown}`"), span)
            .note(
                "an `Atomic` hands its value to every thread that calls `get`: changes to an owned value \
                 through it race, and updates are lost",
            )
            .note("keep a mutable value in a `Mutex` and change it inside `lock`, or freeze it first");
        self.emit(d);
    }

    /// The `lock` rule: the block passed to `lock` on a `Mutex[T]`, returning `R`.
    fn check_lock_block(&mut self, t: TyId, closure: Option<&Expr>, ret: TyId, span: Span) {
        let protected = self.owned_parts(t);
        if protected.tys.is_empty() && !protected.any {
            return;
        }
        let Some(c) = closure else { return };
        let ExprKind::Closure { body, .. } = &c.kind else {
            if self.cap_of(ret) != caps::Cap::Owned || self.is_untracked(ret) {
                return;
            }
            // A function value: its result is not visible here, so it may be the value itself.
            let rp = self.owned_parts(ret);
            if rp.overlaps(&protected) {
                let shown = self.show(ret);
                let d = Diag::error(
                    "E0617",
                    format!("this `lock` returns a `{shown}`, which may be the protected value or part of it"),
                    span,
                )
                .note("pass a block, so the checker can see that its result is not taken from the protected value");
                self.emit(d);
            }
            return;
        };
        let Some(info) = self.closures.get(&c.id).cloned() else { return };
        // Locals holding values computed from the protected one.
        let mut derived: FxHashSet<LocalId> = info.params.iter().copied().collect();
        let mut assigns = Vec::new();
        let mut returns = Vec::new();
        collect_escapes(body, &mut assigns, &mut returns);
        loop {
            let before = derived.len();
            for &(assign, value) in &assigns {
                if let Some(AssignRes::Local(l)) = self.assigns.get(&assign.id).cloned() {
                    if !derived.contains(&l) && self.mentions(value, &derived) {
                        derived.insert(l);
                    }
                }
            }
            if derived.len() == before {
                break;
            }
        }
        // What leaves the block: its value, `return`s, and assignments to outer variables.
        let mut escapes: Vec<(&Expr, &'static str)> = Vec::new();
        if let Some(last) = body.stmts.last() {
            tails(last, &mut escapes);
        }
        for r in returns {
            escapes.push((r, "returns"));
        }
        for &(assign, value) in &assigns {
            let outside = match self.assigns.get(&assign.id) {
                Some(AssignRes::Local(l)) => info.captures.contains(l),
                Some(AssignRes::Global(_)) => true,
                _ => false,
            };
            if outside {
                escapes.push((value, "stores"));
            }
        }
        for (x, how) in escapes {
            if matches!(x.kind, ExprKind::Return(_) | ExprKind::Break(_) | ExprKind::Continue) {
                continue;
            }
            if !self.may_share(x, &derived, &protected, 0) {
                continue;
            }
            let xt = self.ty_of(x.id);
            let shown = self.show(xt);
            let (verb, place) =
                if how == "returns" { ("returns", "") } else { ("stores", " in a variable from outside the block") };
            let msg =
                format!("this `lock` block {verb} a `{shown}` that may be the protected value or part of it{place}");
            let d = Diag::error("E0617", msg, x.span)
                .note(
                    "a `Mutex` protects an owned value only inside `lock`: used outside, other threads could \
                     change it at the same time, and updates would be lost",
                )
                .note(
                    "return only what you need: a frozen value (a count, a string), or a fresh copy whose \
                     contents are frozen (`v.dup` of a list of numbers or strings, `v.keys`, `v.map { ... }`)",
                );
            self.emit(d);
        }
    }

    /// Whether the value of `x` may be, or hold, an owned object of the protected value, given
    /// the locals `derived` that hold values computed from it.
    fn may_share(&mut self, x: &Expr, derived: &FxHashSet<LocalId>, protected: &OwnedParts, depth: u32) -> bool {
        if !self.mentions(x, derived) {
            return false;
        }
        let xt = self.ty_of(x.id);
        if self.cap_of(xt) != caps::Cap::Owned || self.is_untracked(xt) {
            return false;
        }
        if depth < 8 {
            match &x.kind {
                // A new collection holds exactly its items.
                ExprKind::List(items) | ExprKind::Tuple(items) => {
                    return items.iter().any(|i| self.may_share(i, derived, protected, depth + 1));
                }
                ExprKind::Dict(entries) => {
                    return entries.iter().any(|(k, v)| {
                        self.may_share(k, derived, protected, depth + 1)
                            || self.may_share(v, derived, protected, depth + 1)
                    });
                }
                // `map` and `filter_map` make a new list of their block's results.
                ExprKind::Member { recv, name, block: Some(b), .. }
                    if matches!(name.name.as_str(), "map" | "filter_map") && self.is_builtin_coll(recv) =>
                {
                    if let (ExprKind::Closure { body, .. }, Some(info)) = (&b.kind, self.closures.get(&b.id).cloned()) {
                        let mut inner = derived.clone();
                        inner.extend(info.params.iter().copied());
                        let mut ts = Vec::new();
                        if let Some(last) = body.stmts.last() {
                            tails(last, &mut ts);
                        }
                        return ts.into_iter().any(|(t, _)| self.may_share(t, &inner, protected, depth + 1));
                    }
                }
                _ => {}
            }
        }
        let parts = if self.is_fresh(x) { self.owned_parts_inside(xt) } else { self.owned_parts(xt) };
        parts.overlaps(protected)
    }

    fn is_builtin_coll(&mut self, e: &Expr) -> bool {
        let t = self.types.resolve(self.ty_of(e.id));
        let t = match self.types.get(t).clone() {
            Ty::Frozen(x) => self.types.resolve(x),
            _ => t,
        };
        matches!(self.types.get(t), Ty::Class(builtin::LIST | builtin::DICT | builtin::SET, _))
    }

    /// Whether `e` reads one of `locals`.
    fn mentions(&self, e: &Expr, locals: &FxHashSet<LocalId>) -> bool {
        let mut stack = vec![e];
        while let Some(x) = stack.pop() {
            if let ExprKind::Ident(_) = &x.kind {
                if let Some(NameRes::Local(l)) = self.names.get(&x.id) {
                    if locals.contains(l) {
                        return true;
                    }
                }
            }
            stack.extend(crate::query::children(x));
        }
        false
    }

    /// Whether `e` makes a new object (its own owned contents may still come from elsewhere): a
    /// collection literal, a constructor call, or a collection method whose result is a new
    /// collection (not an element).
    fn is_fresh(&mut self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::List(_) | ExprKind::Dict(_) | ExprKind::Tuple(_) | ExprKind::Str(_) | ExprKind::Binary(..) => {
                true
            }
            ExprKind::Member { recv, name, .. } => {
                if name.name == "new" {
                    return true;
                }
                let rt = self.types.resolve(self.ty_of(recv.id));
                let rt = match self.types.get(rt).clone() {
                    Ty::Frozen(x) => self.types.resolve(x),
                    _ => rt,
                };
                let Ty::Class(c, elems) = self.types.get(rt).clone() else { return false };
                if !matches!(c, builtin::LIST | builtin::DICT | builtin::SET | builtin::STRING_BUILDER) {
                    return false;
                }
                if FRESH_METHODS.contains(&name.name.as_str()) {
                    return true;
                }
                let et = self.types.resolve(self.ty_of(e.id));
                let is_coll = matches!(
                    self.types.get(et),
                    Ty::Class(builtin::LIST | builtin::DICT | builtin::SET | builtin::STRING_BUILDER, _)
                );
                // Otherwise a result of an element's type may be an element.
                is_coll && !elems.iter().any(|&x| self.types.resolve(x) == et)
            }
            _ => false,
        }
    }

    fn owned_parts(&mut self, t: TyId) -> OwnedParts {
        let mut p = OwnedParts::default();
        let mut seen = FxHashSet::default();
        self.collect_owned(t, &mut p, &mut seen, 0);
        p
    }

    /// The owned parts of a fresh object of type `t`, without the object itself.
    fn owned_parts_inside(&mut self, t: TyId) -> OwnedParts {
        let t = self.types.resolve(t);
        let mut p = OwnedParts::default();
        let mut seen = FxHashSet::default();
        seen.insert(t);
        self.collect_children(t, &mut p, &mut seen, 0);
        p
    }

    fn collect_owned(&mut self, t: TyId, p: &mut OwnedParts, seen: &mut FxHashSet<TyId>, depth: u32) {
        let t = self.types.resolve(t);
        if depth > 12 {
            p.any = true;
            return;
        }
        if !seen.insert(t) {
            return;
        }
        match self.types.get(t).clone() {
            Ty::Dyn | Ty::Error | Ty::Never | Ty::Frozen(_) => {}
            Ty::Class(..) => {
                if self.cap_of(t) != caps::Cap::Owned {
                    return;
                }
                p.tys.push(t);
                self.collect_children(t, p, seen, depth);
            }
            Ty::Tuple(_) | Ty::Union(_) | Ty::Enum(..) => self.collect_children(t, p, seen, depth),
            _ => {
                if self.cap_of(t) == caps::Cap::Owned {
                    p.any = true;
                }
            }
        }
    }

    fn collect_children(&mut self, t: TyId, p: &mut OwnedParts, seen: &mut FxHashSet<TyId>, depth: u32) {
        match self.types.get(t).clone() {
            Ty::Class(c, args) => {
                let builtin_coll = matches!(c, builtin::LIST | builtin::DICT | builtin::SET | builtin::STRING_BUILDER);
                if builtin_coll || !self.classes.contains_key(&c) {
                    for a in args {
                        self.collect_owned(a, p, seen, depth + 1);
                    }
                    return;
                }
                let info = &self.classes[&c];
                let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args).collect();
                let fields: Vec<TyId> = info.fields.iter().map(|f| f.ty).collect();
                for ft in fields {
                    let ft = self.types.subst(ft, &subst, None);
                    self.collect_owned(ft, p, seen, depth + 1);
                }
            }
            Ty::Tuple(ts) | Ty::Union(ts) => {
                for x in ts {
                    self.collect_owned(x, p, seen, depth + 1);
                }
            }
            Ty::Enum(e, args) => {
                let info = self.enums[e as usize].clone();
                let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args).collect();
                for v in &info.variants {
                    for &(_, ft) in &v.fields {
                        let ft = self.types.subst(ft, &subst, None);
                        self.collect_owned(ft, p, seen, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The expressions a block's value can come from.
fn tails<'a>(e: &'a Expr, out: &mut Vec<(&'a Expr, &'static str)>) {
    let block_tail = |b: &'a Block, out: &mut Vec<(&'a Expr, &'static str)>| {
        if let Some(last) = b.stmts.last() {
            tails(last, out);
        }
    };
    match &e.kind {
        ExprKind::If { then, else_, .. } | ExprKind::IfLet { then, else_, .. } => {
            block_tail(then, out);
            if let Some(x) = else_ {
                tails(x, out);
            }
        }
        ExprKind::Match { arms, .. } => {
            for a in arms {
                tails(&a.body, out);
            }
        }
        ExprKind::Block(b) => block_tail(b, out),
        _ => out.push((e, "returns")),
    }
}

/// Assignments `(assignment, value)` to plain variables, and `return` values, in a block's body (not
/// in nested closures, whose `return`s are their own).
fn collect_escapes<'a>(b: &'a Block, assigns: &mut Vec<(&'a Expr, &'a Expr)>, returns: &mut Vec<&'a Expr>) {
    let mut stack: Vec<&'a Expr> = b.stmts.iter().collect();
    while let Some(x) = stack.pop() {
        match &x.kind {
            ExprKind::Assign { target, value, .. } if matches!(target.kind, ExprKind::Ident(_)) => {
                assigns.push((x, value));
            }
            ExprKind::Return(Some(v)) => returns.push(v),
            ExprKind::Closure { .. } => continue,
            _ => {}
        }
        stack.extend(crate::query::children(x));
    }
}
