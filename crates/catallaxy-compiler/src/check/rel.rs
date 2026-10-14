//! Type relations: display, assignability (subtyping with inference), unification and structural
//! interface satisfaction.

use rustc_hash::FxHashMap;

use super::*;

impl Checker {
    /// Renders a type for diagnostics.
    pub fn show(&self, t: TyId) -> String {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t) {
            Ty::Class(c, args) => {
                let name = match self.classes.get(c) {
                    Some(ci) => ci.name.clone(),
                    None => format!("<class {c}>"),
                };
                if args.is_empty() {
                    name
                } else {
                    format!("{}[{}]", name, args.iter().map(|&a| self.show(a)).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::Enum(e, args) => {
                let name = self.enums[*e as usize].name.clone();
                if args.is_empty() {
                    name
                } else {
                    format!("{}[{}]", name, args.iter().map(|&a| self.show(a)).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::Iface(i, args) => {
                let name = self.ifaces[*i as usize].name.clone();
                if args.is_empty() {
                    name
                } else {
                    format!("{}[{}]", name, args.iter().map(|&a| self.show(a)).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::Tuple(ts) => {
                if ts.len() == 1 {
                    format!("({},)", self.show(ts[0]))
                } else {
                    format!("({})", ts.iter().map(|&a| self.show(a)).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::Fn(ps, r) => {
                let ps = ps.iter().map(|&a| self.show(a)).collect::<Vec<_>>().join(", ");
                if *r == self.types.nil {
                    format!("fn({ps})")
                } else {
                    format!("fn({ps}) -> {}", self.show(*r))
                }
            }
            Ty::Union(ts) => {
                let nil = self.types.nil;
                if ts.contains(&nil) {
                    let rest: Vec<String> = ts.iter().filter(|&&x| x != nil).map(|&x| self.show_atom(x)).collect();
                    if rest.len() == 1 {
                        return format!("{}?", rest[0]);
                    }
                    return format!("({})?", rest.join(" | "));
                }
                ts.iter().map(|&a| self.show_atom(a)).collect::<Vec<_>>().join(" | ")
            }
            Ty::Inter(ts) => ts.iter().map(|&a| self.show_atom(a)).collect::<Vec<_>>().join(" & "),
            Ty::Param(p) => self.params[*p as usize].name.clone(),
            Ty::Var(_) => "_".into(),
            Ty::Frozen(i) => format!("frozen {}", self.show_atom(*i)),
            Ty::Open(o) => format!("<type of `{}`>", self.opens[*o as usize].param_name),
            Ty::SelfTy => "Self".into(),
            Ty::Never => "Never".into(),
            Ty::Dyn => "Dyn".into(),
            Ty::Error => "{unknown}".into(),
        }
    }

    fn show_atom(&self, t: TyId) -> String {
        let s = self.show(t);
        match self.types.get(self.types.shallow_resolve(t)) {
            Ty::Union(_) | Ty::Fn(..) | Ty::Inter(_) => format!("({s})"),
            _ => s,
        }
    }

    fn occurs(&self, v: VarId, t: TyId) -> bool {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t) {
            Ty::Var(w) => *w == v,
            Ty::Class(_, a) | Ty::Enum(_, a) | Ty::Iface(_, a) | Ty::Tuple(a) | Ty::Union(a) | Ty::Inter(a) => {
                a.iter().any(|&x| self.occurs(v, x))
            }
            Ty::Fn(p, r) => p.iter().any(|&x| self.occurs(v, x)) || self.occurs(v, *r),
            Ty::Frozen(i) => self.occurs(v, *i),
            _ => false,
        }
    }

    /// Type equality with inference-variable binding.
    pub fn unify(&mut self, a: TyId, b: TyId) -> bool {
        let a = self.types.shallow_resolve(a);
        let b = self.types.shallow_resolve(b);
        if a == b {
            return true;
        }
        match (self.types.get(a).clone(), self.types.get(b).clone()) {
            (Ty::Error, _) | (_, Ty::Error) => true,
            (Ty::Var(v), _) => {
                if self.occurs(v, b) {
                    return false;
                }
                self.bind_var(v, b);
                true
            }
            (_, Ty::Var(v)) => {
                if self.occurs(v, a) {
                    return false;
                }
                self.bind_var(v, a);
                true
            }
            (Ty::Class(c1, a1), Ty::Class(c2, a2)) if c1 == c2 && a1.len() == a2.len() => {
                a1.iter().zip(a2.iter()).all(|(&x, &y)| self.unify(x, y))
            }
            (Ty::Enum(e1, a1), Ty::Enum(e2, a2)) if e1 == e2 && a1.len() == a2.len() => {
                a1.iter().zip(a2.iter()).all(|(&x, &y)| self.unify(x, y))
            }
            (Ty::Iface(i1, a1), Ty::Iface(i2, a2)) if i1 == i2 && a1.len() == a2.len() => {
                a1.iter().zip(a2.iter()).all(|(&x, &y)| self.unify(x, y))
            }
            (Ty::Tuple(x), Ty::Tuple(y)) if x.len() == y.len() => {
                x.iter().zip(y.iter()).all(|(&p, &q)| self.unify(p, q))
            }
            (Ty::Fn(p1, r1), Ty::Fn(p2, r2)) if p1.len() == p2.len() => {
                p1.iter().zip(p2.iter()).all(|(&p, &q)| self.unify(p, q)) && self.unify(r1, r2)
            }
            (Ty::Frozen(x), Ty::Frozen(y)) => self.unify(x, y),
            (Ty::Union(_), Ty::Union(_)) | (Ty::Inter(_), Ty::Inter(_)) => {
                // Same members modulo variables: mutual assignability.
                self.assignable(a, b) && self.assignable(b, a)
            }
            _ => false,
        }
    }

    /// Can a value of type `src` be used where `dst` is expected? Binds inference variables.
    pub fn assignable(&mut self, src: TyId, dst: TyId) -> bool {
        let s = self.types.shallow_resolve(src);
        let d = self.types.shallow_resolve(dst);
        if s == d {
            return true;
        }
        let st = self.types.get(s).clone();
        let dt = self.types.get(d).clone();
        match (&st, &dt) {
            (Ty::Error, _) | (_, Ty::Error) => return true,
            (Ty::Never, _) => return true,
            (_, Ty::Dyn) => return true,
            (Ty::Var(v), _) => {
                if self.occurs(*v, d) {
                    return false;
                }
                self.bind_var(*v, d);
                return true;
            }
            (_, Ty::Var(v)) => {
                if self.occurs(*v, s) {
                    return false;
                }
                self.bind_var(*v, s);
                return true;
            }
            (Ty::Open(o), _) => {
                let span = self.ctxs.last().map(|c| c.span).unwrap_or_default();
                self.opens[*o as usize].uppers.push((d, span));
                return true;
            }
            _ => {}
        }
        if let Ty::Union(ss) = &st {
            return ss.clone().iter().all(|&x| self.assignable(x, d));
        }
        if let Ty::Union(ds) = &dt {
            if ds.contains(&s) {
                return true;
            }
            // Prefer members without inference variables, then the rest.
            let mut order: Vec<TyId> = ds.clone();
            order.sort_by_key(|&m| self.types.has_vars(m));
            for m in order {
                if self.trial(|c| c.assignable(s, m)) {
                    return true;
                }
            }
            return false;
        }
        match (st, dt) {
            (Ty::Class(c1, a1), Ty::Class(c2, a2)) => {
                if c1 == c2 {
                    return a1.len() == a2.len() && a1.iter().zip(a2.iter()).all(|(&x, &y)| self.unify(x, y));
                }
                match self.upcast(s, c2) {
                    Some(up) => self.assignable(up, d),
                    None => false,
                }
            }
            (Ty::Enum(e1, a1), Ty::Enum(e2, a2)) => {
                e1 == e2 && a1.iter().zip(a2.iter()).all(|(&x, &y)| self.unify(x, y))
            }
            (Ty::Tuple(xs), Ty::Tuple(ys)) => {
                xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(&x, &y)| self.assignable(x, y))
            }
            (Ty::Fn(p1, r1), Ty::Fn(p2, r2)) => {
                if p1.len() != p2.len() {
                    return false;
                }
                for (&x, &y) in p1.iter().zip(p2.iter()) {
                    if !self.assignable(y, x) {
                        return false;
                    }
                }
                let nil = self.types.nil;
                self.types.shallow_resolve(r2) == nil || self.assignable(r1, r2)
            }
            (Ty::Frozen(a), Ty::Frozen(b)) => self.assignable(a, b),
            (Ty::Frozen(a), Ty::Iface(..)) => self.satisfies(s, d) || (self.is_frozen_cap(a) && self.assignable(a, d)),
            (_, Ty::Iface(..)) => self.satisfies(s, d),
            (_, Ty::Inter(parts)) => parts.iter().all(|&p| self.assignable(s, p)),
            (Ty::Param(p), _) => {
                let bounds = self.params[p as usize].bounds.clone();
                bounds.into_iter().any(|b| self.trial(|c| c.assignable(b, d)))
            }
            (Ty::Inter(parts), _) => parts.into_iter().any(|p| self.trial(|c| c.assignable(p, d))),
            (_, Ty::Frozen(b)) => self.is_frozen_cap(s) && self.assignable(s, b),
            (Ty::Frozen(a), _) => self.is_frozen_cap(a) && self.assignable(a, d),
            _ => false,
        }
    }

    /// If class type `t` is (a subclass of) class `target`, returns `t` viewed as `target[...]`.
    pub fn upcast(&mut self, t: TyId, target: ClassId) -> Option<TyId> {
        let mut cur = t;
        for _ in 0..64 {
            let (c, args) = match self.types.get(self.types.shallow_resolve(cur)).clone() {
                Ty::Class(c, a) => (c, a),
                _ => return None,
            };
            if c == target {
                return Some(cur);
            }
            let ci = self.classes.get(&c)?;
            let sup = ci.superclass?;
            let params = ci.params.clone();
            let subst: FxHashMap<ParamId, TyId> = params.into_iter().zip(args).collect();
            cur = self.types.subst(sup, &subst, None);
        }
        None
    }

    /// Structural check: does `s` have every required method of interface type `d`?
    pub fn satisfies(&mut self, s: TyId, d: TyId) -> bool {
        let s = self.types.shallow_resolve(s);
        let d = self.types.shallow_resolve(d);
        let Ty::Iface(iid, args) = self.types.get(d).clone() else { return false };
        if let Ty::Iface(i2, a2) = self.types.get(s).clone() {
            if i2 == iid {
                return args.iter().zip(a2.iter()).all(|(&x, &y)| self.unify(x, y));
            }
        }
        if let Ty::Param(p) = self.types.get(s).clone() {
            let bounds = self.params[p as usize].bounds.clone();
            if bounds.into_iter().any(|b| self.trial(|c| c.assignable(b, d))) {
                return true;
            }
        }
        if Some(iid) == self.well_known.comparable {
            if let Some(r) = self.builtin_comparable(s) {
                return r;
            }
        }
        let cacheable = !self.types.has_vars(s) && !self.types.has_vars(d) && !self.types.has_open(s);
        if cacheable {
            if let Some(&r) = self.satisfies_cache.get(&(s, d)) {
                return r;
            }
        }
        if self.satisfies_stack.contains(&(s, d)) {
            return true;
        }
        self.satisfies_stack.push((s, d));
        let r = self.missing_methods(s, d).is_empty();
        self.satisfies_stack.pop();
        if cacheable {
            self.satisfies_cache.insert((s, d), r);
        }
        r
    }

    /// `Comparable` for builtin types without a `<=>` method: tuples and lists compare
    /// lexicographically when their elements are comparable, and `Dyn` is checked at run time.
    /// `None` for every other type (they satisfy it structurally).
    pub fn builtin_comparable(&mut self, s: TyId) -> Option<bool> {
        let s = self.types.shallow_resolve(s);
        match self.types.get(s).clone() {
            Ty::Frozen(inner) => self.builtin_comparable(inner),
            Ty::Dyn => Some(true),
            Ty::Tuple(elems) => {
                let cmp = self.comparable_ty()?;
                Some(elems.into_iter().all(|e| self.assignable(e, cmp)))
            }
            Ty::Class(builtin::LIST, args) if args.len() == 1 => {
                let cmp = self.comparable_ty()?;
                Some(self.assignable(args[0], cmp))
            }
            _ => None,
        }
    }

    fn comparable_ty(&mut self) -> Option<TyId> {
        let iid = self.well_known.comparable?;
        Some(self.types.intern(Ty::Iface(iid, Vec::new())))
    }

    /// Required methods of interface type `d` that `s` lacks (or has with incompatible signatures).
    pub fn missing_methods(&mut self, s: TyId, d: TyId) -> Vec<String> {
        let Ty::Iface(iid, args) = self.types.get(self.types.shallow_resolve(d)).clone() else { return vec![] };
        let info = self.ifaces[iid as usize].clone();
        let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args.iter().copied()).collect();
        let mut missing = Vec::new();
        for &mid in &info.methods {
            let m = self.fns[mid as usize].clone();
            if m.is_default {
                continue;
            }
            let found = self.lookup_method(s, &m.name, true);
            let Some(found) = found else {
                missing.push(m.name.clone());
                continue;
            };
            // Expected signature with interface params and Self substituted.
            let mut sub = subst.clone();
            sub.insert(info.self_param, s);
            for &tp in &m.type_params {
                let v = self.types.fresh_var();
                sub.insert(tp, v);
            }
            let exp_params: Vec<TyId> = m.params.iter().map(|p| self.types.subst(p.ty, &sub, Some(s))).collect();
            let exp_ret = self.types.subst(m.ret, &sub, Some(s));
            let (got_params, got_ret) = self.instantiate_sig(&found);
            if got_params.len() < exp_params.len() {
                missing.push(m.name.clone());
                continue;
            }
            let ok = self.trial(|c| {
                for (i, &ep) in exp_params.iter().enumerate() {
                    if !c.assignable(ep, got_params[i].ty) {
                        return false;
                    }
                }
                // Extra parameters must have defaults.
                if got_params[exp_params.len()..].iter().any(|p| !p.has_default) {
                    return false;
                }
                let nil = c.types.nil;
                c.types.shallow_resolve(exp_ret) == nil || c.assignable(got_ret, exp_ret)
            });
            if !ok {
                missing.push(m.name.clone());
            }
        }
        missing
    }

    /// Infers the interface type arguments with which `s` satisfies interface `iid`.
    pub fn iface_args_for(&mut self, s: TyId, iid: IfaceId) -> Option<Vec<TyId>> {
        let n = self.ifaces[iid as usize].params.len();
        let vars: Vec<TyId> = (0..n).map(|_| self.types.fresh_var()).collect();
        let d = self.types.intern(Ty::Iface(iid, vars.clone()));
        let cp = self.checkpoint();
        self.suppress_diags += 1;
        let ok = self.satisfies(s, d);
        self.suppress_diags -= 1;
        if !ok {
            self.rollback(cp);
            return None;
        }
        Some(vars.into_iter().map(|v| self.types.resolve(v)).collect())
    }

    /// Joins branch types into one (used for `if`/`match` results without an expected type).
    pub fn join(&mut self, tys: &[TyId]) -> TyId {
        let mut acc: Vec<TyId> = Vec::new();
        for &t in tys {
            let t = self.types.shallow_resolve(t);
            if matches!(self.types.get(t), Ty::Never) {
                continue;
            }
            if acc.contains(&t) {
                continue;
            }
            // Absorb subtypes into an existing member.
            let mut absorbed = false;
            for a in acc.iter_mut() {
                if self.trial(|c| c.assignable(t, *a)) {
                    absorbed = true;
                    break;
                }
                if self.trial(|c| c.assignable(*a, t)) {
                    *a = t;
                    absorbed = true;
                    break;
                }
            }
            if !absorbed {
                acc.push(t);
            }
        }
        self.types.union(acc)
    }
}
