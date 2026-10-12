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
                builtin::INT | builtin::FLOAT | builtin::BOOL | builtin::NIL | builtin::STRING | builtin::RANGE => {
                    Cap::Frozen
                }
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

    pub fn is_frozen_cap(&mut self, t: TyId) -> bool {
        self.cap_of(t) == Cap::Frozen
    }

    pub fn is_sendable(&mut self, t: TyId) -> bool {
        self.cap_of(t) >= Cap::Shareable
    }
}
