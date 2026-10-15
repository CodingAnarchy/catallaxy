//! Concurrency capabilities: every type is frozen, shareable or owned.

use super::*;

/// Ordered from least to most permissive.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cap {
    Owned,
    Shareable,
    Frozen,
}

impl Checker {
    pub fn cap_of(&mut self, t: TyId) -> Cap {
        self.cap_of_depth(t, 0)
    }

    fn cap_of_depth(&mut self, t: TyId, depth: u32) -> Cap {
        if depth > 16 {
            return Cap::Owned;
        }
        let t = self.types.shallow_resolve(t);
        match self.types.get(t).clone() {
            Ty::Class(c, args) => match c {
                builtin::INT
                | builtin::FLOAT
                | builtin::BOOL
                | builtin::NIL
                | builtin::STRING
                | builtin::RANGE
                | builtin::NATIVE => Cap::Frozen,
                builtin::LIST | builtin::DICT | builtin::SET | builtin::STRING_BUILDER => Cap::Owned,
                builtin::MUTEX | builtin::CHANNEL | builtin::THREAD | builtin::ATOMIC => Cap::Shareable,
                _ => match self.classes.get(&c).map(|ci| ci.modifier) {
                    Some(ast::ClassModifier::Frozen) => {
                        // A frozen generic class is frozen only if its arguments are.
                        let mut cap = Cap::Frozen;
                        for a in args {
                            cap = cap.min(self.cap_of_depth(a, depth + 1));
                        }
                        cap
                    }
                    Some(ast::ClassModifier::Shared) => Cap::Shareable,
                    _ => Cap::Owned,
                },
            },
            Ty::Enum(e, args) => {
                let info = self.enums[e as usize].clone();
                let subst: rustc_hash::FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args).collect();
                let mut cap = Cap::Frozen;
                for v in &info.variants {
                    for &(_, ft) in &v.fields {
                        let ft = self.types.subst(ft, &subst, None);
                        if ft == t {
                            continue;
                        }
                        cap = cap.min(self.cap_of_depth(ft, depth + 1));
                    }
                }
                cap
            }
            Ty::Tuple(ts) | Ty::Union(ts) => {
                let mut cap = Cap::Frozen;
                for x in ts {
                    cap = cap.min(self.cap_of_depth(x, depth + 1));
                }
                cap
            }
            Ty::Frozen(_) | Ty::Never | Ty::Error => Cap::Frozen,
            Ty::Param(p) => {
                // A parameter bounded by `Frozen`-like interfaces is not tracked; be conservative.
                let _ = p;
                Cap::Owned
            }
            _ => Cap::Owned,
        }
    }

    /// The pinned type inside `t`, if any: a `Fiber`, which belongs to the thread that started
    /// it, directly or in a collection, tuple, union, enum payload or class field. A value of such
    /// a type cannot move to another thread. (`Dyn` and interface values are checked at run time.)
    pub fn pinned_in(&mut self, t: TyId) -> Option<TyId> {
        let fiber = self.well_known.fiber?;
        let mut seen = rustc_hash::FxHashSet::default();
        self.pinned_in_rec(t, fiber, &mut seen)
    }

    fn pinned_in_rec(&mut self, t: TyId, fiber: ClassId, seen: &mut rustc_hash::FxHashSet<TyId>) -> Option<TyId> {
        let t = self.types.shallow_resolve(t);
        if !seen.insert(t) {
            return None;
        }
        match self.types.get(t).clone() {
            Ty::Class(c, _) if c == fiber => Some(t),
            Ty::Class(c, args) => {
                for a in args {
                    if let Some(p) = self.pinned_in_rec(a, fiber, seen) {
                        return Some(p);
                    }
                }
                // Field types, with the class's type parameters standing for the arguments
                // (checked above).
                let fields: Vec<TyId> = match self.classes.get(&c) {
                    Some(ci) if !ci.is_builtin => ci.fields.iter().map(|f| f.ty).collect(),
                    _ => Vec::new(),
                };
                fields.into_iter().find_map(|f| self.pinned_in_rec(f, fiber, seen))
            }
            Ty::Enum(e, args) => {
                for a in args {
                    if let Some(p) = self.pinned_in_rec(a, fiber, seen) {
                        return Some(p);
                    }
                }
                let fields: Vec<TyId> =
                    self.enums[e as usize].variants.iter().flat_map(|v| v.fields.iter().map(|f| f.1)).collect();
                fields.into_iter().find_map(|f| self.pinned_in_rec(f, fiber, seen))
            }
            Ty::Tuple(ts) | Ty::Union(ts) => ts.into_iter().find_map(|x| self.pinned_in_rec(x, fiber, seen)),
            Ty::Frozen(i) => self.pinned_in_rec(i, fiber, seen),
            _ => None,
        }
    }

    /// E0619 if a value of type `t` would move to another thread but holds a pinned `Fiber`.
    pub fn check_not_pinned(&mut self, t: TyId, what: &str, span: Span) -> bool {
        let Some(p) = self.pinned_in(t) else { return true };
        let inner = if self.types.shallow_resolve(t) == p {
            String::new()
        } else {
            format!(" (it holds a `{}`)", self.show(p))
        };
        let msg = format!("{what} of type `{}` cannot move to another thread{inner}", self.show(t));
        self.emit(
            Diagnostic::error("E0619", msg, span)
                .note("a fiber runs on the thread that started it, and only that thread can join it")
                .note("join it on this thread and send its result instead"),
        );
        false
    }

    pub fn is_frozen_cap(&mut self, t: TyId) -> bool {
        self.cap_of(t) == Cap::Frozen
    }

    pub fn is_sendable(&mut self, t: TyId) -> bool {
        self.cap_of(t) >= Cap::Shareable
    }
}

impl Checker {
    /// A key type hashed by contents that can change after insertion: an owned `List`, `Dict`
    /// or `Set` (directly, in a tuple, or in a union). Frozen collections and other types
    /// (`Hashable` classes, identity-hashed instances) are fine.
    fn is_mutable_key(&self, t: TyId, depth: u32) -> bool {
        if depth > 16 {
            return false;
        }
        let t = self.types.shallow_resolve(t);
        match self.types.get(t) {
            Ty::Class(builtin::LIST | builtin::DICT | builtin::SET, _) => true,
            Ty::Tuple(ts) | Ty::Union(ts) => ts.iter().any(|&x| self.is_mutable_key(x, depth + 1)),
            _ => false,
        }
    }

    /// The first `Dict` or `Set` type inside `t` whose keys are mutable: (container, key).
    pub fn mutable_key_in(&self, t: TyId, depth: u32) -> Option<(TyId, TyId)> {
        if depth > 16 {
            return None;
        }
        let t = self.types.shallow_resolve(t);
        match self.types.get(t) {
            Ty::Class(c, args) => {
                if matches!(*c, builtin::DICT | builtin::SET) && !args.is_empty() && self.is_mutable_key(args[0], 0) {
                    return Some((t, args[0]));
                }
                args.iter().find_map(|&a| self.mutable_key_in(a, depth + 1))
            }
            Ty::Tuple(ts) | Ty::Union(ts) => ts.iter().find_map(|&a| self.mutable_key_in(a, depth + 1)),
            Ty::Frozen(_) => None,
            _ => None,
        }
    }

    /// E0373 for a variable or field `name` of type `t` holding a `Dict`/`Set` with mutable keys.
    pub fn check_mutable_key(&mut self, what: &str, t: TyId, span: Span) {
        let Some((container, key)) = self.mutable_key_in(t, 0) else { return };
        let msg = format!("{what} has type `{}`, whose keys are mutable `{}` values", self.show(t), self.show(key));
        let note = format!(
            "changing a key after it is inserted makes its entry unreachable; use frozen keys (`{}` with \
             `frozen {}` keys, built with `freeze(k)`) or tuples",
            self.show(container),
            self.show(key)
        );
        self.emit(Diag::error("E0373", msg, span).note(note));
    }
}
