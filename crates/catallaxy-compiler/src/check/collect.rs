//! Declaration collection and signature resolution.

use rustc_hash::FxHashMap;

use super::*;
use crate::ast::{ClassModifier, Item, SelfKind, TypeExpr, TypeExprKind};

/// Names in scope while resolving a type expression.
#[derive(Clone, Default)]
pub struct TypeScope {
    pub params: FxHashMap<String, ParamId>,
    pub self_ty: Option<TyId>,
    /// Declare unknown single-word names as fresh parameters (extension targets).
    pub implicit_params: bool,
}

const BUILTIN_CLASSES: &[(u32, &str, &[&str])] = &[
    (builtin::INT, "Int", &[]),
    (builtin::FLOAT, "Float", &[]),
    (builtin::BOOL, "Bool", &[]),
    (builtin::NIL, "Nil", &[]),
    (builtin::STRING, "String", &[]),
    (builtin::LIST, "List", &["T"]),
    (builtin::DICT, "Dict", &["K", "V"]),
    (builtin::SET, "Set", &["T"]),
    (builtin::STRING_BUILDER, "StringBuilder", &[]),
    (builtin::MUTEX, "Mutex", &["T"]),
    (builtin::ATOMIC, "Atomic", &["T"]),
    (builtin::CHANNEL, "Channel", &["T"]),
    (builtin::THREAD, "Thread", &["T"]),
    (builtin::RANGE, "Range", &[]),
];

impl Checker {
    /// Adds a parsed module. Modules must be added in dependency order, the prelude first.
    pub fn add_module(&mut self, name: &str, file: u32, ast: ast::Module, is_prelude: bool) -> ModId {
        let id = self.mods.len() as ModId;
        self.mods.push(ModInfo {
            name: name.to_string(),
            file,
            ast,
            items: FxHashMap::default(),
            imported: FxHashMap::default(),
            imports: Vec::new(),
            extends: Vec::new(),
            main_fn: 0,
            is_prelude,
        });
        if is_prelude && !self.classes.contains_key(&builtin::INT) {
            self.declare_builtins(id);
        }
        id
    }

    fn declare_builtins(&mut self, m: ModId) {
        for &(cid, name, params) in BUILTIN_CLASSES {
            let ps: Vec<ParamId> = params.iter().map(|p| self.new_param(p, vec![])).collect();
            self.classes.insert(
                cid,
                ClassInfo {
                    id: cid,
                    name: name.to_string(),
                    module: m,
                    item: None,
                    params: ps,
                    modifier: ClassModifier::Plain,
                    superclass: None,
                    fields: Vec::new(),
                    methods: FxHashMap::default(),
                    consts: FxHashMap::default(),
                    is_builtin: true,
                    is_pub: true,
                    span: Span::default(),
                    subclasses: Vec::new(),
                    init: None,
                    constructor: None,
                    getters: FxHashMap::default(),
                    vslots: FxHashMap::default(),
                    vtable: Vec::new(),
                    doc: None,
                },
            );
            self.mods[m as usize].items.insert(name.to_string(), ScopeItem::Class(cid));
        }
    }

    fn new_fn(&mut self, name: &str, module: ModId, owner: Owner, span: Span) -> FnId {
        let id = self.fns.len() as FnId;
        let err = self.types.error;
        self.fns.push(FnInfo {
            name: name.to_string(),
            module,
            owner,
            owner_params: Vec::new(),
            type_params: Vec::new(),
            self_kind: SelfKind::None,
            self_ty: err,
            params: Vec::new(),
            ret: err,
            ret_annotated: true,
            is_pub: false,
            is_native: false,
            native_name: None,
            mutating: false,
            decl: DeclRef::Synthetic,
            synth: Synth::None,
            span,
            implicit: None,
            has_untyped_params: false,
            state: CheckState::Unchecked,
            func_id: id,
            is_default: false,
            doc: None,
        });
        id
    }

    fn declare(&mut self, m: ModId, name: &ast::Ident, item: ScopeItem) {
        if let Some(prev) = self.mods[m as usize].items.get(&name.name) {
            if !matches!(prev, ScopeItem::Class(c) if *c < builtin::FIRST_USER) {
                self.err("E0201", format!("`{}` is declared more than once in this module", name.name), name.span);
                // The duplicate is never resolved; don't check its body either.
                if let ScopeItem::Fn(f) = item {
                    self.fns[f as usize].state = CheckState::Done;
                }
                return;
            }
        }
        self.mods[m as usize].items.insert(name.name.clone(), item);
    }

    /// Phase A: declare every item's name and allocate ids.
    pub fn declare_items(&mut self, m: ModId) {
        self.cur_module = m;
        let main = self.new_fn("<main>", m, Owner::None, Span::default());
        self.fns[main as usize].synth = Synth::Main(m);
        self.fns[main as usize].ret = self.types.nil;
        self.mods[m as usize].main_fn = main;
        let n = self.mods[m as usize].ast.items.len();
        for i in 0..n {
            let item = self.mods[m as usize].ast.items[i].clone();
            match &item {
                Item::Class(c) => {
                    let cid = self.next_class_id;
                    self.next_class_id += 1;
                    self.classes.insert(
                        cid,
                        ClassInfo {
                            id: cid,
                            name: c.name.name.clone(),
                            module: m,
                            item: Some(i),
                            params: Vec::new(),
                            modifier: c.modifier,
                            superclass: None,
                            fields: Vec::new(),
                            methods: FxHashMap::default(),
                            consts: FxHashMap::default(),
                            is_builtin: false,
                            is_pub: c.is_pub,
                            span: c.name.span,
                            subclasses: Vec::new(),
                            init: None,
                            constructor: None,
                            getters: FxHashMap::default(),
                            vslots: FxHashMap::default(),
                            vtable: Vec::new(),
                            doc: c.doc.clone(),
                        },
                    );
                    self.declare(m, &c.name, ScopeItem::Class(cid));
                }
                Item::Enum(e) => {
                    let eid = self.enums.len() as EnumId;
                    let mut variants = Vec::new();
                    for (tag, v) in e.variants.iter().enumerate() {
                        let cid = self.next_class_id;
                        self.next_class_id += 1;
                        self.variant_of_class.insert(cid, (eid, tag as u32));
                        variants.push(VariantInfo { name: v.name.name.clone(), fields: Vec::new(), class_id: cid, span: v.span });
                    }
                    self.enums.push(EnumInfo {
                        id: eid,
                        name: e.name.name.clone(),
                        module: m,
                        item: i,
                        params: Vec::new(),
                        variants,
                        methods: FxHashMap::default(),
                        is_pub: e.is_pub,
                        span: e.name.span,
                        doc: e.doc.clone(),
                    });
                    self.declare(m, &e.name, ScopeItem::Enum(eid));
                    if self.mods[m as usize].is_prelude && e.name.name == "Result" {
                        self.well_known.result = Some(eid);
                        for (tag, v) in e.variants.iter().enumerate() {
                            self.mods[m as usize].items.insert(v.name.name.clone(), ScopeItem::Variant(eid, tag as u32));
                        }
                    }
                }
                Item::Interface(f) => {
                    let iid = self.ifaces.len() as IfaceId;
                    let sp = self.new_param("Self", vec![]);
                    self.ifaces.push(IfaceInfo {
                        id: iid,
                        name: f.name.name.clone(),
                        module: m,
                        item: Some(i),
                        params: Vec::new(),
                        self_param: sp,
                        methods: Vec::new(),
                        by_name: FxHashMap::default(),
                        is_pub: f.is_pub,
                        span: f.name.span,
                        rt_index: None,
                        doc: f.doc.clone(),
                    });
                    self.declare(m, &f.name, ScopeItem::Iface(iid));
                    if self.mods[m as usize].is_prelude {
                        self.prelude_ifaces.push(iid);
                        match f.name.name.as_str() {
                            "Enumerable" => self.well_known.enumerable = Some(iid),
                            "Comparable" => self.well_known.comparable = Some(iid),
                            "Display" => self.well_known.display = Some(iid),
                            _ => {}
                        }
                    }
                }
                Item::Fn(f) => {
                    let fid = self.new_fn(&f.name.name, m, Owner::None, f.name.span);
                    self.fns[fid as usize].decl = DeclRef::Item(m, i);
                    self.fns[fid as usize].is_pub = f.is_pub;
                    self.fns[fid as usize].doc = f.doc.clone();
                    self.declare(m, &f.name, ScopeItem::Fn(fid));
                }
                Item::Const(c) => {
                    let gid = self.new_global(&c.name.name, true, m, c.is_pub, c.name.span, GlobalInit::Item(m, i));
                    self.declare(m, &c.name, ScopeItem::Global(gid));
                }
                Item::Var(v) => {
                    let gid = self.new_global(&v.name.name, false, m, v.is_pub, v.name.span, GlobalInit::Item(m, i));
                    self.declare(m, &v.name, ScopeItem::Global(gid));
                }
                Item::Extend(_) | Item::Import(_) | Item::Stmt(_) => {}
            }
        }
    }

    fn new_global(&mut self, name: &str, is_const: bool, m: ModId, is_pub: bool, span: Span, init: GlobalInit) -> GlobalId {
        let gid = self.globals.len() as GlobalId;
        let ty = self.types.fresh_var();
        self.globals.push(GlobalInfo {
            name: name.to_string(),
            ty,
            is_const,
            module: m,
            index: gid,
            is_pub,
            span,
            state: CheckState::Unchecked,
            init,
        });
        gid
    }

    /// Phase B: resolve `import` items (after all modules are declared).
    pub fn resolve_imports(&mut self, m: ModId, module_ids: &FxHashMap<String, ModId>) {
        let items = self.mods[m as usize].ast.items.clone();
        for item in &items {
            let Item::Import(imp) = item else { continue };
            let path: Vec<String> = imp.path.iter().map(|p| p.name.clone()).collect();
            let key = path.join(".");
            let Some(&target) = module_ids.get(&key) else {
                // The driver reports missing modules.
                continue;
            };
            if !self.mods[m as usize].imports.contains(&target) {
                self.mods[m as usize].imports.push(target);
            }
            match &imp.items {
                None => {
                    let alias = path.last().unwrap().clone();
                    self.mods[m as usize].imported.insert(alias, (ScopeItem::Module(target), imp.span));
                }
                Some(list) => {
                    for name in list {
                        match self.mods[target as usize].items.get(&name.name).copied() {
                            Some(si) if self.item_is_pub(si) => {
                                self.mods[m as usize].imported.insert(name.name.clone(), (si, name.span));
                            }
                            Some(_) => self.err(
                                "E0202",
                                format!("`{}` is private to module `{}`", name.name, key),
                                name.span,
                            ),
                            None => self.err("E0203", format!("module `{key}` has no item `{}`", name.name), name.span),
                        }
                    }
                }
            }
        }
    }

    pub fn item_is_pub(&self, si: ScopeItem) -> bool {
        match si {
            ScopeItem::Class(c) => self.classes[&c].is_pub,
            ScopeItem::Enum(e) => self.enums[e as usize].is_pub,
            ScopeItem::Iface(i) => self.ifaces[i as usize].is_pub,
            ScopeItem::Fn(f) => self.fns[f as usize].is_pub,
            ScopeItem::Global(g) => self.globals[g as usize].is_pub,
            ScopeItem::Module(_) => false,
            ScopeItem::Variant(..) => true,
        }
    }

    /// Looks a name up in a module: its items, its imports, then the prelude.
    pub fn lookup_name(&self, m: ModId, name: &str) -> Option<ScopeItem> {
        let md = &self.mods[m as usize];
        if let Some(&si) = md.items.get(name) {
            return Some(si);
        }
        if let Some(&(si, _)) = md.imported.get(name) {
            return Some(si);
        }
        for p in self.mods.iter().filter(|p| p.is_prelude) {
            if let Some(&si) = p.items.get(name) {
                if self.item_is_pub(si) || matches!(si, ScopeItem::Class(c) if c < builtin::FIRST_USER) {
                    return Some(si);
                }
            }
        }
        None
    }

    // ---- type expressions --------------------------------------------------------------------

    pub fn resolve_type(&mut self, te: &TypeExpr, scope: &mut TypeScope) -> TyId {
        match &te.kind {
            TypeExprKind::Error => self.types.error,
            TypeExprKind::SelfType => match scope.self_ty {
                Some(t) => t,
                None => {
                    self.err("E0210", "`Self` is only allowed inside a class or interface", te.span);
                    self.types.error
                }
            },
            TypeExprKind::Optional(inner) => {
                let t = self.resolve_type(inner, scope);
                self.types.optional(t)
            }
            TypeExprKind::Union(parts) => {
                let ts = parts.iter().map(|p| self.resolve_type(p, scope)).collect();
                self.types.union(ts)
            }
            TypeExprKind::Inter(parts) => {
                let ts: Vec<TyId> = parts.iter().map(|p| self.resolve_type(p, scope)).collect();
                for (p, &t) in parts.iter().zip(ts.iter()) {
                    if !matches!(self.types.get(t), Ty::Iface(..) | Ty::Error | Ty::Inter(_)) {
                        self.err("E0211", format!("`&` combines interfaces, but `{}` is not one", self.show(t)), p.span);
                    }
                }
                self.types.inter(ts)
            }
            TypeExprKind::Fn { params, ret } => {
                let ps = params.iter().map(|p| self.resolve_type(p, scope)).collect();
                let r = match ret {
                    Some(r) => self.resolve_type(r, scope),
                    None => self.types.nil,
                };
                self.types.func(ps, r)
            }
            TypeExprKind::Tuple(parts) => {
                let ts = parts.iter().map(|p| self.resolve_type(p, scope)).collect();
                self.types.tuple(ts)
            }
            TypeExprKind::Frozen(inner) => {
                let t = self.resolve_type(inner, scope);
                self.freeze_ty(t)
            }
            TypeExprKind::Inline(methods) => self.inline_iface(methods, scope, te.span),
            TypeExprKind::Named { path, args } => self.resolve_named(path, args, scope, te.span),
        }
    }

    fn resolve_named(&mut self, path: &[ast::Ident], args: &[TypeExpr], scope: &mut TypeScope, span: Span) -> TyId {
        let m = self.cur_module;
        let name = &path.last().unwrap().name;
        let arg_tys: Vec<TyId> = args.iter().map(|a| self.resolve_type(a, scope)).collect();
        if path.len() == 1 {
            if let Some(&p) = scope.params.get(name) {
                if !args.is_empty() {
                    self.err("E0212", format!("type parameter `{name}` takes no type arguments"), span);
                }
                return self.param_ty(p);
            }
            match name.as_str() {
                "Dyn" => return self.types.dyn_,
                "Never" => return self.types.never,
                _ => {}
            }
        }
        let item = if path.len() == 1 {
            self.lookup_name(m, name)
        } else {
            // module.Type
            match self.lookup_name(m, &path[0].name) {
                Some(ScopeItem::Module(target)) => match self.mods[target as usize].items.get(name).copied() {
                    Some(si) if self.item_is_pub(si) => Some(si),
                    Some(_) => {
                        self.err("E0202", format!("`{name}` is private"), span);
                        return self.types.error;
                    }
                    None => None,
                },
                _ => None,
            }
        };
        let (params, mk): (Vec<ParamId>, Box<dyn Fn(&mut Types, Vec<TyId>) -> TyId>) = match item {
            Some(ScopeItem::Class(c)) => (self.classes[&c].params.clone(), Box::new(move |t, a| t.class(c, a))),
            Some(ScopeItem::Enum(e)) => {
                (self.enums[e as usize].params.clone(), Box::new(move |t, a| t.intern(Ty::Enum(e, a))))
            }
            Some(ScopeItem::Iface(i)) => {
                (self.ifaces[i as usize].params.clone(), Box::new(move |t, a| t.intern(Ty::Iface(i, a))))
            }
            _ => {
                if scope.implicit_params && path.len() == 1 && args.is_empty() {
                    let p = self.new_param(name, vec![]);
                    scope.params.insert(name.clone(), p);
                    return self.param_ty(p);
                }
                let mut d = Diag::error("E0213", format!("unknown type `{}`", name), span);
                if let Some(s) = self.suggest_type(name) {
                    d = d.fix(format!("did you mean `{s}`?"), path.last().unwrap().span, s);
                }
                self.emit(d);
                return self.types.error;
            }
        };
        if arg_tys.len() != params.len() {
            if arg_tys.is_empty() && !params.is_empty() {
                self.err(
                    "E0214",
                    format!("`{name}` needs {} type argument{}", params.len(), if params.len() == 1 { "" } else { "s" }),
                    span,
                );
            } else {
                self.err("E0214", format!("`{name}` takes {} type arguments, got {}", params.len(), arg_tys.len()), span);
            }
            return self.types.error;
        }
        mk(&mut self.types, arg_tys)
    }

    fn suggest_type(&self, name: &str) -> Option<String> {
        let m = self.cur_module;
        let mut best: Option<(usize, String)> = None;
        let mut consider = |cand: &str| {
            if !cand.starts_with(|c: char| c.is_uppercase()) {
                return;
            }
            let d = crate::util::edit_distance(&name.to_lowercase(), &cand.to_lowercase());
            if d <= 2 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((d, cand.to_string()));
            }
        };
        for k in self.mods[m as usize].items.keys() {
            consider(k);
        }
        for p in self.mods.iter().filter(|p| p.is_prelude) {
            for k in p.items.keys() {
                consider(k);
            }
        }
        best.map(|(_, s)| s)
    }

    fn inline_iface(&mut self, methods: &[ast::FnDecl], scope: &mut TypeScope, span: Span) -> TyId {
        let iid = self.ifaces.len() as IfaceId;
        let sp = self.new_param("Self", vec![]);
        self.ifaces.push(IfaceInfo {
            id: iid,
            name: "{ ... }".into(),
            module: self.cur_module,
            item: None,
            params: Vec::new(),
            self_param: sp,
            methods: Vec::new(),
            by_name: FxHashMap::default(),
            is_pub: true,
            span,
            rt_index: None,
            doc: None,
        });
        let self_ty = self.param_ty(sp);
        let iface_ty = self.types.intern(Ty::Iface(iid, vec![]));
        self.params[sp as usize].bounds = vec![iface_ty];
        let mut sc = scope.clone();
        sc.self_ty = Some(self_ty);
        for md in methods {
            let fid = self.new_fn(&md.name.name, self.cur_module, Owner::Iface(iid), md.name.span);
            self.fns[fid as usize].self_ty = self_ty;
            self.fill_sig(fid, md, &mut sc, false);
            self.ifaces[iid as usize].methods.push(fid);
            self.ifaces[iid as usize].by_name.insert(md.name.name.clone(), fid);
        }
        iface_ty
    }

    fn type_params(&mut self, tps: &[ast::TypeParam], scope: &mut TypeScope) -> Vec<ParamId> {
        let ids: Vec<ParamId> = tps
            .iter()
            .map(|tp| {
                let p = self.new_param(&tp.name.name, vec![]);
                scope.params.insert(tp.name.name.clone(), p);
                p
            })
            .collect();
        for (tp, &p) in tps.iter().zip(ids.iter()) {
            let bounds: Vec<TyId> = tp.bounds.iter().map(|b| self.resolve_type(b, scope)).collect();
            self.params[p as usize].bounds = bounds;
        }
        ids
    }

    /// Fills a function's signature from its declaration.
    fn fill_sig(&mut self, fid: FnId, d: &ast::FnDecl, scope: &mut TypeScope, is_free_fn: bool) {
        let mut sc = scope.clone();
        let tps = self.type_params(&d.type_params, &mut sc);
        let mut params = Vec::new();
        let mut untyped = false;
        for p in &d.params {
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t, &mut sc),
                None => {
                    untyped = true;
                    if !is_free_fn {
                        self.err("E0215", format!("parameter `{}` needs a type annotation", p.name.name), p.name.span);
                        self.types.error
                    } else if d.is_pub {
                        self.emit(
                            Diag::error(
                                "E0216",
                                format!("parameter `{}` of a `pub` function needs a type annotation", p.name.name),
                                p.name.span,
                            )
                            .note("public signatures are contracts; an interface type such as `Enumerable[Int]` keeps them flexible"),
                        );
                        self.types.error
                    } else {
                        self.types.error
                    }
                }
            };
            params.push(PInfo {
                name: p.name.name.clone(),
                ty,
                has_default: p.default.is_some(),
                is_move: p.is_move,
                span: p.span,
            });
        }
        let (ret, annotated) = match &d.ret {
            Some(r) => (self.resolve_type(r, &mut sc), true),
            None if d.body.is_none() || d.is_native => (self.types.nil, true),
            None if is_free_fn && d.is_pub => (self.types.nil, true),
            None => (self.types.fresh_var(), false),
        };
        let f = &mut self.fns[fid as usize];
        f.type_params = tps;
        f.params = params;
        f.ret = ret;
        f.ret_annotated = annotated;
        f.self_kind = d.self_kind;
        f.is_native = d.is_native;
        f.is_pub = f.is_pub || d.is_pub;
        f.has_untyped_params = untyped && is_free_fn && !d.is_pub;
        f.mutating = d.self_kind == SelfKind::Mut;
        if d.is_native && d.body.is_some() {
            self.err("E0217", "a `native fn` cannot have a body", d.span);
        }
        if !d.is_native && d.body.is_none() && is_free_fn {
            self.err("E0218", format!("function `{}` needs a body", d.name.name), d.span);
        }
    }

    /// Phase C0: declare the type parameters of classes, enums and interfaces, so signatures
    /// anywhere can refer to them regardless of declaration order.
    pub fn resolve_type_params(&mut self, m: ModId) {
        self.cur_module = m;
        let items = self.mods[m as usize].ast.items.clone();
        for item in &items {
            let (name, tps) = match item {
                Item::Class(c) => (&c.name.name, &c.type_params),
                Item::Enum(e) => (&e.name.name, &e.type_params),
                Item::Interface(f) => (&f.name.name, &f.type_params),
                _ => continue,
            };
            let mut scope = TypeScope::default();
            let ids: Vec<ParamId> = tps
                .iter()
                .map(|tp| {
                    let p = self.new_param(&tp.name.name, vec![]);
                    scope.params.insert(tp.name.name.clone(), p);
                    p
                })
                .collect();
            match self.mods[m as usize].items.get(name).copied() {
                Some(ScopeItem::Class(c)) => self.classes.get_mut(&c).unwrap().params = ids.clone(),
                Some(ScopeItem::Enum(e)) => self.enums[e as usize].params = ids.clone(),
                Some(ScopeItem::Iface(i)) => self.ifaces[i as usize].params = ids.clone(),
                _ => continue,
            }
            self.pending_bounds.push((m, ids, tps.clone(), scope));
        }
    }

    /// Resolves type-parameter bounds (after every module's parameters exist).
    pub fn resolve_bounds(&mut self) {
        for (m, ids, tps, mut scope) in std::mem::take(&mut self.pending_bounds) {
            self.cur_module = m;
            for (tp, &p) in tps.iter().zip(ids.iter()) {
                let bounds: Vec<TyId> = tp.bounds.iter().map(|b| self.resolve_type(b, &mut scope)).collect();
                self.params[p as usize].bounds = bounds;
            }
        }
    }

    fn scope_with(&self, params: &[ParamId]) -> TypeScope {
        let mut sc = TypeScope::default();
        for &p in params {
            sc.params.insert(self.params[p as usize].name.clone(), p);
        }
        sc
    }

    /// Phase C: resolve signatures of every item in module `m`.
    pub fn resolve_signatures(&mut self, m: ModId) {
        self.cur_module = m;
        let items = self.mods[m as usize].ast.items.clone();
        let is_prelude = self.mods[m as usize].is_prelude;
        // Classes first (in inheritance order), so field lists can be inherited.
        for (i, item) in items.iter().enumerate() {
            if let Item::Class(c) = item {
                let cid = match self.mods[m as usize].items.get(&c.name.name) {
                    Some(ScopeItem::Class(cid)) => *cid,
                    _ => continue,
                };
                if self.classes[&cid].item == Some(i) {
                    self.resolve_class(cid, c);
                }
            }
        }
        for (i, item) in items.iter().enumerate() {
            match item {
                Item::Fn(f) => {
                    let Some(ScopeItem::Fn(fid)) = self.mods[m as usize].items.get(&f.name.name).copied() else { continue };
                    if self.fns[fid as usize].decl != DeclRef::Item(m, i) {
                        continue;
                    }
                    let mut sc = TypeScope::default();
                    self.fill_sig(fid, f, &mut sc, true);
                    if f.is_native {
                        self.fns[fid as usize].native_name = Some(f.name.name.clone());
                    }
                }
                Item::Enum(e) => {
                    let Some(ScopeItem::Enum(eid)) = self.mods[m as usize].items.get(&e.name.name).copied() else { continue };
                    if self.enums[eid as usize].item != i {
                        continue;
                    }
                    self.resolve_enum(eid, e, i);
                }
                Item::Interface(f) => {
                    let Some(ScopeItem::Iface(iid)) = self.mods[m as usize].items.get(&f.name.name).copied() else { continue };
                    if self.ifaces[iid as usize].item != Some(i) {
                        continue;
                    }
                    self.resolve_iface(iid, f, i);
                }
                Item::Extend(x) => self.resolve_extend(x, i, is_prelude),
                Item::Const(c) => {
                    if let Some(t) = &c.ty {
                        let Some(ScopeItem::Global(g)) = self.mods[m as usize].items.get(&c.name.name).copied() else { continue };
                        let ty = self.resolve_type(t, &mut TypeScope::default());
                        self.globals[g as usize].ty = ty;
                    }
                }
                Item::Var(v) => {
                    let Some(ScopeItem::Global(g)) = self.mods[m as usize].items.get(&v.name.name).copied() else { continue };
                    let ty = self.resolve_type(&v.ty, &mut TypeScope::default());
                    self.globals[g as usize].ty = ty;
                    if self.cap_of(ty) == caps::Cap::Owned {
                        self.emit(
                            Diag::error(
                                "E0601",
                                format!(
                                    "module variable `{}` has type `{}`, which cannot be shared between threads",
                                    v.name.name,
                                    self.show(ty)
                                ),
                                v.ty.span,
                            )
                            .note("module variables are visible to every thread; use a frozen type, `Mutex[T]` or `Atomic[T]`"),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn resolve_class(&mut self, cid: ClassId, c: &ast::ClassDecl) {
        if !self.resolved_classes.insert(cid) {
            return;
        }
        let m = self.cur_module;
        let item = self.classes[&cid].item.unwrap();
        let params = self.classes[&cid].params.clone();
        let mut scope = self.scope_with(&params);
        let ptys: Vec<TyId> = params.iter().map(|&p| self.param_ty(p)).collect();
        let self_ty = self.types.class(cid, ptys);
        scope.self_ty = Some(self_ty);
        // Superclass (resolved first so inherited fields are known).
        let mut fields = Vec::new();
        if let Some(sup) = &c.superclass {
            let st = self.resolve_type(sup, &mut scope);
            match self.types.get(st).clone() {
                Ty::Class(sc, _) if sc >= builtin::FIRST_USER => {
                    if sc == cid || self.is_subclass(sc, cid) {
                        self.err("E0219", "a class cannot inherit from itself", sup.span);
                    } else {
                        // Make sure the superclass is resolved.
                        let sm = self.classes[&sc].module;
                        let sitem = self.classes[&sc].item.unwrap();
                        if let Item::Class(sdecl) = self.mods[sm as usize].ast.items[sitem].clone() {
                            let saved = self.cur_module;
                            self.cur_module = sm;
                            self.resolve_class(sc, &sdecl);
                            self.cur_module = saved;
                        }
                        if self.classes[&sc].modifier != c.modifier {
                            self.err(
                                "E0220",
                                "a subclass must have the same `frozen`/`shared` modifier as its superclass",
                                sup.span,
                            );
                        }
                        self.classes.get_mut(&cid).unwrap().superclass = Some(st);
                        self.classes.get_mut(&sc).unwrap().subclasses.push(cid);
                        fields = self.classes[&sc].fields.clone();
                    }
                }
                Ty::Error => {}
                _ => self.err("E0221", format!("cannot inherit from `{}`", self.show(st)), sup.span),
            }
        }
        for (fi, f) in c.fields.iter().enumerate() {
            if fields.iter().any(|x: &FieldInfo| x.name == f.name.name) {
                self.err("E0222", format!("field `{}` is already defined", f.name.name), f.name.span);
                continue;
            }
            let ty = self.resolve_type(&f.ty, &mut scope);
            let cap = self.cap_of(ty);
            match c.modifier {
                ClassModifier::Frozen if cap != caps::Cap::Frozen => self.emit(
                    Diag::error(
                        "E0602",
                        format!("field `{}` of a frozen class must have a frozen type, not `{}`", f.name.name, self.show(ty)),
                        f.ty.span,
                    )
                    .fix("make the field type frozen", f.ty.span.empty_at(), "frozen "),
                ),
                ClassModifier::Shared if cap == caps::Cap::Owned => self.emit(
                    Diag::error(
                        "E0603",
                        format!(
                            "field `{}` of a shared class must be frozen or shareable, not `{}`",
                            f.name.name,
                            self.show(ty)
                        ),
                        f.ty.span,
                    )
                    .note("wrap mutable state in `Mutex[T]` or `Atomic[T]`"),
                ),
                _ => {}
            }
            fields.push(FieldInfo {
                name: f.name.name.clone(),
                ty,
                decl_class: cid,
                decl_index: fi,
                has_default: f.default.is_some(),
                span: f.name.span,
            });
        }
        self.classes.get_mut(&cid).unwrap().fields = fields;
        // Methods.
        for (mi, md) in c.methods.iter().enumerate() {
            if self.classes[&cid].methods.contains_key(&md.name.name) {
                self.err("E0223", format!("method `{}` is already defined", md.name.name), md.name.span);
                continue;
            }
            let fid = self.new_fn(&md.name.name, m, Owner::Class(cid), md.name.span);
            self.fns[fid as usize].decl = DeclRef::Method(m, item, mi);
            self.fns[fid as usize].owner_params = params.clone();
            self.fns[fid as usize].self_ty = self_ty;
            self.fns[fid as usize].doc = md.doc.clone();
            self.fill_sig(fid, md, &mut scope, false);
            if md.is_native {
                self.fns[fid as usize].native_name = Some(format!("{}.{}", c.name.name, md.name.name));
            }
            if md.name.name == "init" {
                if md.self_kind == SelfKind::None {
                    self.err("E0224", "`init` must take `self`", md.name.span);
                }
                self.classes.get_mut(&cid).unwrap().init = Some(fid);
            }
            self.classes.get_mut(&cid).unwrap().methods.insert(md.name.name.clone(), fid);
        }
        // Class constants.
        for (ci, k) in c.consts.iter().enumerate() {
            let gid = self.new_global(
                &format!("{}.{}", c.name.name, k.name.name),
                true,
                m,
                true,
                k.name.span,
                GlobalInit::ClassConst(cid, ci),
            );
            if let Some(t) = &k.ty {
                let ty = self.resolve_type(t, &mut scope);
                self.globals[gid as usize].ty = ty;
            }
            self.classes.get_mut(&cid).unwrap().consts.insert(k.name.name.clone(), gid);
        }
    }

    pub fn is_subclass(&self, c: ClassId, of: ClassId) -> bool {
        let mut cur = Some(c);
        let mut n = 0;
        while let Some(x) = cur {
            if x == of {
                return true;
            }
            n += 1;
            if n > 64 {
                return false;
            }
            cur = self.classes.get(&x).and_then(|ci| ci.superclass).and_then(|t| match self.types.get(t) {
                Ty::Class(sc, _) => Some(*sc),
                _ => None,
            });
        }
        false
    }

    fn resolve_enum(&mut self, eid: EnumId, e: &ast::EnumDecl, item: usize) {
        let m = self.cur_module;
        let params = self.enums[eid as usize].params.clone();
        let mut scope = self.scope_with(&params);
        let _ = &e.type_params;
        let ptys: Vec<TyId> = params.iter().map(|&p| self.param_ty(p)).collect();
        let self_ty = self.types.intern(Ty::Enum(eid, ptys));
        scope.self_ty = Some(self_ty);
        for (vi, v) in e.variants.iter().enumerate() {
            let fields: Vec<(Option<String>, TyId)> =
                v.fields.iter().map(|(n, t)| (n.as_ref().map(|n| n.name.clone()), self.resolve_type(t, &mut scope))).collect();
            self.enums[eid as usize].variants[vi].fields = fields;
        }
        for (mi, md) in e.methods.iter().enumerate() {
            let fid = self.new_fn(&md.name.name, m, Owner::Enum(eid), md.name.span);
            self.fns[fid as usize].decl = DeclRef::Method(m, item, mi);
            self.fns[fid as usize].owner_params = params.clone();
            self.fns[fid as usize].self_ty = self_ty;
            self.fill_sig(fid, md, &mut scope, false);
            if md.is_native {
                self.fns[fid as usize].native_name = Some(format!("{}.{}", e.name.name, md.name.name));
            }
            self.enums[eid as usize].methods.insert(md.name.name.clone(), fid);
        }
    }

    fn resolve_iface(&mut self, iid: IfaceId, f: &ast::InterfaceDecl, item: usize) {
        let m = self.cur_module;
        let params = self.ifaces[iid as usize].params.clone();
        let mut scope = self.scope_with(&params);
        let _ = &f.type_params;
        let ptys: Vec<TyId> = params.iter().map(|&p| self.param_ty(p)).collect();
        let iface_ty = self.types.intern(Ty::Iface(iid, ptys));
        let sp = self.ifaces[iid as usize].self_param;
        self.params[sp as usize].bounds = vec![iface_ty];
        let self_ty = self.param_ty(sp);
        scope.self_ty = Some(self_ty);
        for (mi, md) in f.methods.iter().enumerate() {
            let fid = self.new_fn(&md.name.name, m, Owner::Iface(iid), md.name.span);
            self.fns[fid as usize].decl = DeclRef::Method(m, item, mi);
            let mut ops = params.clone();
            ops.push(sp);
            self.fns[fid as usize].owner_params = ops;
            self.fns[fid as usize].self_ty = self_ty;
            self.fns[fid as usize].is_default = md.body.is_some();
            self.fns[fid as usize].is_pub = true;
            self.fill_sig(fid, md, &mut scope, false);
            if md.self_kind == SelfKind::None {
                self.err("E0225", "interface methods must take `self`", md.name.span);
            }
            self.ifaces[iid as usize].methods.push(fid);
            self.ifaces[iid as usize].by_name.insert(md.name.name.clone(), fid);
        }
    }

    fn resolve_extend(&mut self, x: &ast::ExtendDecl, item: usize, is_prelude: bool) {
        let m = self.cur_module;
        let mut scope = TypeScope::default();
        let mut params = self.type_params(&x.type_params, &mut scope);
        // Builtin types extended by the prelude in their generic form get class methods.
        let mut class_target = None;
        if let TypeExprKind::Named { path, args } = &x.target.kind {
            if path.len() == 1 {
                if let Some(ScopeItem::Class(cid)) = self.lookup_name(m, &path[0].name) {
                    let ci = &self.classes[&cid];
                    let simple = args.iter().all(|a| match &a.kind {
                        TypeExprKind::Named { path, args } => {
                            path.len() == 1 && args.is_empty() && self.lookup_name(m, &path[0].name).is_none()
                        }
                        _ => false,
                    });
                    if is_prelude && ci.is_builtin && simple && args.len() == ci.params.len() && x.type_params.is_empty() {
                        for (a, &p) in args.iter().zip(ci.params.iter()) {
                            if let TypeExprKind::Named { path, .. } = &a.kind {
                                scope.params.insert(path[0].name.clone(), p);
                            }
                        }
                        params = ci.params.clone();
                        class_target = Some(cid);
                    }
                }
            }
        }
        scope.implicit_params = true;
        let before: Vec<String> = scope.params.keys().cloned().collect();
        let target = self.resolve_type(&x.target, &mut scope);
        scope.implicit_params = false;
        for (name, &p) in &scope.params {
            if !before.contains(name) && !params.contains(&p) {
                params.push(p);
            }
        }
        if let Some(cid) = class_target {
            // Methods become class methods of the builtin type.
            scope.self_ty = Some(target);
            for (mi, md) in x.methods.iter().enumerate() {
                let fid = self.new_fn(&md.name.name, m, Owner::Class(cid), md.name.span);
                self.fns[fid as usize].decl = DeclRef::Method(m, item, mi);
                self.fns[fid as usize].owner_params = params.clone();
                self.fns[fid as usize].self_ty = target;
                self.fns[fid as usize].is_pub = true;
                self.fill_sig(fid, md, &mut scope, false);
                if md.is_native {
                    let cname = self.classes[&cid].name.clone();
                    self.fns[fid as usize].native_name = Some(format!("{}.{}", cname, md.name.name));
                }
                if self.classes[&cid].methods.contains_key(&md.name.name) {
                    self.err("E0223", format!("method `{}` is already defined", md.name.name), md.name.span);
                }
                self.classes.get_mut(&cid).unwrap().methods.insert(md.name.name.clone(), fid);
            }
            let eid = self.exts.len() as ExtId;
            self.exts.push(ExtendInfo {
                module: m,
                item,
                params,
                target,
                methods: FxHashMap::default(),
                is_pub: true,
                is_class_methods: true,
            });
            self.mods[m as usize].extends.push(eid);
            return;
        }
        if matches!(self.types.get(target), Ty::Error) {
            return;
        }
        let eid = self.exts.len() as ExtId;
        scope.self_ty = Some(target);
        let mut methods = FxHashMap::default();
        for (mi, md) in x.methods.iter().enumerate() {
            let fid = self.new_fn(&md.name.name, m, Owner::Extend(eid), md.name.span);
            self.fns[fid as usize].decl = DeclRef::Method(m, item, mi);
            self.fns[fid as usize].owner_params = params.clone();
            self.fns[fid as usize].self_ty = target;
            self.fns[fid as usize].is_pub = x.is_pub || md.is_pub || is_prelude;
            self.fill_sig(fid, md, &mut scope, false);
            if md.is_native {
                self.fns[fid as usize].native_name = Some(format!("ext.{}", md.name.name));
            }
            methods.insert(md.name.name.clone(), fid);
        }
        self.exts.push(ExtendInfo {
            module: m,
            item,
            params,
            target,
            methods,
            is_pub: x.is_pub || is_prelude,
            is_class_methods: false,
        });
        self.mods[m as usize].extends.push(eid);
    }

    /// Phase D: per-class derived data (getters, constructors, vtables, mutating flags).
    pub fn finish_classes(&mut self) {
        let mut ids: Vec<ClassId> = self.classes.keys().copied().filter(|&c| c >= builtin::FIRST_USER).collect();
        ids.sort_unstable();
        // Parents before children.
        ids.sort_by_key(|&c| self.depth(c));
        for cid in ids {
            if self.classes[&cid].constructor.is_some() {
                continue;
            }
            let ci = self.classes[&cid].clone();
            let m = ci.module;
            let ptys: Vec<TyId> = ci.params.iter().map(|&p| self.param_ty(p)).collect();
            let self_ty = self.types.class(cid, ptys);
            // Getters for this class's own fields.
            for (fi, f) in ci.fields.iter().enumerate() {
                if f.decl_class != cid {
                    continue;
                }
                let g = self.new_fn(&f.name, m, Owner::Class(cid), f.span);
                let fnf = &mut self.fns[g as usize];
                fnf.owner_params = ci.params.clone();
                fnf.self_ty = self_ty;
                fnf.self_kind = SelfKind::Ref;
                fnf.ret = f.ty;
                fnf.is_pub = true;
                fnf.synth = Synth::Getter { field: fi };
                fnf.state = CheckState::Done;
                self.classes.get_mut(&cid).unwrap().getters.insert(f.name.clone(), g);
            }
            // Effective init: own or inherited.
            let init = ci.init.or_else(|| {
                let sup = ci.superclass?;
                match self.types.get(sup) {
                    Ty::Class(sc, _) => self.inherited_init(*sc),
                    _ => None,
                }
            });
            let ctor = self.new_fn("new", m, Owner::Class(cid), ci.span);
            {
                let params: Vec<PInfo> = match init {
                    Some(i) => {
                        // Inherited init params are expressed in the superclass's params; map them.
                        let ip = self.fns[i as usize].params.clone();
                        let owner_cls = match self.fns[i as usize].owner {
                            Owner::Class(c) => c,
                            _ => cid,
                        };
                        let view = self.upcast(self_ty, owner_cls).unwrap_or(self_ty);
                        let sub: FxHashMap<ParamId, TyId> = match self.types.get(view).clone() {
                            Ty::Class(c, a) => self.classes[&c].params.iter().copied().zip(a).collect(),
                            _ => FxHashMap::default(),
                        };
                        ip.into_iter().map(|p| PInfo { ty: self.types.subst(p.ty, &sub, Some(self_ty)), ..p }).collect()
                    }
                    None => ci
                        .fields
                        .iter()
                        .map(|f| PInfo { name: f.name.clone(), ty: f.ty, has_default: f.has_default, is_move: false, span: f.span })
                        .collect(),
                };
                let fnf = &mut self.fns[ctor as usize];
                fnf.owner_params = ci.params.clone();
                fnf.self_ty = self_ty;
                fnf.params = params;
                fnf.ret = self_ty;
                fnf.is_pub = true;
                fnf.synth = Synth::Constructor(cid);
                fnf.state = CheckState::Unchecked;
            }
            let cim = self.classes.get_mut(&cid).unwrap();
            cim.constructor = Some(ctor);
            if init.is_some() && cim.init.is_none() {
                cim.init = init;
            }
            // Vtable: inherit the parent's layout, override or append.
            let (mut vtable, mut vslots) = match ci.superclass.map(|s| self.types.get(s).clone()) {
                Some(Ty::Class(sc, _)) => (self.classes[&sc].vtable.clone(), self.classes[&sc].vslots.clone()),
                _ => (Vec::new(), FxHashMap::default()),
            };
            let mut own: Vec<(String, FnId)> = ci.methods.iter().map(|(k, &v)| (k.clone(), v)).collect();
            own.sort_by_key(|(_, f)| *f);
            for (name, fid) in own {
                if self.fns[fid as usize].self_kind == SelfKind::None || name == "init" {
                    continue;
                }
                match vslots.get(&name) {
                    Some(&slot) => {
                        let parent = vtable[slot as usize];
                        self.check_override(parent, fid);
                        vtable[slot as usize] = fid;
                    }
                    None => {
                        vslots.insert(name.clone(), vtable.len() as u32);
                        vtable.push(fid);
                    }
                }
            }
            let cim = self.classes.get_mut(&cid).unwrap();
            cim.vtable = vtable;
            cim.vslots = vslots;
        }
    }

    fn inherited_init(&self, c: ClassId) -> Option<FnId> {
        let ci = self.classes.get(&c)?;
        if let Some(i) = ci.init {
            return Some(i);
        }
        match ci.superclass.map(|s| self.types.get(s)) {
            Some(Ty::Class(sc, _)) => self.inherited_init(*sc),
            _ => None,
        }
    }

    fn depth(&self, c: ClassId) -> usize {
        let mut d = 0;
        let mut cur = self.classes[&c].superclass;
        while let Some(t) = cur {
            d += 1;
            if d > 64 {
                break;
            }
            cur = match self.types.get(t) {
                Ty::Class(sc, _) => self.classes.get(sc).and_then(|x| x.superclass),
                _ => None,
            };
        }
        d
    }

    fn check_override(&mut self, parent: FnId, child: FnId) {
        let (p, c) = (self.fns[parent as usize].clone(), self.fns[child as usize].clone());
        if p.params.len() != c.params.len() {
            self.emit(
                Diag::error(
                    "E0226",
                    format!(
                        "`{}` overrides a method with {} parameter(s) but takes {}",
                        c.name,
                        p.params.len(),
                        c.params.len()
                    ),
                    c.span,
                )
                .note_at("overridden method", p.span),
            );
        }
    }
}
