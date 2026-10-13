//! Method lookup: class methods (with inheritance), extensions, interface defaults and dynamic
//! receivers (interfaces, type parameters, unions).

use rustc_hash::FxHashMap;

use super::*;

#[derive(Clone, Debug)]
pub struct MethodRef {
    pub fid: FnId,
    pub subst: FxHashMap<ParamId, TyId>,
    pub self_ty: TyId,
    pub dispatch: Dispatch,
    /// The receiver is frozen: mutating methods are rejected and results are frozen.
    pub frozen: bool,
}

impl Checker {
    /// Modules whose public items are visible from `m` (itself, its imports, the prelude).
    pub fn visible_modules(&self, m: ModId) -> Vec<ModId> {
        let mut out = vec![m];
        for (i, md) in self.mods.iter().enumerate() {
            if md.is_prelude && i as ModId != m {
                out.push(i as ModId);
            }
        }
        for &i in &self.mods[m as usize].imports {
            if !out.contains(&i) {
                out.push(i);
            }
        }
        out
    }

    fn visible_exts(&self) -> Vec<ExtId> {
        let mut out = Vec::new();
        for m in self.visible_modules(self.cur_module) {
            for &e in &self.mods[m as usize].extends {
                let ext = &self.exts[e as usize];
                if ext.is_class_methods {
                    continue;
                }
                if m == self.cur_module || ext.is_pub || self.mods[m as usize].is_prelude {
                    out.push(e);
                }
            }
        }
        out
    }

    fn visible_ifaces(&self) -> Vec<IfaceId> {
        let vis = self.visible_modules(self.cur_module);
        self.ifaces
            .iter()
            .filter(|i| vis.contains(&i.module) && (i.module == self.cur_module || i.is_pub))
            .map(|i| i.id)
            .collect()
    }

    pub fn ensure_signature(&mut self, fid: FnId) {
        let f = &self.fns[fid as usize];
        if f.ret_annotated || f.is_native || f.state == CheckState::Done {
            return;
        }
        if matches!(f.synth, Synth::Main(_)) {
            return;
        }
        self.check_fn_body(fid);
    }

    /// Instantiates a method's parameter and result types for one call.
    pub fn instantiate_sig(&mut self, m: &MethodRef) -> (Vec<PInfo>, TyId) {
        self.ensure_signature(m.fid);
        let f = self.fns[m.fid as usize].clone();
        let mut sub = m.subst.clone();
        self.last_inst.clear();
        for &tp in &f.type_params {
            let v = self.types.fresh_var();
            sub.insert(tp, v);
            self.last_inst.push((tp, v));
        }
        self.last_inst_subst = sub.clone();
        let params: Vec<PInfo> =
            f.params.iter().map(|p| PInfo { ty: self.types.subst(p.ty, &sub, Some(m.self_ty)), ..p.clone() }).collect();
        let mut ret = self.types.subst(f.ret, &sub, Some(m.self_ty));
        let builtin = f.is_native || self.mods[f.module as usize].is_prelude;
        if m.frozen && !(builtin && self.is_frozen_builtin_collection(m.self_ty)) {
            ret = self.freeze_ty(ret);
        }
        (params, ret)
    }

    /// `frozen List[T]`, `frozen Dict[K, V]` or `frozen Set[T]`. Their builtin methods (native or
    /// in the prelude) never return the receiver or a part of it other than elements, whose types
    /// are already frozen through the substitution, so results such as `dup()`, `map` or `take`
    /// are fresh, owned values and need not be frozen.
    fn is_frozen_builtin_collection(&self, t: TyId) -> bool {
        match self.types.get(self.types.shallow_resolve(t)) {
            Ty::Frozen(inner) => matches!(
                self.types.get(self.types.shallow_resolve(*inner)),
                Ty::Class(builtin::LIST | builtin::DICT | builtin::SET, _)
            ),
            _ => false,
        }
    }

    /// The deeply frozen view of a type.
    pub fn freeze_ty(&mut self, t: TyId) -> TyId {
        let t = self.types.shallow_resolve(t);
        if self.is_frozen_cap(t) {
            return t;
        }
        match self.types.get(t).clone() {
            Ty::Union(parts) => {
                let ps = parts.into_iter().map(|p| self.freeze_ty(p)).collect();
                self.types.union(ps)
            }
            Ty::Tuple(parts) => {
                let ps = parts.into_iter().map(|p| self.freeze_ty(p)).collect();
                self.types.tuple(ps)
            }
            Ty::Var(_) | Ty::Error | Ty::Never | Ty::Dyn => t,
            _ => self.types.frozen(t),
        }
    }

    /// Finds method `name` on values of type `recv`.
    pub fn lookup_method(&mut self, recv: TyId, name: &str, allow_getters: bool) -> Option<MethodRef> {
        let r = self.types.shallow_resolve(recv);
        match self.types.get(r).clone() {
            Ty::Class(cid, args) => self
                .lookup_in_class(r, cid, args, name, allow_getters)
                .or_else(|| self.lookup_ext(r, name, false))
                .or_else(|| self.lookup_default(r, name, false)),
            Ty::Enum(eid, args) => {
                let info = &self.enums[eid as usize];
                if let Some(&fid) = info.methods.get(name) {
                    let subst = info.params.iter().copied().zip(args).collect();
                    return Some(MethodRef { fid, subst, self_ty: r, dispatch: Dispatch::Static(fid), frozen: false });
                }
                self.lookup_ext(r, name, false).or_else(|| self.lookup_default(r, name, false))
            }
            Ty::Iface(iid, args) => {
                self.lookup_in_iface(r, iid, &args, name).or_else(|| self.lookup_ext(r, name, true))
            }
            Ty::Inter(parts) => {
                for p in parts {
                    if let Some(mut m) = self.lookup_method(p, name, allow_getters) {
                        self.rebind_self(&mut m, r);
                        return Some(m);
                    }
                }
                None
            }
            Ty::Param(p) => {
                let bounds = self.params[p as usize].bounds.clone();
                for b in bounds {
                    if let Some(mut m) = self.lookup_method(b, name, allow_getters) {
                        self.rebind_self(&mut m, r);
                        let fallback = match &m.dispatch {
                            Dispatch::Dynamic(_, f) => *f,
                            Dispatch::Static(f) if self.fns[*f as usize].is_default || self.is_ext_fn(*f) => Some(*f),
                            _ => None,
                        };
                        m.dispatch = Dispatch::Dynamic(name.to_string(), fallback);
                        return Some(m);
                    }
                }
                None
            }
            Ty::Frozen(inner) => {
                let mut m = self.lookup_method(inner, name, allow_getters)?;
                m.frozen = true;
                // Deep immutability: the receiver's type arguments are seen frozen too.
                let keys: Vec<ParamId> = m.subst.keys().copied().collect();
                for k in keys {
                    let t = m.subst[&k];
                    let ft = self.freeze_ty(t);
                    m.subst.insert(k, ft);
                }
                m.self_ty = r;
                Some(m)
            }
            Ty::Union(parts) => {
                let mut first = None;
                for p in parts {
                    let m = self.lookup_method(p, name, allow_getters)?;
                    if first.is_none() {
                        first = Some(m);
                    }
                }
                let mut m = first?;
                m.dispatch = Dispatch::Dynamic(name.to_string(), None);
                m.self_ty = r;
                Some(m)
            }
            Ty::Tuple(_) | Ty::Fn(..) => {
                self.lookup_ext(r, name, false).or_else(|| self.lookup_default(r, name, false))
            }
            _ => None,
        }
    }

    /// Re-targets a method found through a bound or an intersection member at the actual
    /// receiver `r`: `Self` in the member's signature means `r` (e.g. `T` for `T: Clone`), not the
    /// interface the method was found on.
    fn rebind_self(&self, m: &mut MethodRef, r: TyId) {
        m.self_ty = r;
        if let Owner::Iface(iid) = self.fns[m.fid as usize].owner {
            m.subst.insert(self.ifaces[iid as usize].self_param, r);
        }
    }

    pub fn is_ext_fn(&self, f: FnId) -> bool {
        matches!(self.fns[f as usize].owner, Owner::Extend(e) if !self.exts[e as usize].is_class_methods)
    }

    fn lookup_in_class(
        &mut self,
        r: TyId,
        cid: ClassId,
        args: Vec<TyId>,
        name: &str,
        allow_getters: bool,
    ) -> Option<MethodRef> {
        let static_cls = cid;
        let mut cur = cid;
        let mut cur_args = args;
        for _ in 0..64 {
            let ci = self.classes.get(&cur)?;
            let subst: FxHashMap<ParamId, TyId> = ci.params.iter().copied().zip(cur_args.iter().copied()).collect();
            if let Some(&fid) = ci.methods.get(name) {
                let sc = &self.classes[&static_cls];
                let dispatch = match sc.vslots.get(name) {
                    Some(&slot)
                        if !sc.subclasses.is_empty() && self.fns[fid as usize].self_kind != ast::SelfKind::None =>
                    {
                        Dispatch::Virtual(fid, slot)
                    }
                    _ => Dispatch::Static(fid),
                };
                return Some(MethodRef { fid, subst, self_ty: r, dispatch, frozen: false });
            }
            if allow_getters {
                if let Some(&g) = ci.getters.get(name) {
                    return Some(MethodRef { fid: g, subst, self_ty: r, dispatch: Dispatch::Static(g), frozen: false });
                }
            }
            let sup = ci.superclass?;
            let next = self.types.subst(sup, &subst, None);
            match self.types.get(next).clone() {
                Ty::Class(c, a) => {
                    cur = c;
                    cur_args = a;
                }
                _ => return None,
            }
        }
        None
    }

    fn lookup_in_iface(&mut self, r: TyId, iid: IfaceId, args: &[TyId], name: &str) -> Option<MethodRef> {
        let info = &self.ifaces[iid as usize];
        let &fid = info.by_name.get(name)?;
        let mut subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args.iter().copied()).collect();
        subst.insert(info.self_param, r);
        let fallback = if self.fns[fid as usize].is_default { Some(fid) } else { None };
        Some(MethodRef {
            fid,
            subst,
            self_ty: r,
            dispatch: Dispatch::Dynamic(name.to_string(), fallback),
            frozen: false,
        })
    }

    /// Extension methods visible in the current module whose target matches `r`.
    fn lookup_ext(&mut self, r: TyId, name: &str, dynamic: bool) -> Option<MethodRef> {
        for e in self.visible_exts() {
            let Some(&fid) = self.exts[e as usize].methods.get(name) else { continue };
            let ext = self.exts[e as usize].clone();
            let mut sub: FxHashMap<ParamId, TyId> = FxHashMap::default();
            for &p in &ext.params {
                let v = self.types.fresh_var();
                sub.insert(p, v);
            }
            let target = self.types.subst(ext.target, &sub, None);
            let cp = self.checkpoint();
            self.suppress_diags += 1;
            let ok = match self.types.get(target).clone() {
                Ty::Iface(..) => self.assignable(r, target),
                _ => self.unify_receiver(r, target),
            } && ext.params.iter().all(|p| {
                let actual = sub[p];
                let bounds = self.params[*p as usize].bounds.clone();
                bounds.into_iter().all(|b| {
                    let b = self.types.subst(b, &sub, None);
                    self.assignable(actual, b)
                })
            });
            self.suppress_diags -= 1;
            if !ok {
                self.rollback(cp);
                continue;
            }
            let subst: FxHashMap<ParamId, TyId> = sub.into_iter().map(|(p, t)| (p, self.types.resolve(t))).collect();
            let dispatch = if dynamic { Dispatch::Dynamic(name.to_string(), Some(fid)) } else { Dispatch::Static(fid) };
            return Some(MethodRef { fid, subst, self_ty: r, dispatch, frozen: false });
        }
        None
    }

    /// Matches a receiver against an extension target (subclasses match their superclass).
    fn unify_receiver(&mut self, r: TyId, target: TyId) -> bool {
        if let Ty::Class(tc, _) = self.types.get(target).clone() {
            if let Some(up) = self.upcast(r, tc) {
                return self.unify(up, target);
            }
            return false;
        }
        self.unify(r, target)
    }

    /// Interface default methods available to `r` because it satisfies the interface.
    fn lookup_default(&mut self, r: TyId, name: &str, dynamic: bool) -> Option<MethodRef> {
        for iid in self.visible_ifaces() {
            let info = &self.ifaces[iid as usize];
            let Some(&fid) = info.by_name.get(name) else { continue };
            if !self.fns[fid as usize].is_default {
                continue;
            }
            let self_param = info.self_param;
            let params = info.params.clone();
            let Some(args) = self.iface_args_for(r, iid) else { continue };
            let mut subst: FxHashMap<ParamId, TyId> = params.into_iter().zip(args).collect();
            subst.insert(self_param, r);
            let dispatch = if dynamic { Dispatch::Dynamic(name.to_string(), Some(fid)) } else { Dispatch::Static(fid) };
            return Some(MethodRef { fid, subst, self_ty: r, dispatch, frozen: false });
        }
        None
    }

    /// Field of a class type: (index, type with class params substituted).
    pub fn lookup_field(&mut self, recv: TyId, name: &str) -> Option<(u32, TyId)> {
        let r = self.types.shallow_resolve(recv);
        let (cid, args, frozen) = match self.types.get(r).clone() {
            Ty::Class(c, a) => (c, a, false),
            Ty::Frozen(inner) => match self.types.get(self.types.shallow_resolve(inner)).clone() {
                Ty::Class(c, a) => (c, a, true),
                _ => return None,
            },
            _ => return None,
        };
        let ci = self.classes.get(&cid)?;
        if ci.is_builtin {
            return None;
        }
        let idx = ci.fields.iter().position(|f| f.name == name)?;
        let field = ci.fields[idx].clone();
        // Substitute the declaring class's params as seen from this class.
        let decl_view = self.upcast(r, field.decl_class).unwrap_or(r);
        let (dc_params, dargs) = match self.types.get(decl_view).clone() {
            Ty::Class(c, a) => (self.classes[&c].params.clone(), a),
            _ => (Vec::new(), args),
        };
        let subst: FxHashMap<ParamId, TyId> = dc_params.into_iter().zip(dargs).collect();
        let mut ty = self.types.subst(field.ty, &subst, Some(r));
        if frozen {
            ty = self.freeze_ty(ty);
        }
        Some((idx as u32, ty))
    }
}
