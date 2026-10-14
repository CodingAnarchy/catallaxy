//! Type representation. Types are interned: a [`TyId`] is an index into [`Types`], so equality is
//! an integer comparison. Inference variables are resolved through a substitution kept in
//! [`Types::vars`].

use rustc_hash::FxHashMap;

pub type TyId = u32;
pub type ClassId = u32;
pub type EnumId = u32;
pub type IfaceId = u32;
pub type ParamId = u32;
pub type VarId = u32;
pub type OpenId = u32;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ty {
    /// A nominal class type: builtin (Int, String, List...) or user-defined.
    Class(ClassId, Vec<TyId>),
    Enum(EnumId, Vec<TyId>),
    Tuple(Vec<TyId>),
    Fn(Vec<TyId>, TyId),
    /// Sorted, flattened, deduplicated, at least two members.
    Union(Vec<TyId>),
    Iface(IfaceId, Vec<TyId>),
    /// Sorted intersection of interfaces.
    Inter(Vec<TyId>),
    /// A generic type parameter (rigid inside its scope).
    Param(ParamId),
    /// An inference variable.
    Var(VarId),
    /// Deeply immutable view.
    Frozen(TyId),
    /// An implicit-generic parameter's inferred structural requirements.
    Open(OpenId),
    /// `Self` inside an interface or class declaration.
    SelfTy,
    Never,
    Dyn,
    /// Produced after an error; compatible with everything to avoid cascades.
    Error,
}

/// Builtin class ids (equal to the runtime's ids).
pub mod builtin {
    pub const INT: u32 = 0;
    pub const FLOAT: u32 = 1;
    pub const BOOL: u32 = 2;
    pub const NIL: u32 = 3;
    pub const STRING: u32 = 4;
    pub const LIST: u32 = 6;
    pub const DICT: u32 = 7;
    pub const SET: u32 = 8;
    pub const TUPLE: u32 = 9;
    pub const CLOSURE: u32 = 10;
    pub const STRING_BUILDER: u32 = 12;
    pub const MUTEX: u32 = 13;
    pub const ATOMIC: u32 = 14;
    pub const CHANNEL: u32 = 15;
    pub const THREAD: u32 = 16;
    pub const RANGE: u32 = 17;
    pub const NATIVE: u32 = 18;
    pub const FIRST_USER: u32 = 19;
}

#[derive(Clone)]
pub struct Types {
    list: Vec<Ty>,
    map: FxHashMap<Ty, TyId>,
    /// Bindings of inference variables.
    pub vars: Vec<Option<TyId>>,
    pub int: TyId,
    pub float: TyId,
    pub bool: TyId,
    pub nil: TyId,
    pub string: TyId,
    pub never: TyId,
    pub dyn_: TyId,
    pub error: TyId,
    pub range: TyId,
    pub self_ty: TyId,
}

impl Default for Types {
    fn default() -> Self {
        Self::new()
    }
}

impl Types {
    pub fn new() -> Self {
        let mut t = Types {
            list: Vec::new(),
            map: FxHashMap::default(),
            vars: Vec::new(),
            int: 0,
            float: 0,
            bool: 0,
            nil: 0,
            string: 0,
            never: 0,
            dyn_: 0,
            error: 0,
            range: 0,
            self_ty: 0,
        };
        t.int = t.intern(Ty::Class(builtin::INT, vec![]));
        t.float = t.intern(Ty::Class(builtin::FLOAT, vec![]));
        t.bool = t.intern(Ty::Class(builtin::BOOL, vec![]));
        t.nil = t.intern(Ty::Class(builtin::NIL, vec![]));
        t.string = t.intern(Ty::Class(builtin::STRING, vec![]));
        t.never = t.intern(Ty::Never);
        t.dyn_ = t.intern(Ty::Dyn);
        t.error = t.intern(Ty::Error);
        t.range = t.intern(Ty::Class(builtin::RANGE, vec![]));
        t.self_ty = t.intern(Ty::SelfTy);
        t
    }

    pub fn intern(&mut self, ty: Ty) -> TyId {
        if let Some(&id) = self.map.get(&ty) {
            return id;
        }
        let id = self.list.len() as TyId;
        self.list.push(ty.clone());
        self.map.insert(ty, id);
        id
    }

    #[inline]
    pub fn get(&self, id: TyId) -> &Ty {
        &self.list[id as usize]
    }

    pub fn class(&mut self, cid: ClassId, args: Vec<TyId>) -> TyId {
        self.intern(Ty::Class(cid, args))
    }
    pub fn list_of(&mut self, t: TyId) -> TyId {
        self.class(builtin::LIST, vec![t])
    }
    pub fn dict_of(&mut self, k: TyId, v: TyId) -> TyId {
        self.class(builtin::DICT, vec![k, v])
    }
    pub fn tuple(&mut self, ts: Vec<TyId>) -> TyId {
        self.intern(Ty::Tuple(ts))
    }
    pub fn func(&mut self, params: Vec<TyId>, ret: TyId) -> TyId {
        self.intern(Ty::Fn(params, ret))
    }
    pub fn fresh_var(&mut self) -> TyId {
        let v = self.vars.len() as VarId;
        self.vars.push(None);
        self.intern(Ty::Var(v))
    }

    pub fn optional(&mut self, t: TyId) -> TyId {
        let nil = self.nil;
        self.union(vec![t, nil])
    }

    /// Builds a normalized union (flattened, deduplicated, sorted; `Never` dropped).
    pub fn union(&mut self, parts: Vec<TyId>) -> TyId {
        let mut flat = Vec::new();
        for p in parts {
            let p = self.shallow_resolve(p);
            match self.get(p).clone() {
                Ty::Union(inner) => flat.extend(inner),
                Ty::Never => {}
                Ty::Error => return self.error,
                _ => flat.push(p),
            }
        }
        flat.sort_unstable();
        flat.dedup();
        if flat.iter().any(|&t| matches!(self.get(t), Ty::Dyn)) {
            return self.dyn_;
        }
        match flat.len() {
            0 => self.never,
            1 => flat[0],
            _ => self.intern(Ty::Union(flat)),
        }
    }

    pub fn inter(&mut self, parts: Vec<TyId>) -> TyId {
        let mut flat = Vec::new();
        for p in parts {
            match self.get(p).clone() {
                Ty::Inter(inner) => flat.extend(inner),
                _ => flat.push(p),
            }
        }
        flat.sort_unstable();
        flat.dedup();
        if flat.len() == 1 {
            return flat[0];
        }
        self.intern(Ty::Inter(flat))
    }

    pub fn frozen(&mut self, t: TyId) -> TyId {
        match self.get(t) {
            Ty::Frozen(_) | Ty::Never | Ty::Error => t,
            _ => self.intern(Ty::Frozen(t)),
        }
    }

    /// Follows variable bindings one level deep.
    pub fn shallow_resolve(&self, mut t: TyId) -> TyId {
        loop {
            match self.get(t) {
                Ty::Var(v) => match self.vars[*v as usize] {
                    Some(b) => t = b,
                    None => return t,
                },
                _ => return t,
            }
        }
    }

    /// Substitutes all bound variables, recursively.
    pub fn resolve(&mut self, t: TyId) -> TyId {
        let t = self.shallow_resolve(t);
        match self.get(t).clone() {
            Ty::Class(c, args) => {
                let args2: Vec<TyId> = args.iter().map(|&a| self.resolve(a)).collect();
                if args2 == args {
                    t
                } else {
                    self.intern(Ty::Class(c, args2))
                }
            }
            Ty::Enum(e, args) => {
                let args2: Vec<TyId> = args.iter().map(|&a| self.resolve(a)).collect();
                if args2 == args {
                    t
                } else {
                    self.intern(Ty::Enum(e, args2))
                }
            }
            Ty::Iface(i, args) => {
                let args2: Vec<TyId> = args.iter().map(|&a| self.resolve(a)).collect();
                if args2 == args {
                    t
                } else {
                    self.intern(Ty::Iface(i, args2))
                }
            }
            Ty::Tuple(ts) => {
                let ts2: Vec<TyId> = ts.iter().map(|&a| self.resolve(a)).collect();
                if ts2 == ts {
                    t
                } else {
                    self.intern(Ty::Tuple(ts2))
                }
            }
            Ty::Fn(ps, r) => {
                let ps2: Vec<TyId> = ps.iter().map(|&a| self.resolve(a)).collect();
                let r2 = self.resolve(r);
                if ps2 == ps && r2 == r {
                    t
                } else {
                    self.intern(Ty::Fn(ps2, r2))
                }
            }
            Ty::Union(ts) => {
                let ts2: Vec<TyId> = ts.iter().map(|&a| self.resolve(a)).collect();
                if ts2 == ts {
                    t
                } else {
                    self.union(ts2)
                }
            }
            Ty::Inter(ts) => {
                let ts2: Vec<TyId> = ts.iter().map(|&a| self.resolve(a)).collect();
                if ts2 == ts {
                    t
                } else {
                    self.inter(ts2)
                }
            }
            Ty::Frozen(inner) => {
                let i2 = self.resolve(inner);
                if i2 == inner {
                    t
                } else {
                    self.frozen(i2)
                }
            }
            _ => t,
        }
    }

    /// Replaces type parameters (and `Self`) using `subst`.
    pub fn subst(&mut self, t: TyId, subst: &FxHashMap<ParamId, TyId>, self_ty: Option<TyId>) -> TyId {
        if subst.is_empty() && self_ty.is_none() {
            return t;
        }
        let t = self.shallow_resolve(t);
        match self.get(t).clone() {
            Ty::Param(p) => subst.get(&p).copied().unwrap_or(t),
            Ty::SelfTy => self_ty.unwrap_or(t),
            Ty::Class(c, args) => {
                let args: Vec<TyId> = args.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                self.intern(Ty::Class(c, args))
            }
            Ty::Enum(e, args) => {
                let args: Vec<TyId> = args.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                self.intern(Ty::Enum(e, args))
            }
            Ty::Iface(i, args) => {
                let args: Vec<TyId> = args.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                self.intern(Ty::Iface(i, args))
            }
            Ty::Tuple(ts) => {
                let ts: Vec<TyId> = ts.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                self.intern(Ty::Tuple(ts))
            }
            Ty::Fn(ps, r) => {
                let ps: Vec<TyId> = ps.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                let r = self.subst(r, subst, self_ty);
                self.intern(Ty::Fn(ps, r))
            }
            Ty::Union(ts) => {
                let ts: Vec<TyId> = ts.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                self.union(ts)
            }
            Ty::Inter(ts) => {
                let ts: Vec<TyId> = ts.iter().map(|&a| self.subst(a, subst, self_ty)).collect();
                self.inter(ts)
            }
            Ty::Frozen(inner) => {
                let i = self.subst(inner, subst, self_ty);
                self.frozen(i)
            }
            _ => t,
        }
    }

    /// True if `t` (resolved) contains unbound inference variables.
    pub fn has_vars(&self, t: TyId) -> bool {
        let t = self.shallow_resolve(t);
        match self.get(t) {
            Ty::Var(_) => true,
            Ty::Class(_, a) | Ty::Enum(_, a) | Ty::Iface(_, a) | Ty::Tuple(a) | Ty::Union(a) | Ty::Inter(a) => {
                a.iter().any(|&x| self.has_vars(x))
            }
            Ty::Fn(p, r) => p.iter().any(|&x| self.has_vars(x)) || self.has_vars(*r),
            Ty::Frozen(i) => self.has_vars(*i),
            _ => false,
        }
    }

    /// True if `t` mentions an open (implicit-generic) type.
    pub fn has_open(&self, t: TyId) -> bool {
        let t = self.shallow_resolve(t);
        match self.get(t) {
            Ty::Open(_) => true,
            Ty::Class(_, a) | Ty::Enum(_, a) | Ty::Iface(_, a) | Ty::Tuple(a) | Ty::Union(a) | Ty::Inter(a) => {
                a.iter().any(|&x| self.has_open(x))
            }
            Ty::Fn(p, r) => p.iter().any(|&x| self.has_open(x)) || self.has_open(*r),
            Ty::Frozen(i) => self.has_open(*i),
            _ => false,
        }
    }

    /// Binds every unbound variable in `t` to `default`.
    pub fn default_vars(&mut self, t: TyId, default: TyId) {
        let t = self.shallow_resolve(t);
        match self.get(t).clone() {
            Ty::Var(v) => self.vars[v as usize] = Some(default),
            Ty::Class(_, a) | Ty::Enum(_, a) | Ty::Iface(_, a) | Ty::Tuple(a) | Ty::Union(a) | Ty::Inter(a) => {
                for x in a {
                    self.default_vars(x, default)
                }
            }
            Ty::Fn(p, r) => {
                for x in p {
                    self.default_vars(x, default)
                }
                self.default_vars(r, default)
            }
            Ty::Frozen(i) => self.default_vars(i, default),
            _ => {}
        }
    }

    /// The non-nil part of an optional, if `t` includes nil.
    pub fn strip_nil(&mut self, t: TyId) -> Option<TyId> {
        let t = self.shallow_resolve(t);
        match self.get(t).clone() {
            Ty::Union(parts) if parts.contains(&self.nil) => {
                let rest: Vec<TyId> = parts.into_iter().filter(|&p| p != self.nil).collect();
                Some(self.union(rest))
            }
            _ => None,
        }
    }

    pub fn is_nilable(&self, t: TyId) -> bool {
        let t = self.shallow_resolve(t);
        match self.get(t) {
            Ty::Union(parts) => parts.contains(&self.nil),
            _ => t == self.nil,
        }
    }

    pub fn is_error(&self, t: TyId) -> bool {
        matches!(self.get(self.shallow_resolve(t)), Ty::Error)
    }
}
