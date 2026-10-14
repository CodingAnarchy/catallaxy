//! Function bodies: contexts, scopes, locals, narrowing and statements.

use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};

use super::*;
use crate::ast::{Block, Expr, ExprKind, FnDecl, Item, SelfKind};

impl Checker {
    /// Finds the declaration of a function with a body.
    pub fn fn_decl(&self, fid: FnId) -> Option<FnDecl> {
        match self.fns[fid as usize].decl {
            DeclRef::Item(m, i) => match &self.mods[m as usize].ast.items[i] {
                Item::Fn(f) => Some(f.clone()),
                _ => None,
            },
            DeclRef::Method(m, i, j) => match &self.mods[m as usize].ast.items[i] {
                Item::Class(c) => c.methods.get(j).cloned(),
                Item::Enum(e) => e.methods.get(j).cloned(),
                Item::Interface(f) => f.methods.get(j).cloned(),
                Item::Extend(x) => x.methods.get(j).cloned(),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn push_ctx(&mut self, fn_id: FnId, kind: CtxKind, ret: TyId, span: Span) {
        self.ctxs.push(FnCtx {
            fn_id,
            kind,
            ret,
            scopes: vec![FxHashMap::default()],
            narrow: vec![FxHashMap::default()],
            path_narrow: vec![FxHashMap::default()],
            loops: Vec::new(),
            captures: Vec::new(),
            returns: Vec::new(),
            frame_local: None,
            self_local: None,
            moved: FxHashSet::default(),
            scan: Default::default(),
            span,
        });
    }

    pub fn push_scope(&mut self) {
        let c = self.ctx();
        c.scopes.push(FxHashMap::default());
        c.narrow.push(FxHashMap::default());
        c.path_narrow.push(FxHashMap::default());
    }

    pub fn pop_scope(&mut self) {
        let c = self.ctx();
        c.scopes.pop();
        c.narrow.pop();
        c.path_narrow.pop();
    }

    pub fn declare_local(&mut self, name: &str, ty: TyId, span: Span) -> LocalId {
        let id = self.new_local(name, ty, span, false);
        self.ctx().scopes.last_mut().unwrap().insert(name.to_string(), id);
        id
    }

    /// A local of the current context by name, without recording a capture.
    pub fn lookup_local_quiet(&self, name: &str) -> Option<LocalId> {
        self.ctxs.last()?.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    /// Resolves a local by name, recording captures across closure boundaries.
    pub fn lookup_local(&mut self, name: &str) -> Option<LocalId> {
        let n = self.ctxs.len();
        for ci in (0..n).rev() {
            let found = self.ctxs[ci].scopes.iter().rev().find_map(|s| s.get(name).copied());
            if let Some(id) = found {
                // Record the capture in every closure between the owner and here.
                for cj in ci + 1..n {
                    if !self.ctxs[cj].captures.contains(&id) {
                        self.ctxs[cj].captures.push(id);
                    }
                }
                if ci + 1 < n {
                    self.locals[id as usize].captured = true;
                }
                return Some(id);
            }
            if self.ctxs[ci].kind == CtxKind::Function {
                break;
            }
        }
        None
    }

    /// The current (possibly narrowed) type of a local.
    pub fn local_ty(&self, id: LocalId) -> TyId {
        let n = self.ctxs.len();
        let owner = self.locals[id as usize].ctx as usize;
        let reassigned = self.is_reassigned(id);
        for ci in (0..n).rev() {
            if ci < owner {
                break;
            }
            // Narrowing from enclosing contexts only applies to variables never reassigned.
            if ci != n - 1 && reassigned {
                break;
            }
            for layer in self.ctxs[ci].narrow.iter().rev() {
                if let Some(&t) = layer.get(&id) {
                    return t;
                }
            }
        }
        // A narrowing that a loop being checked keeps holds throughout its body (every assignment
        // to the local in the loop keeps it), even after an assignment in a branch dropped the
        // layered one. Not in a closure, which may run after the loop.
        for &(l, t, depth) in self.loop_facts.iter().rev() {
            if l == id && self.ctxs[depth.min(n)..].iter().all(|c| c.kind == CtxKind::ForBody) {
                return t;
            }
        }
        self.locals[id as usize].ty
    }

    pub fn narrow(&mut self, key: impl Into<NKey>, ty: TyId) {
        match key.into() {
            NKey::Local(id) => {
                self.ctx().narrow.last_mut().unwrap().insert(id, ty);
            }
            NKey::Path(p, stable) => {
                self.ctx().path_narrow.last_mut().unwrap().insert(p, (ty, stable));
            }
        }
    }

    /// Forgets what is known about a reassigned local, and about field paths rooted at it.
    pub fn clear_narrowing(&mut self, id: LocalId) {
        for layer in self.ctx().narrow.iter_mut() {
            layer.remove(&id);
        }
        self.clear_paths(|p, _| p.0 == id);
    }

    /// The narrowed type of a field path in the current context, if any.
    pub fn path_ty(&self, p: &FieldPath) -> Option<TyId> {
        let c = self.ctxs.last()?;
        c.path_narrow.iter().rev().find_map(|layer| layer.get(p).map(|&(t, _)| t))
    }

    /// Drops field-path narrowings matching `pred(path, stable)` in the current context.
    pub fn clear_paths(&mut self, pred: impl Fn(&FieldPath, bool) -> bool) {
        let Some(c) = self.ctxs.last_mut() else { return };
        for layer in c.path_narrow.iter_mut() {
            layer.retain(|p, &mut (_, stable)| !pred(p, stable));
        }
    }

    /// A call may change any field: drop the field-path narrowings that are not stable. It may
    /// also run a closure that assigns a local, or (on another fiber) code that does: drop the
    /// narrowings of such locals too.
    pub fn clear_unstable_paths(&mut self) {
        self.path_epoch += 1;
        self.clear_paths(|_, stable| !stable);
        let Some(c) = self.ctxs.last() else { return };
        let dying: Vec<LocalId> =
            c.narrow.iter().flat_map(|l| l.keys().copied()).filter(|&l| self.dies_at_calls(l)).collect();
        for l in dying {
            for layer in self.ctx().narrow.iter_mut() {
                layer.remove(&l);
            }
        }
    }

    /// Whether local `l` is, or may later be, assigned more than once in its function.
    pub fn is_reassigned(&self, l: LocalId) -> bool {
        let info = &self.locals[l as usize];
        info.assigns > 1 || info.reassigned || info.closure_assigned
    }

    /// Whether a narrowing of local `l` in the current context ends at any call: a closure that
    /// the call runs may assign it, or, inside a closure, its function or another closure may
    /// have reassigned it meanwhile (a fiber runs other code during calls that wait).
    pub fn dies_at_calls(&self, l: LocalId) -> bool {
        let info = &self.locals[l as usize];
        if info.closure_assigned {
            return true;
        }
        // `for` bodies run inside the loop's call to `each`, while their function waits.
        let owner = info.ctx as usize;
        self.is_reassigned(l) && self.ctxs.iter().skip(owner + 1).any(|c| c.kind != CtxKind::ForBody)
    }

    // ---- bodies ------------------------------------------------------------------------------

    pub fn check_all_bodies(&mut self) {
        let nmods = self.mods.len();
        for m in 0..nmods {
            let main = self.mods[m].main_fn;
            if self.fns[main as usize].state == CheckState::Unchecked {
                self.check_fn_body(main);
            }
        }
        let mut i = 0;
        while i < self.fns.len() {
            if self.fns[i].state == CheckState::Unchecked {
                self.check_fn_body(i as FnId);
            }
            i += 1;
        }
        for g in 0..self.globals.len() {
            self.ensure_global(g as GlobalId);
        }
        self.check_local_keys();
    }

    pub fn check_fn_body(&mut self, fid: FnId) {
        if self.fns[fid as usize].state != CheckState::Unchecked {
            if self.fns[fid as usize].state == CheckState::InProgress && !self.fns[fid as usize].ret_annotated {
                let f = &self.fns[fid as usize];
                let (name, span) = (f.name.clone(), f.span);
                self.emit(
                    Diag::error("E0230", format!("recursive function `{name}` needs a return type annotation"), span)
                        .suggest("annotate the return type", span.after(), " -> T"),
                );
                // The result is unknown from here on: an error, which fits any use (`unify` with
                // an error does not bind, so bind it here). Uses of the result can then no longer
                // pin it to a type that the body's returns contradict.
                let err = self.types.error;
                let r = self.types.shallow_resolve(self.fns[fid as usize].ret);
                if let Ty::Var(v) = self.types.get(r) {
                    let v = *v;
                    self.bind_var(v, err);
                }
            }
            return;
        }
        self.fns[fid as usize].state = CheckState::InProgress;
        let saved_module = self.cur_module;
        let saved_ctxs = std::mem::take(&mut self.ctxs);
        let f = self.fns[fid as usize].clone();
        self.cur_module = f.module;
        match f.synth {
            Synth::Main(m) => self.check_main(fid, m),
            Synth::Constructor(cid) => self.check_constructor(fid, cid),
            Synth::Getter { .. } => {}
            Synth::None => {
                if let Some(decl) = self.fn_decl(fid) {
                    if let Some(body) = &decl.body {
                        if !f.is_native {
                            self.check_decl_body(fid, &decl, body);
                        }
                    }
                }
            }
        }
        self.ctxs = saved_ctxs;
        self.cur_module = saved_module;
        self.fns[fid as usize].state = CheckState::Done;
    }

    fn check_decl_body(&mut self, fid: FnId, decl: &FnDecl, body: &Block) {
        let f = self.fns[fid as usize].clone();
        self.push_ctx(fid, CtxKind::Function, f.ret, decl.span);
        self.ctx().scan = Rc::new(AssignScan::new(&body.stmts));
        if f.self_kind != SelfKind::None {
            let s = self.new_local("self", f.self_ty, decl.span, true);
            self.ctx().scopes[0].insert("self".into(), s);
            self.ctx().self_local = Some(s);
        }
        let mut open_params = Vec::new();
        for (i, p) in f.params.iter().enumerate() {
            let ty = if f.has_untyped_params && decl.params[i].ty.is_none() {
                let o = self.opens.len() as OpenId;
                self.opens.push(OpenInfo {
                    param_name: p.name.clone(),
                    label: None,
                    reqs: Vec::new(),
                    uppers: Vec::new(),
                });
                self.types.intern(Ty::Open(o))
            } else {
                p.ty
            };
            open_params.push(ty);
            // The local's span is the parameter's name (tooling renames and references it).
            let l = self.new_local(&p.name, ty, decl.params[i].name.span, true);
            self.ctx().scopes[0].insert(p.name.clone(), l);
        }
        if f.has_untyped_params {
            self.fns[fid as usize].params.iter_mut().zip(open_params.iter()).for_each(|(p, &t)| p.ty = t);
        }
        // Default values are checked in the function's scope (they may use earlier parameters).
        for (i, p) in decl.params.iter().enumerate() {
            if let Some(d) = &p.default {
                let pt = open_params[i];
                self.check_against(d, pt, "default value");
            }
        }
        let expected = if f.ret_annotated { Some(f.ret) } else { None };
        let t = self.check_block(body, expected);
        self.finish_fn(fid, t, body.span);
        if f.has_untyped_params {
            let ret = self.fns[fid as usize].ret;
            let ret = self.types.resolve(ret);
            self.fns[fid as usize].implicit = Some(ImplicitSig { params: open_params, ret });
        }
        self.ctxs.pop();
    }

    /// Unifies the inferred result with the declared one and finalizes local types.
    fn finish_fn(&mut self, fid: FnId, body_ty: TyId, span: Span) {
        let f = self.fns[fid as usize].clone();
        let nil = self.types.nil;
        if f.ret_annotated {
            let ret = self.types.shallow_resolve(f.ret);
            if ret != nil && !self.assignable(body_ty, f.ret) {
                if self.try_int_to_float(body_ty, f.ret) {
                    // The last expression is coerced by codegen via the recorded coercion.
                } else {
                    let msg = format!(
                        "`{}` should return `{}`, but its body produces `{}`",
                        f.name,
                        self.show(f.ret),
                        self.show(body_ty)
                    );
                    self.err("E0301", msg, span);
                }
            }
        } else {
            let mut all = self.ctx().returns.clone();
            all.push(body_ty);
            let joined = self.join(&all);
            // The result is still a free variable here: only a use of it while the body is being
            // checked could have bound it, and such a (recursive) use is E0230, which binds it to
            // an error that unifies with anything.
            let unified = self.unify(f.ret, joined);
            debug_assert!(
                unified || self.diags.iter().any(|d| d.is_error()) || self.suppressed_errors > 0,
                "the inferred result of `{}` was bound before its body was checked",
                f.name
            );
        }
        // Finalize local types: unbound variables (e.g. an unused `[]`) default to Never.
        let never = self.types.never;
        if let Some(locals) = self.fn_locals.get(&fid).cloned() {
            for l in locals {
                let t = self.locals[l as usize].ty;
                self.types.default_vars(t, never);
                let r = self.types.resolve(t);
                self.locals[l as usize].ty = r;
            }
        }
        let r = self.fns[fid as usize].ret;
        self.types.default_vars(r, nil);
        let r = self.types.resolve(r);
        self.fns[fid as usize].ret = r;
    }

    pub fn try_int_to_float(&mut self, from: TyId, to: TyId) -> bool {
        let from = self.types.shallow_resolve(from);
        let to = self.types.shallow_resolve(to);
        let float = self.types.float;
        let fopt = self.types.optional(float);
        from == self.types.int && (to == float || to == fopt)
    }

    fn check_main(&mut self, fid: FnId, m: ModId) {
        let nil = self.types.nil;
        self.push_ctx(fid, CtxKind::Function, nil, Span::default());
        let items = self.mods[m as usize].ast.items.clone();
        let stmts = items.iter().filter_map(|i| if let Item::Stmt(e) = i { Some(e) } else { None });
        self.ctx().scan = Rc::new(AssignScan::new(stmts));
        let mut last = nil;
        for item in &items {
            match item {
                Item::Stmt(e) => last = self.check_stmt(e, None),
                Item::Const(c) => {
                    if let Some(ScopeItem::Global(g)) = self.mods[m as usize].items.get(&c.name.name).copied() {
                        self.ensure_global(g);
                    }
                }
                Item::Var(v) => {
                    if let Some(ScopeItem::Global(g)) = self.mods[m as usize].items.get(&v.name.name).copied() {
                        self.ensure_global(g);
                    }
                }
                Item::Class(c) => {
                    if let Some(ScopeItem::Class(cid)) = self.mods[m as usize].items.get(&c.name.name).copied() {
                        let consts: Vec<GlobalId> = self.classes[&cid].consts.values().copied().collect();
                        for g in consts {
                            self.ensure_global(g);
                        }
                    }
                }
                _ => {}
            }
        }
        let _ = last;
        let locals = self.fn_locals.get(&fid).cloned().unwrap_or_default();
        let never = self.types.never;
        for l in locals {
            let t = self.locals[l as usize].ty;
            self.types.default_vars(t, never);
            let r = self.types.resolve(t);
            self.locals[l as usize].ty = r;
        }
        self.ctxs.pop();
    }

    /// E0373 for user variables (and parameters) holding a `Dict`/`Set` with mutable keys,
    /// once every body (closures included) has been checked and their types are known.
    fn check_local_keys(&mut self) {
        let mut fids: Vec<FnId> = self.fn_locals.keys().copied().collect();
        fids.sort_unstable();
        for fid in fids {
            let f = &self.fns[fid as usize];
            // Generated constructors repeat the fields, which are checked themselves.
            if self.mods[f.module as usize].is_prelude || matches!(f.synth, Synth::Constructor(_)) {
                continue;
            }
            for l in self.fn_locals[&fid].clone() {
                let info = &self.locals[l as usize];
                if info.name == "self" || info.name.starts_with('$') {
                    continue;
                }
                let (what, ty, span) = (format!("`{}`", info.name), info.ty, info.span);
                let ty = self.types.resolve(ty);
                self.check_mutable_key(&what, ty, span);
            }
        }
    }

    /// Checks a constant's or module variable's initializer (once).
    pub fn ensure_global(&mut self, g: GlobalId) {
        match self.globals[g as usize].state {
            CheckState::Done => return,
            CheckState::InProgress => {
                let (name, span) = (self.globals[g as usize].name.clone(), self.globals[g as usize].span);
                self.err("E0231", format!("`{name}` is defined in terms of itself"), span);
                let err = self.types.error;
                let t = self.globals[g as usize].ty;
                self.unify(t, err);
                return;
            }
            CheckState::Unchecked => {}
        }
        self.globals[g as usize].state = CheckState::InProgress;
        let info = self.globals[g as usize].clone();
        let (value, annotated) = match &info.init {
            GlobalInit::Item(m, i) => match &self.mods[*m as usize].ast.items[*i] {
                Item::Const(c) => (c.value.clone(), c.ty.is_some()),
                Item::Var(v) => (v.value.clone(), true),
                _ => unreachable!(),
            },
            GlobalInit::ClassConst(cid, i) => {
                let ci = &self.classes[cid];
                let Item::Class(c) = &self.mods[ci.module as usize].ast.items[ci.item.unwrap()] else { unreachable!() };
                (c.consts[*i].value.clone(), c.consts[*i].ty.is_some())
            }
        };
        let saved_module = self.cur_module;
        let saved_ctxs = std::mem::take(&mut self.ctxs);
        self.cur_module = info.module;
        let main = self.mods[info.module as usize].main_fn;
        let nil = self.types.nil;
        self.push_ctx(main, CtxKind::Function, nil, info.span);
        let t = if annotated {
            self.check_against(&value, info.ty, "initializer");
            info.ty
        } else {
            let t = self.check_expr(&value, None);
            let t = if info.is_const { self.freeze_ty(t) } else { t };
            self.unify(info.ty, t);
            t
        };
        let never = self.types.never;
        self.types.default_vars(t, never);
        let resolved = self.types.resolve(info.ty);
        self.globals[g as usize].ty = resolved;
        if !info.is_const && !self.is_sendable(resolved) {
            // Reported at the declaration in collect for annotated vars.
        }
        self.ctxs.pop();
        self.ctxs = saved_ctxs;
        self.cur_module = saved_module;
        self.globals[g as usize].state = CheckState::Done;
    }

    /// The generated `Class.new`: field defaults are checked here.
    fn check_constructor(&mut self, fid: FnId, cid: ClassId) {
        let ci = self.classes[&cid].clone();
        let f = self.fns[fid as usize].clone();
        self.push_ctx(fid, CtxKind::Function, f.ret, ci.span);
        // `self` for default expressions.
        let s = self.new_local("self", f.ret, ci.span, true);
        self.ctx().self_local = Some(s);
        for p in &f.params {
            let l = self.new_local(&p.name, p.ty, p.span, true);
            let _ = l;
        }
        for field in &ci.fields {
            if !field.has_default {
                continue;
            }
            let dc = &self.classes[&field.decl_class];
            let Item::Class(decl) = &self.mods[dc.module as usize].ast.items[dc.item.unwrap()] else { continue };
            let Some(def) = decl.fields[field.decl_index].default.clone() else { continue };
            let saved = self.cur_module;
            self.cur_module = dc.module;
            let fty = self.lookup_field(f.ret, &field.name).map(|x| x.1).unwrap_or(field.ty);
            self.check_against(&def, fty, "field default");
            self.cur_module = saved;
        }
        // With an `init`, every field without a default must be assigned by it.
        if let Some(init) = ci.init {
            if let Some(decl) = self.fn_decl(init) {
                let assigned = assigned_self_fields(decl.body.as_ref());
                let calls_super_init = calls_super_init(decl.body.as_ref());
                for field in &ci.fields {
                    if field.has_default || assigned.contains(&field.name) {
                        continue;
                    }
                    let inherited = field.decl_class != cid;
                    if inherited && calls_super_init {
                        continue;
                    }
                    if self.fns[init as usize].owner != Owner::Class(cid) && field.decl_class != cid {
                        continue;
                    }
                    let init_owner_is_super = self.fns[init as usize].owner != Owner::Class(cid);
                    let msg = if init_owner_is_super {
                        format!(
                            "field `{}` has no default and the inherited `init` does not set it; add a default or define `init` in `{}`",
                            field.name, ci.name
                        )
                    } else {
                        format!("`init` must assign field `{}` (it has no default value)", field.name)
                    };
                    self.err("E0232", msg, if init_owner_is_super { field.span } else { self.fns[init as usize].span });
                }
            }
        }
        self.ctxs.pop();
    }

    // ---- blocks and statements ---------------------------------------------------------------

    pub fn check_block(&mut self, b: &Block, expected: Option<TyId>) -> TyId {
        self.push_scope();
        let n = b.stmts.len();
        let mut ty = self.types.nil;
        let mut diverged = false;
        for (i, s) in b.stmts.iter().enumerate() {
            let last = i + 1 == n;
            let t = self.check_stmt(s, if last { expected } else { None });
            if self.is_never(t) {
                diverged = true;
            }
            if last {
                ty = if is_valueless_stmt(s) { self.types.nil } else { t };
            }
        }
        self.pop_scope();
        if diverged && !self.is_never(ty) {
            // Code after a diverging statement still has a type, but the block never completes
            // normally if a statement diverged unconditionally at top level.
            if b.stmts.iter().any(|s| matches!(s.kind, ExprKind::Return(_) | ExprKind::Break(_) | ExprKind::Continue)) {
                return self.types.never;
            }
        }
        ty
    }

    pub fn is_never(&self, t: TyId) -> bool {
        matches!(self.types.get(self.types.shallow_resolve(t)), Ty::Never)
    }

    pub fn check_stmt(&mut self, e: &Expr, expected: Option<TyId>) -> TyId {
        match &e.kind {
            ExprKind::Assign { .. } | ExprKind::Let { .. } => {
                self.check_expr(e, None);
                self.types.nil
            }
            _ => self.check_expr(e, expected),
        }
    }
}

fn is_valueless_stmt(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Assign { .. } | ExprKind::Let { .. } | ExprKind::While { .. } | ExprKind::For { .. })
}

/// Fields assigned as `self.f = ...` anywhere at the top level of an init body.
fn assigned_self_fields(body: Option<&Block>) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    if let Some(b) = body {
        for s in &b.stmts {
            if let ExprKind::Assign { target, op: None, .. } = &s.kind {
                if let ExprKind::Member { recv, name, .. } = &target.kind {
                    if matches!(recv.kind, ExprKind::SelfValue) {
                        out.insert(name.name.clone());
                    }
                }
            }
        }
    }
    out
}

fn calls_super_init(body: Option<&Block>) -> bool {
    body.is_some_and(|b| {
        b.stmts.iter().any(|s| match &s.kind {
            ExprKind::Member { recv, name, .. } => {
                name.name == "init" && matches!(&recv.kind, ExprKind::Ident(n) if n == "super")
            }
            _ => false,
        })
    })
}
