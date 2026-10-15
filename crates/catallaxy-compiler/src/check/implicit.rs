//! Methods the language calls implicitly: `[]` and `[]=` for indexing, `hash` and `==` for dict
//! keys, `==` and `<=>` for comparisons, operators through `Dyn` or generics, `to_s` and `inspect`
//! for interpolation, and `each` for `for` loops.
//!
//! A user-defined one may run any code, including a closure that assigns a local or a field, so
//! narrowings end after such an expression just as after an explicit call (see
//! [`Checker::clear_unstable_paths`]).

use rustc_hash::FxHashSet;

use super::*;
use crate::ast::{BinOp, Block, Expr, ExprKind, StrPart, UnOp};

/// The methods string interpolation may call on a value (`inspect` for nested values).
const TO_S: &[&str] = &["to_s", "inspect"];
/// The methods hashing a dict or set key may call.
const KEY: &[&str] = &["hash", "=="];

impl Checker {
    /// Whether evaluating `e` (just checked) may run user code: a function, method or closure
    /// call, a constructor, or an operator, index, comparison, interpolation or loop implemented
    /// by a user-defined method.
    pub fn may_mutate(&mut self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Call { .. } | ExprKind::Member { .. } => self.is_call_res(e),
            ExprKind::Index { recv, args } => {
                if self.is_call_res(e) {
                    return true;
                }
                match self.calls.get(&e.id) {
                    Some(CallRes::ListGet) => false,
                    Some(CallRes::IndexGet) => {
                        let it = args.first().map(|a| self.ty_of(a.id));
                        let rt = self.ty_of(recv.id);
                        self.index_runs_user(rt, it)
                    }
                    _ => false,
                }
            }
            ExprKind::Binary(op, l, r) => {
                let names: &[&str] = match self.bins.get(&e.id) {
                    Some(BinRes::Method(..)) => return true,
                    Some(BinRes::Eq) => &["=="],
                    Some(BinRes::Compare(ArithKind::Generic)) => &["<=>", "=="],
                    Some(BinRes::Arith(ArithKind::Generic) | BinRes::Bitwise) => &[op.symbol()],
                    _ => return false,
                };
                // Comparing with `nil` never calls a method.
                if matches!(l.kind, ExprKind::Nil) || matches!(r.kind, ExprKind::Nil) {
                    return false;
                }
                let (lt, rt) = (self.ty_of(l.id), self.ty_of(r.id));
                self.ty_runs_user(lt, names) || self.ty_runs_user(rt, names)
            }
            ExprKind::Unary(UnOp::Neg | UnOp::BitNot, x) => match self.bins.get(&e.id) {
                Some(BinRes::Arith(ArithKind::Generic) | BinRes::Bitwise) => {
                    let t = self.ty_of(x.id);
                    self.ty_runs_user(t, &["-", "~"])
                }
                _ => false,
            },
            ExprKind::Assign { target, op, .. } => {
                if matches!(self.bins.get(&e.id), Some(BinRes::Method(..))) {
                    return true;
                }
                if let (Some(op), Some(BinRes::Arith(ArithKind::Generic) | BinRes::Bitwise)) =
                    (op, self.bins.get(&e.id))
                {
                    let t = self.ty_of(target.id);
                    if self.ty_runs_user(t, &[op.symbol()]) {
                        return true;
                    }
                }
                match (self.assigns.get(&e.id), &target.kind) {
                    (Some(AssignRes::Index(_)), _) => true,
                    (Some(AssignRes::IndexGeneric), ExprKind::Index { recv, args }) => {
                        let it = args.first().map(|a| self.ty_of(a.id));
                        let rt = self.ty_of(recv.id);
                        self.index_runs_user(rt, it)
                    }
                    // A field assigned through `Dyn`.
                    (Some(AssignRes::IndexGeneric), _) => true,
                    _ => false,
                }
            }
            ExprKind::Str(parts) => parts.iter().any(|p| match p {
                StrPart::Expr(x) | StrPart::Fmt(x, ..) => {
                    let t = self.ty_of(x.id);
                    self.ty_runs_user(t, TO_S)
                }
                StrPart::Lit(_) => false,
            }),
            ExprKind::Dict(entries) => entries.iter().any(|(k, _)| {
                let t = self.ty_of(k.id);
                self.ty_runs_user(t, KEY)
            }),
            // `each` may be user code; the methods that list a builtin collection are not.
            ExprKind::For { .. } => matches!(self.fors.get(&e.id), Some(ForKind::Each(_))),
            _ => false,
        }
    }

    fn is_call_res(&self, e: &Expr) -> bool {
        matches!(
            self.calls.get(&e.id),
            Some(CallRes::Fn { .. } | CallRes::Closure { .. } | CallRes::Construct { .. } | CallRes::DynMember(_))
        )
    }

    /// Whether `recv[index]` (or `recv[index] = v`) on a value of type `rt`, with an index of
    /// type `it`, may run user code: a user-defined `[]`/`[]=`, or a user `hash`/`==` of a key.
    fn index_runs_user(&mut self, rt: TyId, it: Option<TyId>) -> bool {
        let r = self.types.shallow_resolve(rt);
        let base = match self.types.get(r) {
            Ty::Frozen(i) => self.types.shallow_resolve(*i),
            _ => r,
        };
        match self.types.get(base).clone() {
            Ty::Class(builtin::LIST | builtin::STRING, _) => false,
            Ty::Class(builtin::DICT | builtin::SET, a) => {
                let key = a[0];
                self.ty_runs_user(key, KEY) || it.is_some_and(|t| self.ty_runs_user(t, KEY))
            }
            Ty::Error | Ty::Never => false,
            _ => true,
        }
    }

    /// Whether calling one of `names` implicitly on a value of type `t` (as interpolation,
    /// equality, hashing or an operator through `Dyn` does) may run user code. Builtin
    /// implementations only call the same methods on the elements, and user classes without such
    /// a method are formatted, compared and hashed through their fields. Unknown types (`Dyn`,
    /// interfaces, type parameters) may.
    pub fn ty_runs_user(&mut self, t: TyId, names: &[&str]) -> bool {
        let mut seen = FxHashSet::default();
        self.ty_runs_user_in(t, names, &mut seen, true)
    }

    /// `params` says whether a type parameter is unknown (true) or stands for type arguments that
    /// are checked separately (false, inside a class's field types).
    fn ty_runs_user_in(&mut self, t: TyId, names: &[&str], seen: &mut FxHashSet<TyId>, params: bool) -> bool {
        let t = self.types.shallow_resolve(t);
        if !seen.insert(t) {
            return false;
        }
        match self.types.get(t).clone() {
            Ty::Never | Ty::Error | Ty::Fn(..) => false,
            Ty::Param(_) | Ty::SelfTy => params,
            Ty::Var(_) | Ty::Open(_) | Ty::Dyn | Ty::Iface(..) | Ty::Inter(_) => true,
            Ty::Tuple(ts) | Ty::Union(ts) => ts.into_iter().any(|x| self.ty_runs_user_in(x, names, seen, params)),
            Ty::Frozen(i) => self.ty_runs_user_in(i, names, seen, params),
            Ty::Class(c, args) => {
                if args.iter().any(|&a| self.ty_runs_user_in(a, names, seen, params)) {
                    return true;
                }
                if c < builtin::FIRST_USER {
                    return false;
                }
                if self.class_has_user_method(c, names) {
                    return true;
                }
                let fields: Vec<TyId> =
                    self.classes.get(&c).map(|ci| ci.fields.iter().map(|f| f.ty).collect()).unwrap_or_default();
                fields.into_iter().any(|f| self.ty_runs_user_in(f, names, seen, false))
            }
            Ty::Enum(en, args) => {
                if args.iter().any(|&a| self.ty_runs_user_in(a, names, seen, params)) {
                    return true;
                }
                let info = &self.enums[en as usize];
                if !self.mods[info.module as usize].is_prelude && names.iter().any(|n| info.methods.contains_key(*n)) {
                    return true;
                }
                if self.ext_defines(names) {
                    return true;
                }
                let fields: Vec<TyId> =
                    info.variants.iter().flat_map(|v| v.fields.iter().map(|f| f.1)).collect::<Vec<_>>();
                fields.into_iter().any(|f| self.ty_runs_user_in(f, names, seen, false))
            }
        }
    }

    /// Whether class `c`, a superclass, a subclass (which a value of type `c` may be) or an
    /// extension defines one of `names` in user code.
    fn class_has_user_method(&self, c: ClassId, names: &[&str]) -> bool {
        let defines =
            |ci: &ClassInfo| names.iter().any(|n| ci.methods.get(*n).is_some_and(|&f| !self.is_prelude_fn(f)));
        // Up the superclass chain.
        let mut cur = Some(c);
        for _ in 0..64 {
            let Some(ci) = cur.and_then(|c| self.classes.get(&c)) else { break };
            if defines(ci) {
                return true;
            }
            cur = ci.superclass.and_then(|s| match self.types.get(self.types.shallow_resolve(s)) {
                Ty::Class(p, _) => Some(*p),
                _ => None,
            });
        }
        // Down to every subclass.
        let mut stack = vec![c];
        let mut seen = FxHashSet::default();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            let Some(ci) = self.classes.get(&x) else { continue };
            if defines(ci) {
                return true;
            }
            stack.extend(ci.subclasses.iter().copied());
        }
        self.ext_defines(names)
    }

    /// Whether a user extension or interface (whose default methods classes inherit) defines one
    /// of `names`: then any user type may have it.
    fn ext_defines(&self, names: &[&str]) -> bool {
        self.exts
            .iter()
            .any(|x| !self.mods[x.module as usize].is_prelude && names.iter().any(|n| x.methods.contains_key(*n)))
            || self.ifaces.iter().any(|i| {
                !self.mods[i.module as usize].is_prelude
                    && i.methods.iter().any(|&f| names.contains(&self.fns[f as usize].name.as_str()))
            })
    }

    fn is_prelude_fn(&self, f: FnId) -> bool {
        self.mods[self.fns[f as usize].module as usize].is_prelude
    }

    /// Before a loop, judged from the body's syntax (it is not checked yet): whether it may run
    /// user code, through a call or any of the implicit calls of [`Self::may_mutate`]. Types
    /// are known only for locals and fields read through them; anything else counts as a call.
    pub fn block_may_call(&mut self, body: &Block, cond: Option<&Expr>) -> bool {
        self.ensure_method_names();
        let mut stack: Vec<&Expr> = body.stmts.iter().chain(cond).collect();
        while let Some(x) = stack.pop() {
            if self.expr_may_call_shallow(x) {
                return true;
            }
            stack.extend(crate::query::children(x));
        }
        false
    }

    /// Whether `x` itself (not its children) may run user code, judged before it is checked.
    fn expr_may_call_shallow(&mut self, x: &Expr) -> bool {
        match &x.kind {
            ExprKind::Call { .. } => true,
            // Without parentheses, a member is a field read unless some type has a method of
            // that name (zero-argument methods share the namespace).
            ExprKind::Member { name, args, block, parens, .. } => {
                if *parens || !args.is_empty() || block.is_some() || self.method_names.contains(&name.name) {
                    true
                } else {
                    if self.record_method_name_misses {
                        self.method_name_misses.insert(name.name.clone());
                    }
                    false
                }
            }
            ExprKind::Index { recv, args } => self.pre_index_may_call(recv, args.first()),
            ExprKind::Assign { target, op, .. } => {
                let index = match &target.kind {
                    ExprKind::Index { recv, args } => self.pre_index_may_call(recv, args.first()),
                    _ => false,
                };
                index
                    || op.is_some_and(|op| {
                        !matches!(op, BinOp::And | BinOp::Or | BinOp::Coalesce)
                            && self.pre_runs_user(target, op.symbol())
                    })
            }
            ExprKind::Binary(op, l, r) => {
                if matches!(op, BinOp::And | BinOp::Or | BinOp::Coalesce) {
                    return false;
                }
                if matches!(op, BinOp::Eq | BinOp::Ne)
                    && (matches!(l.kind, ExprKind::Nil) || matches!(r.kind, ExprKind::Nil))
                {
                    return false;
                }
                // `in` is a call to `contains`.
                *op == BinOp::In || self.pre_runs_user(l, op.symbol()) || self.pre_runs_user(r, op.symbol())
            }
            ExprKind::Unary(UnOp::Neg | UnOp::BitNot, v) => self.pre_runs_user(v, "-"),
            ExprKind::Str(parts) => parts.iter().any(|p| match p {
                StrPart::Expr(v) | StrPart::Fmt(v, ..) => match self.pre_ty(v) {
                    Some(t) => self.ty_runs_user(t, TO_S),
                    None => true,
                },
                StrPart::Lit(_) => false,
            }),
            ExprKind::Dict(entries) => entries.iter().any(|(k, _)| match self.pre_ty(k) {
                Some(t) => self.ty_runs_user(t, KEY),
                None => !matches!(k.kind, ExprKind::Str(_) | ExprKind::Int(_)),
            }),
            ExprKind::For { iter, .. } => match self.pre_ty(iter).map(|t| self.types.shallow_resolve(t)) {
                Some(t) => !matches!(
                    self.types.get(t),
                    Ty::Class(builtin::LIST | builtin::RANGE | builtin::DICT | builtin::SET | builtin::STRING, _)
                ),
                None => !matches!(iter.kind, ExprKind::Range { .. } | ExprKind::List(_)),
            },
            _ => false,
        }
    }

    /// Every method name of any type, for telling a field read from a zero-argument call.
    pub fn ensure_method_names(&mut self) {
        if !self.method_names.is_empty() {
            return;
        }
        let mut names: FxHashSet<String> = FxHashSet::default();
        for ci in self.classes.values() {
            names.extend(ci.methods.keys().cloned());
        }
        for x in &self.exts {
            names.extend(x.methods.keys().cloned());
        }
        for i in &self.ifaces {
            names.extend(i.by_name.keys().cloned());
        }
        self.method_names = names;
    }

    fn pre_index_may_call(&mut self, recv: &Expr, index: Option<&Expr>) -> bool {
        match self.pre_ty(recv) {
            Some(rt) => {
                let it = index.and_then(|i| self.pre_ty(i));
                let unknown_key = index.is_some() && it.is_none();
                let r = self.types.shallow_resolve(rt);
                let is_dict = matches!(self.types.get(r), Ty::Class(builtin::DICT | builtin::SET, _));
                self.index_runs_user(rt, it) || (is_dict && unknown_key)
            }
            None => true,
        }
    }

    /// Whether operator `op` on `e` (with whatever is on the other side) may run user code,
    /// judged before `e` is checked: unknown types may.
    fn pre_runs_user(&mut self, e: &Expr, op: &str) -> bool {
        match self.pre_ty(e) {
            Some(t) => self.ty_runs_user(t, &[op, "==", "<=>"]),
            None => true,
        }
    }

    /// The type of `e` judged before it is checked: literals, locals (their declared type) and
    /// fields read from them.
    fn pre_ty(&mut self, e: &Expr) -> Option<TyId> {
        match &e.kind {
            ExprKind::Int(_) => Some(self.types.int),
            ExprKind::Float(_) => Some(self.types.float),
            ExprKind::Str(_) => Some(self.types.string),
            ExprKind::Bool(_) => Some(self.types.bool),
            ExprKind::Nil => Some(self.types.nil),
            ExprKind::Ident(n) => {
                let l = self.lookup_local_quiet(n)?;
                let t = self.types.resolve(self.locals[l as usize].ty);
                (!self.types.has_vars(t)).then_some(t)
            }
            ExprKind::SelfValue => {
                let l = self.ctxs.last()?.self_local?;
                Some(self.locals[l as usize].ty)
            }
            ExprKind::Member { recv, name, args, block: None, parens: false, .. } if args.is_empty() => {
                if self.method_names.contains(&name.name) {
                    return None;
                }
                let rt = self.pre_ty(recv)?;
                let rt = self.types.strip_nil(self.types.shallow_resolve(rt)).unwrap_or(rt);
                self.lookup_field(rt, &name.name).map(|f| f.1)
            }
            // Elements of builtin collections.
            ExprKind::Index { recv, args } if args.len() == 1 => {
                let rt = self.pre_ty(recv)?;
                let rt = self.types.shallow_resolve(rt);
                let (base, frozen) = match self.types.get(rt) {
                    Ty::Frozen(i) => (self.types.shallow_resolve(*i), true),
                    _ => (rt, false),
                };
                let it = self.pre_ty(&args[0]).map(|t| self.types.shallow_resolve(t));
                let t = match self.types.get(base).clone() {
                    Ty::Class(builtin::LIST, a) if it == Some(self.types.int) => a[0],
                    Ty::Class(builtin::STRING, _) => self.types.string,
                    Ty::Class(builtin::DICT, a) => self.types.optional(a[1]),
                    _ => return None,
                };
                Some(if frozen { self.freeze_ty(t) } else { t })
            }
            // Builtin arithmetic on numbers.
            ExprKind::Binary(BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod, l, r) => {
                let (lt, rt) = (self.pre_ty(l)?, self.pre_ty(r)?);
                let (lt, rt) = (self.types.shallow_resolve(lt), self.types.shallow_resolve(rt));
                let (int, float) = (self.types.int, self.types.float);
                match (lt == int || lt == float, rt == int || rt == float) {
                    (true, true) => Some(if lt == float || rt == float { float } else { int }),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}
