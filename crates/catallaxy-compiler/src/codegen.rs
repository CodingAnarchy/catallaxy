//! Bytecode emission from the checked AST.
//!
//! Registers are allocated stack-like: parameters first, then locals as they are declared, then
//! temporaries. Temporaries are released after each statement; block-scoped locals at block end.

use rustc_hash::FxHashMap;

use crate::ast::{self, Arg, BinOp, Block, Expr, ExprKind, Item, Pattern, PatternKind, StrPart, UnOp};
use crate::check::*;
use crate::diag::{SourceMap, Span};
use crate::types::*;
use catallaxy_vm::bytecode::*;

pub struct Codegen<'a> {
    ck: &'a Checker,
    sm: &'a SourceMap,
    pub prog: Program,
    /// Internal natives used by generated code.
    internal: FxHashMap<&'static str, u32>,
    pub errors: Vec<crate::diag::Diagnostic>,
}

struct LoopLabels {
    start: usize,
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// Where `break value` stores its value.
    result: Option<u8>,
    with_depth: usize,
    /// For `continue`: jump target computed later (for `for` loops the increment).
    continue_patch: bool,
}

struct FnState {
    fid: FnId,
    code: Vec<u32>,
    lines: Vec<(u32, u32)>,
    consts: Vec<Const>,
    const_map: FxHashMap<String, u16>,
    ics: Vec<IcProto>,
    next: usize,
    max: usize,
    regs: FxHashMap<LocalId, u8>,
    caps: FxHashMap<LocalId, u8>,
    loops: Vec<LoopLabels>,
    withs: Vec<u8>,
    last_line: u32,
    file: u32,
    is_closure: bool,
}

impl<'a> Codegen<'a> {
    pub fn new(ck: &'a Checker, sm: &'a SourceMap) -> Self {
        Codegen { ck, sm, prog: Program::default(), internal: FxHashMap::default(), errors: Vec::new() }
    }

    fn internal_native(&mut self, name: &'static str) -> u32 {
        if let Some(&i) = self.internal.get(name) {
            return i;
        }
        let i = self.prog.natives.len() as u32;
        self.prog.natives.push(name.to_string());
        self.internal.insert(name, i);
        i
    }

    /// Builds the whole program.
    pub fn generate(mut self) -> (Program, Vec<crate::diag::Diagnostic>) {
        let ck = self.ck;
        self.prog.files = self.sm.files.iter().map(|f| f.path.clone()).collect();
        self.prog.rt_types = ck.rt_types.clone();
        self.prog.ifaces = ck.rt_ifaces.clone();
        self.prog.funcs = vec![FuncProto::default(); ck.fns.len()];
        for fid in 0..ck.fns.len() as FnId {
            if matches!(ck.fns[fid as usize].decl, DeclRef::Closure(_)) {
                // Generated together with the enclosing function.
                continue;
            }
            let proto = self.gen_fn(fid);
            self.prog.funcs[fid as usize] = proto;
        }
        self.gen_classes();
        self.prog.globals = ck
            .globals
            .iter()
            .map(|g| GlobalProto { name: g.name.clone(), is_const: g.is_const })
            .collect();
        self.prog.entry = ck.mods.iter().map(|m| m.main_fn).collect();
        (self.prog, self.errors)
    }

    // ---- classes -----------------------------------------------------------------------------

    fn all_methods(&self, cid: ClassId) -> Vec<(String, u32)> {
        let ck = self.ck;
        let mut chain = Vec::new();
        let mut cur = Some(cid);
        while let Some(c) = cur {
            chain.push(c);
            cur = ck.classes[&c].superclass.and_then(|t| match ck.types.get(t) {
                Ty::Class(sc, _) => Some(*sc),
                _ => None,
            });
        }
        let mut out: FxHashMap<String, u32> = FxHashMap::default();
        for &c in chain.iter().rev() {
            let ci = &ck.classes[&c];
            for (n, &f) in &ci.getters {
                out.insert(n.clone(), f);
            }
            for (n, &f) in &ci.methods {
                if ck.fns[f as usize].self_kind != ast::SelfKind::None {
                    out.insert(n.clone(), f);
                }
            }
        }
        // Extension methods targeting this class (for dynamic dispatch from generic code).
        for ext in &ck.exts {
            if ext.is_class_methods {
                continue;
            }
            if let Ty::Class(tc, _) = ck.types.get(ext.target) {
                if chain.contains(tc) {
                    for (n, &f) in &ext.methods {
                        out.entry(n.clone()).or_insert(f);
                    }
                }
            }
        }
        let mut v: Vec<(String, u32)> = out.into_iter().collect();
        v.sort();
        v
    }

    fn gen_classes(&mut self) {
        let ck = self.ck;
        let mut protos: Vec<ClassProto> = Vec::new();
        for (&cid, ci) in &ck.classes {
            if ci.is_builtin {
                continue;
            }
            let superclass = ci.superclass.and_then(|t| match ck.types.get(t) {
                Ty::Class(sc, _) => Some(*sc),
                _ => None,
            });
            protos.push(ClassProto {
                id: cid,
                name: ci.name.clone(),
                fields: ci.fields.iter().map(|f| f.name.clone()).collect(),
                superclass,
                methods: self.all_methods(cid),
                vtable: ci.vtable.clone(),
                capability: match ci.modifier {
                    ast::ClassModifier::Plain => ClassCap::Owned,
                    ast::ClassModifier::Frozen => ClassCap::Frozen,
                    ast::ClassModifier::Shared => ClassCap::Shared,
                },
                variant: None,
            });
        }
        for en in &ck.enums {
            let mut methods: Vec<(String, u32)> = en
                .methods
                .iter()
                .filter(|(_, &f)| ck.fns[f as usize].self_kind != ast::SelfKind::None)
                .map(|(n, &f)| (n.clone(), f))
                .collect();
            for ext in &ck.exts {
                if let Ty::Enum(e, _) = ck.types.get(ext.target) {
                    if *e == en.id && !ext.is_class_methods {
                        for (n, &f) in &ext.methods {
                            if !methods.iter().any(|(m, _)| m == n) {
                                methods.push((n.clone(), f));
                            }
                        }
                    }
                }
            }
            methods.sort();
            for (tag, v) in en.variants.iter().enumerate() {
                let fields = v
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| n.clone().unwrap_or_else(|| format!("_{i}")))
                    .collect();
                protos.push(ClassProto {
                    id: v.class_id,
                    name: v.name.clone(),
                    fields,
                    superclass: None,
                    methods: methods.clone(),
                    vtable: Vec::new(),
                    capability: ClassCap::Owned,
                    variant: Some((en.id, tag as u32)),
                });
            }
        }
        protos.sort_by_key(|p| p.id);
        self.prog.classes = protos;
        // Builtin classes: prelude class methods plus extensions on them.
        let mut bm = Vec::new();
        let mut ids: Vec<ClassId> = ck.classes.keys().copied().filter(|&c| c < builtin::FIRST_USER).collect();
        ids.sort_unstable();
        for cid in ids {
            let ci = &ck.classes[&cid];
            let mut names: Vec<(&String, &u32)> = ci.methods.iter().collect();
            names.sort();
            for (n, &f) in names {
                if ck.fns[f as usize].self_kind != ast::SelfKind::None {
                    bm.push((cid, n.clone(), f));
                }
            }
            for ext in &ck.exts {
                if ext.is_class_methods {
                    continue;
                }
                if let Ty::Class(tc, _) = ck.types.get(ext.target) {
                    if *tc == cid {
                        for (n, &f) in &ext.methods {
                            if !ci.methods.contains_key(n) {
                                bm.push((cid, n.clone(), f));
                            }
                        }
                    }
                }
            }
        }
        self.prog.builtin_methods = bm;
    }

    // ---- functions ---------------------------------------------------------------------------

    fn new_state(&self, fid: FnId) -> FnState {
        let f = &self.ck.fns[fid as usize];
        let file = if (f.span.file as usize) < self.sm.files.len() && f.span != Span::default() {
            f.span.file
        } else {
            self.ck.mods[f.module as usize].file
        };
        FnState {
            fid,
            code: Vec::new(),
            lines: Vec::new(),
            consts: Vec::new(),
            const_map: FxHashMap::default(),
            ics: Vec::new(),
            next: 0,
            max: 0,
            regs: FxHashMap::default(),
            caps: FxHashMap::default(),
            loops: Vec::new(),
            withs: Vec::new(),
            last_line: 0,
            file,
            is_closure: false,
        }
    }

    fn finish(&self, st: FnState, name: String, nparams: usize, captures: Vec<CapSrc>) -> FuncProto {
        FuncProto {
            name,
            file: st.file,
            nparams: nparams as u8,
            nregs: st.max.max(nparams).min(255) as u8,
            code: st.code,
            lines: st.lines,
            consts: st.consts,
            ics: st.ics,
            captures,
            native: None,
        }
    }

    fn display_name(&self, fid: FnId) -> String {
        let f = &self.ck.fns[fid as usize];
        match f.owner {
            Owner::Class(c) => format!("{}.{}", self.ck.classes[&c].name, f.name),
            Owner::Enum(e) => format!("{}.{}", self.ck.enums[e as usize].name, f.name),
            Owner::Iface(i) => format!("{}.{}", self.ck.ifaces[i as usize].name, f.name),
            Owner::Extend(_) => f.name.clone(),
            Owner::None => match f.synth {
                Synth::Main(m) => format!("<main {}>", self.ck.mods[m as usize].name),
                _ => f.name.clone(),
            },
        }
    }

    fn gen_fn(&mut self, fid: FnId) -> FuncProto {
        let ck = self.ck;
        let f = &ck.fns[fid as usize];
        let name = self.display_name(fid);
        if f.is_native {
            let native = f.native_name.clone().unwrap_or_else(|| f.name.clone());
            let nparams = f.params.len() + (f.self_kind != ast::SelfKind::None) as usize;
            return FuncProto { name, nparams: nparams as u8, native: Some(native), file: 0, ..Default::default() };
        }
        match &f.synth {
            Synth::Getter { field } => {
                let mut st = self.new_state(fid);
                st.next = 1;
                st.max = 1;
                self.emit(&mut st, encode(Op::GetField, 0, 0, *field as u8));
                self.emit(&mut st, encode(Op::Return, 0, 0, 0));
                return self.finish(st, name, 1, vec![]);
            }
            Synth::Constructor(cid) => return self.gen_constructor(fid, *cid, name),
            Synth::Main(m) => return self.gen_main(fid, *m, name),
            Synth::None => {}
        }
        match f.decl {
            DeclRef::Closure(_) => {
                // Closures and for-bodies are generated with their enclosing function.
                FuncProto { name, ..Default::default() }
            }
            _ => match ck.fn_decl(fid) {
                Some(d) if d.body.is_some() => self.gen_decl(fid, &d, name),
                _ => {
                    // Abstract interface method: panics if ever reached.
                    let mut st = self.new_state(fid);
                    let nparams = f.params.len() + 1;
                    st.next = nparams;
                    st.max = nparams;
                    let r = self.alloc(&mut st);
                    let k = self.konst(&mut st, Const::Str(format!("abstract method `{}` called", f.name)));
                    self.emit(&mut st, encode_bx(Op::LoadK, r, k));
                    self.emit(&mut st, encode(Op::Panic, r, 0, 0));
                    self.finish(st, name, nparams, vec![])
                }
            },
        }
    }

    fn gen_decl(&mut self, fid: FnId, d: &ast::FnDecl, name: String) -> FuncProto {
        let ck = self.ck;
        let f = &ck.fns[fid as usize];
        let mut st = self.new_state(fid);
        let locals = ck.fn_locals.get(&fid).cloned().unwrap_or_default();
        // Parameters: self first, then declared parameters, in declaration order.
        let params: Vec<LocalId> = locals.iter().copied().filter(|&l| ck.locals[l as usize].is_param).collect();
        let nparams = params.len();
        for (i, &l) in params.iter().enumerate() {
            st.regs.insert(l, i as u8);
        }
        st.next = nparams;
        st.max = nparams;
        // Defaults.
        let off = (f.self_kind != ast::SelfKind::None) as usize;
        for (i, p) in d.params.iter().enumerate() {
            if let Some(def) = &p.default {
                let r = (i + off) as u8;
                let j = self.emit_jump(&mut st, Op::JmpIfNotEmpty, r);
                self.expr_to(&mut st, def, r);
                self.patch(&mut st, j);
            }
        }
        self.prologue(&mut st, &params);
        let body = d.body.as_ref().unwrap();
        let r = self.alloc(&mut st);
        self.block_to(&mut st, body, r);
        if self.has_coercion_to_float(f.ret, body) {
            self.emit(&mut st, encode(Op::IToF, r, r, 0));
        }
        self.emit(&mut st, encode(Op::Return, r, 0, 0));
        self.finish(st, name, nparams, vec![])
    }

    /// The last expression of an Int-producing body returned as Float.
    fn has_coercion_to_float(&self, ret: TyId, body: &Block) -> bool {
        let ck = self.ck;
        let ret = ck.types.shallow_resolve(ret);
        if ret != ck.types.float {
            return false;
        }
        match body.stmts.last() {
            Some(e) => ck.types.shallow_resolve(ck.ty_of(e.id)) == ck.types.int && !ck.coerce.contains_key(&e.id),
            None => false,
        }
    }

    /// Cells for captured-and-reassigned parameters, and the frame id for non-local exits.
    fn prologue(&mut self, st: &mut FnState, params: &[LocalId]) {
        for &l in params {
            if self.ck.locals[l as usize].is_cell() {
                let r = st.regs[&l];
                self.emit(st, encode(Op::NewCell, r, r, 0));
            }
        }
        if let Some(&fl) = self.ck.frame_locals.get(&st.fid) {
            let r = self.alloc(st);
            st.regs.insert(fl, r);
            self.emit(st, encode(Op::FrameId, r, 0, 0));
        }
    }

    fn gen_main(&mut self, fid: FnId, m: ModId, name: String) -> FuncProto {
        let ck = self.ck;
        let mut st = self.new_state(fid);
        self.prologue(&mut st, &[]);
        let items = ck.mods[m as usize].ast.items.clone();
        for item in &items {
            let mark = st.next;
            match item {
                Item::Stmt(e) => {
                    self.stmt(&mut st, e);
                }
                Item::Const(c) => {
                    if let Some(ScopeItem::Global(g)) = ck.mods[m as usize].items.get(&c.name.name) {
                        self.init_global(&mut st, *g, &c.value, true);
                    }
                }
                Item::Var(v) => {
                    if let Some(ScopeItem::Global(g)) = ck.mods[m as usize].items.get(&v.name.name) {
                        self.init_global(&mut st, *g, &v.value, false);
                    }
                }
                Item::Class(c) => {
                    if let Some(ScopeItem::Class(cid)) = ck.mods[m as usize].items.get(&c.name.name) {
                        for k in &c.consts {
                            if let Some(&g) = ck.classes[cid].consts.get(&k.name.name) {
                                self.init_global(&mut st, g, &k.value, true);
                            }
                        }
                    }
                }
                _ => {}
            }
            self.release_to(&mut st, mark);
        }
        self.emit(&mut st, encode(Op::ReturnNil, 0, 0, 0));
        self.finish(st, name, 0, vec![])
    }

    fn init_global(&mut self, st: &mut FnState, g: GlobalId, value: &Expr, is_const: bool) {
        let r = self.alloc(st);
        self.line(st, value.span);
        self.expr_to(st, value, r);
        if is_const {
            self.emit(st, encode(Op::Freeze, r, r, 0));
        }
        self.emit(st, encode(Op::SetGlobal, r, 0, 0));
        self.emit(st, g);
    }

    fn gen_constructor(&mut self, fid: FnId, cid: ClassId, name: String) -> FuncProto {
        let ck = self.ck;
        let ci = &ck.classes[&cid];
        let f = &ck.fns[fid as usize];
        let mut st = self.new_state(fid);
        let locals = ck.fn_locals.get(&fid).cloned().unwrap_or_default();
        // Locals: [self, params...] were created in check_constructor; registers: params first.
        let nparams = f.params.len();
        let self_local = locals.first().copied();
        let param_locals: Vec<LocalId> = locals.iter().skip(1).take(nparams).copied().collect();
        for (i, &l) in param_locals.iter().enumerate() {
            st.regs.insert(l, i as u8);
        }
        st.next = nparams;
        st.max = nparams;
        let selfr = self.alloc(&mut st);
        if let Some(sl) = self_local {
            st.regs.insert(sl, selfr);
        }
        self.emit(&mut st, encode(Op::New, selfr, 0, 0));
        self.emit(&mut st, cid);
        // Field defaults (the class's own AST, possibly from a superclass declaration).
        let has_init = ci.init.is_some();
        for (fi, field) in ci.fields.iter().enumerate() {
            if !field.has_default {
                continue;
            }
            let dc = &ck.classes[&field.decl_class];
            let Item::Class(decl) = &ck.mods[dc.module as usize].ast.items[dc.item.unwrap()] else { continue };
            let Some(def) = &decl.fields[field.decl_index].default else { continue };
            if has_init {
                let t = self.alloc(&mut st);
                self.expr_to(&mut st, def, t);
                self.emit(&mut st, encode(Op::SetField, selfr, fi as u8, t));
                self.release_to(&mut st, t as usize);
            } else {
                // The parameter for this field may be missing: apply the default.
                let p = fi as u8;
                let j = self.emit_jump(&mut st, Op::JmpIfNotEmpty, p);
                self.expr_to(&mut st, def, p);
                self.patch(&mut st, j);
            }
        }
        match ci.init {
            Some(init) => {
                let base = self.alloc_n(&mut st, nparams + 1);
                self.emit(&mut st, encode(Op::Move, base, selfr, 0));
                for i in 0..nparams {
                    self.emit(&mut st, encode(Op::Move, base + 1 + i as u8, i as u8, 0));
                }
                self.emit(&mut st, encode(Op::Call, base, (nparams + 1) as u8, 0));
                self.emit(&mut st, init);
            }
            None => {
                for i in 0..ci.fields.len().min(nparams) {
                    self.emit(&mut st, encode(Op::SetField, selfr, i as u8, i as u8));
                }
            }
        }
        if ci.modifier == ast::ClassModifier::Frozen {
            self.emit(&mut st, encode(Op::Freeze, selfr, selfr, 0));
        }
        self.emit(&mut st, encode(Op::Return, selfr, 0, 0));
        self.finish(st, name, nparams, vec![])
    }

    /// Generates a closure (or for-body block) function and returns the code creating it.
    fn gen_closure_fn(&mut self, outer: &mut FnState, info: &ClosureInfo, body: &Block, pattern: Option<&Pattern>) -> FnId {
        let ck = self.ck;
        let fid = info.fn_id;
        let mut st = self.new_state(fid);
        st.is_closure = true;
        let nparams = if info.destructure { 1 } else { info.params.len() };
        st.next = nparams;
        st.max = nparams;
        // Captures by index.
        let mut srcs = Vec::new();
        for (i, &l) in info.captures.iter().enumerate() {
            st.caps.insert(l, i as u8);
            let src = match outer.regs.get(&l) {
                Some(&r) => CapSrc::Reg(r),
                None => match outer.caps.get(&l) {
                    Some(&c) => CapSrc::Cap(c),
                    None => {
                        // A local declared later in the enclosing function: give it its
                        // register now so the closure captures the right slot.
                        let r = self.local_reg(outer, l);
                        self.emit(outer, encode(Op::LoadNil, r, 0, 0));
                        if self.ck.locals[l as usize].is_cell() {
                            self.emit(outer, encode(Op::NewCell, r, r, 0));
                        }
                        CapSrc::Reg(r)
                    }
                },
            };
            srcs.push(src);
        }
        if info.destructure {
            for (i, &l) in info.params.iter().enumerate() {
                let r = self.alloc(&mut st);
                st.regs.insert(l, r);
                self.emit(&mut st, encode(Op::TupleGet, r, 0, i as u8));
            }
        } else {
            for (i, &l) in info.params.iter().enumerate() {
                st.regs.insert(l, i as u8);
            }
        }
        let params = info.params.clone();
        self.prologue(&mut st, &params);
        if let Some(p) = pattern {
            // A for-body block: bind the loop pattern from the item parameter.
            let mut fails = Vec::new();
            self.pattern(&mut st, p, 0, &mut fails);
        }
        let r = self.alloc(&mut st);
        self.block_to(&mut st, body, r);
        if info.is_for_body {
            self.emit(&mut st, encode(Op::ReturnNil, 0, 0, 0));
        } else {
            let ret = ck.fns[fid as usize].ret;
            if self.has_coercion_to_float(ret, body) {
                self.emit(&mut st, encode(Op::IToF, r, r, 0));
            }
            self.emit(&mut st, encode(Op::Return, r, 0, 0));
        }
        let name = ck.fns[fid as usize].name.clone();
        let proto = self.finish(st, name, nparams, srcs);
        self.prog.funcs[fid as usize] = proto;
        fid
    }

    // ---- emission helpers --------------------------------------------------------------------

    fn emit(&self, st: &mut FnState, w: u32) -> usize {
        st.code.push(w);
        st.code.len() - 1
    }

    fn line(&self, st: &mut FnState, span: Span) {
        if span.file as usize >= self.sm.files.len() {
            return;
        }
        let (line, _) = self.sm.file(span.file).line_col(span.start);
        if line != st.last_line {
            st.lines.push((st.code.len() as u32, line));
            st.last_line = line;
        }
    }

    fn alloc(&mut self, st: &mut FnState) -> u8 {
        let r = st.next;
        st.next += 1;
        if st.next > st.max {
            st.max = st.next;
        }
        if r >= 250 {
            let name = self.ck.fns[st.fid as usize].name.clone();
            if !self.errors.iter().any(|d| d.code == "E0900") {
                self.errors.push(crate::diag::Diagnostic::error(
                    "E0900",
                    format!("function `{name}` needs too many registers; split it into smaller functions"),
                    self.ck.fns[st.fid as usize].span,
                ));
            }
            return 249;
        }
        r as u8
    }

    fn alloc_n(&mut self, st: &mut FnState, n: usize) -> u8 {
        let base = st.next;
        for _ in 0..n.max(1) {
            self.alloc(st);
        }
        if n == 0 {
            st.next -= 1;
        }
        base.min(249) as u8
    }

    fn release_to(&self, st: &mut FnState, mark: usize) {
        if mark < st.next {
            // Never release registers holding locals declared since `mark`.
            let max_local = st.regs.values().map(|&r| r as usize + 1).filter(|&r| r > mark).max();
            st.next = max_local.unwrap_or(mark).max(mark);
        }
    }

    fn konst(&self, st: &mut FnState, c: Const) -> u16 {
        let key = format!("{c:?}");
        if let Some(&i) = st.const_map.get(&key) {
            return i;
        }
        let i = st.consts.len() as u16;
        st.consts.push(c);
        st.const_map.insert(key, i);
        i
    }

    fn emit_jump(&self, st: &mut FnState, op: Op, a: u8) -> usize {
        self.emit(st, encode_sbx(op, a, 0))
    }

    /// Points a forward jump at the current position.
    fn patch(&self, st: &mut FnState, at: usize) {
        let off = st.code.len() as isize - at as isize - 1;
        let w = st.code[at];
        let op = Op::from_u8(op_of(w));
        st.code[at] = if op == Op::Jmp {
            encode_sax(Op::Jmp, off as i32)
        } else {
            assert!(off <= i16::MAX as isize, "jump too far");
            encode_sbx(op, a_of(w) as u8, off as i16)
        };
    }

    fn jmp(&self, st: &mut FnState) -> usize {
        self.emit(st, encode_sax(Op::Jmp, 0))
    }

    fn jmp_back(&self, st: &mut FnState, target: usize) {
        let off = target as isize - st.code.len() as isize - 1;
        self.emit(st, encode_sax(Op::Jmp, off as i32));
    }

    fn ic(&self, st: &mut FnState, name: &str, fallback: Option<FnId>) -> u32 {
        st.ics.push(IcProto { name: name.to_string(), fallback });
        st.ics.len() as u32 - 1
    }

    // ---- locals ------------------------------------------------------------------------------

    fn local_reg(&mut self, st: &mut FnState, l: LocalId) -> u8 {
        if let Some(&r) = st.regs.get(&l) {
            return r;
        }
        let r = self.alloc(st);
        st.regs.insert(l, r);
        r
    }

    /// Stores `src` into local `l` (declaring it if needed).
    fn store_local(&mut self, st: &mut FnState, l: LocalId, src: u8, declare: bool) {
        let cell = self.ck.locals[l as usize].is_cell();
        if let Some(&c) = st.caps.get(&l) {
            // Assigning a captured variable: it lives in a cell.
            let t = self.alloc(st);
            self.emit(st, encode(Op::GetCap, t, c, 0));
            self.emit(st, encode(Op::SetCell, t, src, 0));
            return;
        }
        let first = !st.regs.contains_key(&l);
        let r = self.local_reg(st, l);
        if cell {
            if first || declare {
                self.emit(st, encode(Op::NewCell, r, src, 0));
            } else {
                self.emit(st, encode(Op::SetCell, r, src, 0));
            }
        } else if r != src {
            self.emit(st, encode(Op::Move, r, src, 0));
        }
    }

    /// Loads local `l` into `dst`.
    fn load_local(&mut self, st: &mut FnState, l: LocalId, dst: u8) {
        let cell = self.ck.locals[l as usize].is_cell();
        if let Some(&c) = st.caps.get(&l) {
            self.emit(st, encode(Op::GetCap, dst, c, 0));
            if cell {
                self.emit(st, encode(Op::GetCell, dst, dst, 0));
            }
            return;
        }
        match st.regs.get(&l) {
            Some(&r) => {
                if cell {
                    self.emit(st, encode(Op::GetCell, dst, r, 0));
                } else if r != dst {
                    self.emit(st, encode(Op::Move, dst, r, 0));
                }
            }
            None => {
                // Declared but never assigned on this path (e.g. a pattern local): nil.
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
        }
    }

    /// A register already holding `e`'s value (plain locals), or `e` evaluated into a temp.
    fn expr_reg(&mut self, st: &mut FnState, e: &Expr) -> u8 {
        if let ExprKind::Ident(_) | ExprKind::SelfValue = e.kind {
            if let Some(NameRes::Local(l)) = self.ck.names.get(&e.id) {
                if !self.ck.locals[*l as usize].is_cell() && !st.caps.contains_key(l) && !self.ck.coerce.contains_key(&e.id) {
                    if let Some(&r) = st.regs.get(l) {
                        return r;
                    }
                }
            }
        }
        let r = self.alloc(st);
        self.expr_to(st, e, r);
        r
    }

    // ---- statements and blocks ---------------------------------------------------------------

    fn stmt(&mut self, st: &mut FnState, e: &Expr) {
        let mark = st.next;
        self.line(st, e.span);
        match &e.kind {
            ExprKind::Assign { .. } | ExprKind::Let { .. } => self.assign_or_let(st, e),
            _ => {
                let r = self.alloc(st);
                self.expr_to(st, e, r);
            }
        }
        self.release_to(st, mark);
    }

    fn block_to(&mut self, st: &mut FnState, b: &Block, dst: u8) {
        let mark = st.next;
        let saved: Vec<LocalId> = st.regs.keys().copied().collect();
        let n = b.stmts.len();
        if n == 0 {
            self.emit(st, encode(Op::LoadNil, dst, 0, 0));
        }
        for (i, s) in b.stmts.iter().enumerate() {
            if i + 1 == n {
                self.line(st, s.span);
                match &s.kind {
                    ExprKind::Assign { .. } | ExprKind::Let { .. } | ExprKind::While { .. } | ExprKind::For { .. } => {
                        self.stmt(st, s);
                        self.emit(st, encode(Op::LoadNil, dst, 0, 0));
                    }
                    _ => self.expr_to(st, s, dst),
                }
            } else {
                self.stmt(st, s);
            }
        }
        // Locals declared in this block die here.
        st.regs.retain(|l, _| saved.contains(l));
        self.release_to(st, mark);
    }

    fn assign_or_let(&mut self, st: &mut FnState, e: &Expr) {
        let ck = self.ck;
        match &e.kind {
            ExprKind::Let { pattern, value } => {
                let r = self.expr_reg(st, value);
                let mut fails = Vec::new();
                self.pattern(st, pattern, r, &mut fails);
            }
            ExprKind::Assign { target, op, value, .. } => {
                let Some(res) = ck.assigns.get(&e.id).cloned() else { return };
                match res {
                    AssignRes::Local(l) => {
                        let declare = ck.decl_locals.get(&e.id) == Some(&l);
                        if declare && !ck.locals[l as usize].is_cell() && !st.caps.contains_key(&l) && op.is_none() {
                            let r = self.local_reg(st, l);
                            self.expr_to(st, value, r);
                            return;
                        }
                        if declare && ck.locals[l as usize].is_cell() && !st.caps.contains_key(&l) && !st.regs.contains_key(&l) {
                            // Create the cell first so a closure in the value can capture it.
                            let r = self.local_reg(st, l);
                            self.emit(st, encode(Op::LoadNil, r, 0, 0));
                            self.emit(st, encode(Op::NewCell, r, r, 0));
                            let t = self.alloc(st);
                            self.expr_to(st, value, t);
                            self.emit(st, encode(Op::SetCell, r, t, 0));
                            return;
                        }
                        let t = self.alloc(st);
                        match op {
                            None => self.expr_to(st, value, t),
                            Some(o) => self.compound(st, e, *o, target, value, t),
                        }
                        let declare = declare && !st.regs.contains_key(&l);
                        self.store_local(st, l, t, declare);
                    }
                    AssignRes::Global(g) => {
                        let t = self.alloc(st);
                        match op {
                            None => self.expr_to(st, value, t),
                            Some(o) => self.compound(st, e, *o, target, value, t),
                        }
                        self.emit(st, encode(Op::SetGlobal, t, 0, 0));
                        self.emit(st, g);
                    }
                    AssignRes::Field(idx) => {
                        let ExprKind::Member { recv, .. } = &target.kind else { return };
                        let o = self.expr_reg(st, recv);
                        let t = self.alloc(st);
                        match op {
                            None => self.expr_to(st, value, t),
                            Some(op) => {
                                // Read the field into t, then combine.
                                self.emit(st, encode(Op::GetField, t, o, idx as u8));
                                let v = self.expr_reg(st, value);
                                self.combine(st, e, *op, t, t, v, value);
                            }
                        }
                        self.emit(st, encode(Op::SetField, o, idx as u8, t));
                    }
                    AssignRes::Index(dispatch) => {
                        let ExprKind::Index { recv, args } = &target.kind else { return };
                        let base = self.alloc_n(st, 3);
                        self.expr_to(st, recv, base);
                        self.expr_to(st, &args[0], base + 1);
                        match op {
                            None => self.expr_to(st, value, base + 2),
                            Some(op) => {
                                self.emit(st, encode(Op::Index, base + 2, base, base + 1));
                                let v = self.expr_reg(st, value);
                                self.combine(st, e, *op, base + 2, base + 2, v, value);
                            }
                        }
                        self.call_dispatch(st, &dispatch, base, 3);
                    }
                    AssignRes::IndexGeneric => {
                        match &target.kind {
                            ExprKind::Index { recv, args } => {
                                let o = self.expr_reg(st, recv);
                                let i = self.expr_reg(st, &args[0]);
                                let t = self.alloc(st);
                                match op {
                                    None => self.expr_to(st, value, t),
                                    Some(op) => {
                                        self.emit(st, encode(Op::Index, t, o, i));
                                        let v = self.expr_reg(st, value);
                                        self.combine(st, e, *op, t, t, v, value);
                                    }
                                }
                                self.emit(st, encode(Op::SetIndex, o, i, t));
                            }
                            ExprKind::Member { recv, name, .. } => {
                                // Dyn member assignment: d.name = v (dict key).
                                let o = self.expr_reg(st, recv);
                                let k = self.alloc(st);
                                let kc = self.konst(st, Const::Str(name.name.clone()));
                                self.emit(st, encode_bx(Op::LoadK, k, kc));
                                let t = self.alloc(st);
                                self.expr_to(st, value, t);
                                self.emit(st, encode(Op::SetIndex, o, k, t));
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    /// `target op= value` where the target's current value is read from `target` itself.
    fn compound(&mut self, st: &mut FnState, e: &Expr, op: BinOp, target: &Expr, value: &Expr, dst: u8) {
        match op {
            BinOp::Or | BinOp::And | BinOp::Coalesce => {
                self.expr_to(st, target, dst);
                let j = match op {
                    BinOp::Or => self.emit_jump(st, Op::JmpIf, dst),
                    BinOp::And => self.emit_jump(st, Op::JmpIfNot, dst),
                    _ => self.emit_jump(st, Op::JmpIfNotNil, dst),
                };
                self.expr_to(st, value, dst);
                self.patch(st, j);
            }
            _ => {
                self.expr_to(st, target, dst);
                let v = self.expr_reg(st, value);
                self.combine(st, e, op, dst, dst, v, value);
            }
        }
    }

    /// dst = l op r per the recorded operator resolution of node `e`.
    #[allow(clippy::too_many_arguments)]
    fn combine(&mut self, st: &mut FnState, e: &Expr, op: BinOp, dst: u8, l: u8, r: u8, rexpr: &Expr) {
        let _ = rexpr;
        match self.ck.bins.get(&e.id).cloned() {
            Some(BinRes::Arith(k)) => {
                if k == ArithKind::Float {
                    // Coercions of either side were recorded on the operand nodes.
                    self.coerce_reg_if(st, rexpr, r);
                }
                let opc = arith_op(op, k);
                self.emit(st, encode(opc, dst, l, r));
            }
            Some(BinRes::Concat) => {
                let base = self.alloc_n(st, 2);
                self.emit(st, encode(Op::Move, base, l, 0));
                self.emit(st, encode(Op::Move, base + 1, r, 0));
                self.emit(st, encode(Op::Concat, dst, base, 2));
            }
            Some(BinRes::Bitwise) => {
                self.emit(st, encode(bit_op(op), dst, l, r));
            }
            Some(BinRes::Method(dispatch, _)) => {
                let base = self.alloc_n(st, 2);
                self.emit(st, encode(Op::Move, base, l, 0));
                self.emit(st, encode(Op::Move, base + 1, r, 0));
                self.call_dispatch(st, &dispatch, base, 2);
                self.emit(st, encode(Op::Move, dst, base, 0));
            }
            _ => {
                let opc = arith_op(op, ArithKind::Generic);
                self.emit(st, encode(opc, dst, l, r));
            }
        }
    }

    fn coerce_reg_if(&mut self, st: &mut FnState, e: &Expr, r: u8) {
        let _ = (st, e, r);
    }

    // ---- expressions -------------------------------------------------------------------------

    /// Evaluates `e` into register `dst`, applying any recorded coercion.
    fn expr_to(&mut self, st: &mut FnState, e: &Expr, dst: u8) {
        self.expr_inner(st, e, dst);
        if self.ck.coerce.get(&e.id) == Some(&Coercion::IntToFloat) {
            self.emit(st, encode(Op::IToF, dst, dst, 0));
        }
    }

    fn expr_inner(&mut self, st: &mut FnState, e: &Expr, dst: u8) {
        let ck = self.ck;
        match &e.kind {
            ExprKind::Int(s) => self.load_int(st, s, dst),
            ExprKind::Float(f) => {
                let k = self.konst(st, Const::Float(*f));
                self.emit(st, encode_bx(Op::LoadK, dst, k));
            }
            ExprKind::Bool(b) => {
                self.emit(st, encode(if *b { Op::LoadTrue } else { Op::LoadFalse }, dst, 0, 0));
            }
            ExprKind::Nil => {
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
            ExprKind::Str(parts) => self.string(st, parts, dst),
            ExprKind::Ident(_) | ExprKind::SelfValue => self.ident(st, e, dst),
            ExprKind::List(items) => {
                if items.len() <= 200 {
                    let mark = st.next;
                    let base = self.alloc_n(st, items.len());
                    for (i, x) in items.iter().enumerate() {
                        self.expr_to(st, x, base + i as u8);
                    }
                    self.emit(st, encode(Op::NewList, dst, base, items.len() as u8));
                    self.release_to(st, mark);
                } else {
                    self.emit(st, encode(Op::NewList, dst, 0, 0));
                    for x in items {
                        let mark = st.next;
                        let t = self.alloc(st);
                        self.expr_to(st, x, t);
                        self.emit(st, encode(Op::ListPush, dst, t, 0));
                        self.release_to(st, mark);
                    }
                }
            }
            ExprKind::Dict(entries) => {
                if entries.len() <= 100 {
                    let mark = st.next;
                    let base = self.alloc_n(st, entries.len() * 2);
                    for (i, (k, v)) in entries.iter().enumerate() {
                        self.expr_to(st, k, base + 2 * i as u8);
                        self.expr_to(st, v, base + 2 * i as u8 + 1);
                    }
                    self.emit(st, encode(Op::NewDict, dst, base, entries.len() as u8));
                    self.release_to(st, mark);
                } else {
                    self.emit(st, encode(Op::NewDict, dst, 0, 0));
                    for (k, v) in entries {
                        let mark = st.next;
                        let kr = self.alloc(st);
                        let vr = self.alloc(st);
                        self.expr_to(st, k, kr);
                        self.expr_to(st, v, vr);
                        self.emit(st, encode(Op::SetIndex, dst, kr, vr));
                        self.release_to(st, mark);
                    }
                }
            }
            ExprKind::Tuple(items) => {
                let mark = st.next;
                let base = self.alloc_n(st, items.len());
                for (i, x) in items.iter().enumerate() {
                    self.expr_to(st, x, base + i as u8);
                }
                self.emit(st, encode(Op::NewTuple, dst, base, items.len() as u8));
                self.release_to(st, mark);
            }
            ExprKind::Unary(op, x) => {
                let mark = st.next;
                let r = self.expr_reg(st, x);
                let opc = match (op, ck.bins.get(&e.id)) {
                    (UnOp::Not, _) => Op::Not,
                    (UnOp::Neg, Some(BinRes::Arith(ArithKind::Int))) => Op::NegI,
                    (UnOp::Neg, Some(BinRes::Arith(ArithKind::Float))) => Op::NegF,
                    (UnOp::Neg, _) => Op::Neg,
                    (UnOp::BitNot, _) => Op::BNot,
                };
                self.emit(st, encode(opc, dst, r, 0));
                self.release_to(st, mark);
            }
            ExprKind::Binary(op, l, r) => self.binary(st, e, *op, l, r, dst),
            ExprKind::Range { lo, hi, exclusive } => {
                let mark = st.next;
                let a = self.expr_reg(st, lo);
                let b = self.expr_reg(st, hi);
                self.emit(st, encode(if *exclusive { Op::NewRangeEx } else { Op::NewRange }, dst, a, b));
                self.release_to(st, mark);
            }
            ExprKind::Call { callee, args, block } => self.call(st, e, Some(callee), None, args, block.as_deref(), dst),
            ExprKind::Member { recv, args, block, .. } => {
                self.call(st, e, None, Some(recv), args, block.as_deref(), dst)
            }
            ExprKind::Index { recv, args } => {
                let mark = st.next;
                match ck.calls.get(&e.id) {
                    Some(CallRes::TupleField(i)) => {
                        let r = self.expr_reg(st, recv);
                        self.emit(st, encode(Op::TupleGet, dst, r, *i as u8));
                    }
                    Some(CallRes::ListGet) => {
                        let r = self.expr_reg(st, recv);
                        let i = self.expr_reg(st, &args[0]);
                        self.emit(st, encode(Op::ListGet, dst, r, i));
                    }
                    _ => {
                        let r = self.expr_reg(st, recv);
                        let i = self.expr_reg(st, &args[0]);
                        self.emit(st, encode(Op::Index, dst, r, i));
                    }
                }
                self.release_to(st, mark);
            }
            ExprKind::Closure { body, .. } => {
                if let Some(info) = ck.closures.get(&e.id).cloned() {
                    let fid = self.gen_closure_fn(st, &info, body, None);
                    self.emit(st, encode(Op::Closure, dst, 0, 0));
                    self.emit(st, fid);
                }
            }
            ExprKind::If { cond, then, else_ } => {
                let fails = self.cond_jump(st, cond);
                self.block_to(st, then, dst);
                match else_ {
                    Some(x) => {
                        let end = self.jmp(st);
                        for f in fails {
                            self.patch(st, f);
                        }
                        self.expr_to(st, x, dst);
                        self.patch(st, end);
                    }
                    None => {
                        let end = self.jmp(st);
                        for f in fails {
                            self.patch(st, f);
                        }
                        self.emit(st, encode(Op::LoadNil, dst, 0, 0));
                        self.patch(st, end);
                    }
                }
            }
            ExprKind::IfLet { pattern, value, then, else_ } => {
                let mark = st.next;
                let v = self.alloc(st);
                self.expr_to(st, value, v);
                let mut fails = Vec::new();
                if let PatternKind::Bind(_) = pattern.kind {
                    fails.push(self.emit_jump(st, Op::JmpIfNil, v));
                }
                self.pattern(st, pattern, v, &mut fails);
                self.block_to(st, then, dst);
                let end = self.jmp(st);
                for f in fails {
                    self.patch(st, f);
                }
                match else_ {
                    Some(x) => self.expr_to(st, x, dst),
                    None => {
                        self.emit(st, encode(Op::LoadNil, dst, 0, 0));
                    }
                }
                self.patch(st, end);
                self.release_to(st, mark);
            }
            ExprKind::While { cond, body } => {
                let start = st.code.len();
                let fails = self.cond_jump(st, cond);
                st.loops.push(LoopLabels { start, breaks: fails, continues: Vec::new(), result: None, with_depth: st.withs.len(), continue_patch: false });
                let t = self.alloc(st);
                self.block_to(st, body, t);
                self.jmp_back(st, start);
                let lp = st.loops.pop().unwrap();
                for b in lp.breaks {
                    self.patch(st, b);
                }
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
            ExprKind::Loop { body } => {
                let start = st.code.len();
                self.emit(st, encode(Op::Safepoint, 0, 0, 0));
                st.loops.push(LoopLabels { start, breaks: Vec::new(), continues: Vec::new(), result: Some(dst), with_depth: st.withs.len(), continue_patch: false });
                let t = self.alloc(st);
                self.block_to(st, body, t);
                self.jmp_back(st, start);
                let lp = st.loops.pop().unwrap();
                for b in lp.breaks {
                    self.patch(st, b);
                }
            }
            ExprKind::For { pattern, iter, body } => self.for_loop(st, e, pattern, iter, body, dst),
            ExprKind::Match { subject, arms } => self.match_expr(st, subject, arms, dst),
            ExprKind::Block(b) => self.block_to(st, b, dst),
            ExprKind::Return(v) => {
                let r = match v {
                    Some(x) => self.expr_reg(st, x),
                    None => {
                        let t = self.alloc(st);
                        self.emit(st, encode(Op::LoadNil, t, 0, 0));
                        t
                    }
                };
                self.unlock_withs(st, 0);
                match ck.control.get(&e.id) {
                    Some(Control::ReturnUnwind(fl)) => {
                        let f = self.alloc(st);
                        self.load_local(st, *fl, f);
                        self.emit(st, encode(Op::Unwind, 1, f, r));
                    }
                    _ => {
                        let ret = ck.fns[st.fid as usize].ret;
                        if let Some(x) = v {
                            if ck.types.shallow_resolve(ret) == ck.types.float
                                && ck.types.shallow_resolve(ck.ty_of(x.id)) == ck.types.int
                                && !ck.coerce.contains_key(&x.id)
                            {
                                self.emit(st, encode(Op::IToF, r, r, 0));
                            }
                        }
                        self.emit(st, encode(Op::Return, r, 0, 0));
                    }
                }
            }
            ExprKind::Break(v) => {
                match ck.control.get(&e.id) {
                    Some(Control::BreakUnwind(fl)) => {
                        let r = match v {
                            Some(x) => self.expr_reg(st, x),
                            None => {
                                let t = self.alloc(st);
                                self.emit(st, encode(Op::LoadNil, t, 0, 0));
                                t
                            }
                        };
                        let f = self.alloc(st);
                        self.load_local(st, *fl, f);
                        self.emit(st, encode(Op::Unwind, 0, f, r));
                    }
                    _ => {
                        let target = st.loops.last().and_then(|l| l.result);
                        if let (Some(x), Some(rr)) = (v, target) {
                            self.expr_to(st, x, rr);
                        } else if let Some(rr) = target {
                            self.emit(st, encode(Op::LoadNil, rr, 0, 0));
                        }
                        let depth = st.loops.last().map(|l| l.with_depth).unwrap_or(0);
                        self.unlock_withs(st, depth);
                        let j = self.jmp(st);
                        if let Some(lp) = st.loops.last_mut() {
                            lp.breaks.push(j);
                        }
                    }
                }
            }
            ExprKind::Continue => match ck.control.get(&e.id) {
                Some(Control::ContinueReturn) => {
                    self.emit(st, encode(Op::ReturnNil, 0, 0, 0));
                }
                _ => {
                    let depth = st.loops.last().map(|l| l.with_depth).unwrap_or(0);
                    self.unlock_withs(st, depth);
                    let (start, deferred) = st.loops.last().map(|l| (l.start, l.continue_patch)).unwrap_or((0, false));
                    if deferred {
                        let j = self.jmp(st);
                        st.loops.last_mut().unwrap().continues.push(j);
                    } else {
                        self.jmp_back(st, start);
                    }
                }
            },
            ExprKind::Assign { .. } | ExprKind::Let { .. } => {
                self.assign_or_let(st, e);
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
            ExprKind::Is { expr, .. } => {
                let mark = st.next;
                let r = self.expr_reg(st, expr);
                if let Some(PatRes::Type { rt, .. }) = ck.pats.get(&e.id) {
                    self.emit(st, encode(Op::Is, dst, r, 0));
                    self.emit(st, *rt);
                }
                self.release_to(st, mark);
            }
            ExprKind::AsCast { expr, .. } => {
                let mark = st.next;
                let r = self.expr_reg(st, expr);
                if let Some(PatRes::Type { rt, .. }) = ck.pats.get(&e.id) {
                    self.emit(st, encode(Op::AsCast, dst, r, 0));
                    self.emit(st, *rt);
                }
                self.release_to(st, mark);
            }
            ExprKind::Unwrap(x) => {
                let mark = st.next;
                let r = self.expr_reg(st, x);
                self.emit(st, encode(Op::Unwrap, dst, r, 0));
                self.release_to(st, mark);
            }
            ExprKind::Try(x) => self.try_expr(st, e, x, dst),
            ExprKind::Spawn { body, .. } => {
                if let Some(info) = ck.closures.get(&e.id).cloned() {
                    let fid = self.gen_closure_fn(st, &info, body, None);
                    let base = self.alloc_n(st, 2);
                    self.emit(st, encode(Op::Closure, base, 0, 0));
                    self.emit(st, fid);
                    let is_move = ck.spawn_nodes.get(&e.id).copied().unwrap_or(false);
                    self.emit(st, encode(if is_move { Op::LoadTrue } else { Op::LoadFalse }, base + 1, 0, 0));
                    let n = self.internal_native("__spawn");
                    self.emit(st, encode(Op::CallNative, base, 2, 0));
                    self.emit(st, n);
                    self.emit(st, encode(Op::Move, dst, base, 0));
                }
            }
            ExprKind::Move(x) => self.expr_to(st, x, dst),
            ExprKind::WithLock { obj, body } => {
                let o = self.alloc(st);
                self.expr_to(st, obj, o);
                self.emit(st, encode(Op::LockObj, o, 0, 0));
                st.withs.push(o);
                self.block_to(st, body, dst);
                st.withs.pop();
                self.emit(st, encode(Op::UnlockObj, o, 0, 0));
            }
            ExprKind::Error => {
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
        }
    }

    fn unlock_withs(&mut self, st: &mut FnState, depth: usize) {
        let regs: Vec<u8> = st.withs[depth.min(st.withs.len())..].iter().rev().copied().collect();
        for r in regs {
            self.emit(st, encode(Op::UnlockObj, r, 0, 0));
        }
    }

    fn load_int(&mut self, st: &mut FnState, s: &str, dst: u8) {
        match s.parse::<i64>() {
            Ok(i) if (i16::MIN as i64..=i16::MAX as i64).contains(&i) => {
                self.emit(st, encode_sbx(Op::LoadInt, dst, i as i16));
            }
            Ok(i) if catallaxy_runtime_fits(i) => {
                let k = self.konst(st, Const::Int(i));
                self.emit(st, encode_bx(Op::LoadK, dst, k));
            }
            _ => {
                let k = self.konst(st, Const::BigInt(s.to_string()));
                self.emit(st, encode_bx(Op::LoadK, dst, k));
            }
        }
    }

    fn string(&mut self, st: &mut FnState, parts: &[StrPart], dst: u8) {
        if parts.len() == 1 {
            if let StrPart::Lit(s) = &parts[0] {
                let k = self.konst(st, Const::Str(s.clone()));
                self.emit(st, encode_bx(Op::LoadK, dst, k));
                return;
            }
        }
        let mark = st.next;
        let base = self.alloc_n(st, parts.len());
        for (i, p) in parts.iter().enumerate() {
            let r = base + i as u8;
            match p {
                StrPart::Lit(s) => {
                    let k = self.konst(st, Const::Str(s.clone()));
                    self.emit(st, encode_bx(Op::LoadK, r, k));
                }
                StrPart::Expr(x) => {
                    self.expr_to(st, x, r);
                    let is_str = self.ck.types.shallow_resolve(self.ck.ty_of(x.id)) == self.ck.types.string;
                    if !is_str {
                        self.emit(st, encode(Op::ToS, r, r, 0));
                    }
                }
            }
        }
        self.emit(st, encode(Op::Concat, dst, base, parts.len() as u8));
        self.release_to(st, mark);
    }

    fn ident(&mut self, st: &mut FnState, e: &Expr, dst: u8) {
        match self.ck.names.get(&e.id).copied() {
            Some(NameRes::Local(l)) => self.load_local(st, l, dst),
            Some(NameRes::Global(g)) => {
                self.emit(st, encode(Op::GetGlobal, dst, 0, 0));
                self.emit(st, g);
            }
            Some(NameRes::Fn(f)) => {
                self.emit(st, encode(Op::Closure, dst, 0, 0));
                self.emit(st, f);
            }
            _ => {
                // Payload-less variant (`Ok`-style names) or error.
                if let Some(CallRes::Variant { class, .. }) = self.ck.calls.get(&e.id) {
                    self.emit(st, encode(Op::New, dst, 0, 0));
                    self.emit(st, *class);
                } else {
                    self.emit(st, encode(Op::LoadNil, dst, 0, 0));
                }
            }
        }
    }

    fn binary(&mut self, st: &mut FnState, e: &Expr, op: BinOp, l: &Expr, r: &Expr, dst: u8) {
        let ck = self.ck;
        let mark = st.next;
        match ck.bins.get(&e.id).cloned() {
            Some(BinRes::Logic) => {
                let fails = self.cond_jump(st, e);
                self.emit(st, encode(Op::LoadTrue, dst, 0, 0));
                let end = self.jmp(st);
                for f in fails {
                    self.patch(st, f);
                }
                self.emit(st, encode(Op::LoadFalse, dst, 0, 0));
                self.patch(st, end);
            }
            Some(BinRes::Coalesce) => {
                self.expr_to(st, l, dst);
                let j = self.emit_jump(st, Op::JmpIfNotNil, dst);
                self.expr_to(st, r, dst);
                self.patch(st, j);
            }
            Some(BinRes::Arith(k)) => {
                let a = self.expr_reg(st, l);
                if k == ArithKind::Int && op == BinOp::Add || k == ArithKind::Int && op == BinOp::Sub {
                    if let ExprKind::Int(s) = &r.kind {
                        if let Ok(i) = s.parse::<i64>() {
                            let i = if op == BinOp::Sub { -i } else { i };
                            if (-128..=127).contains(&i) {
                                self.emit(st, encode(Op::AddIK, dst, a, i as i8 as u8));
                                self.release_to(st, mark);
                                return;
                            }
                        }
                    }
                }
                let b = self.expr_reg(st, r);
                self.emit(st, encode(arith_op(op, k), dst, a, b));
            }
            Some(BinRes::Concat) => {
                let base = self.alloc_n(st, 2);
                self.expr_to(st, l, base);
                self.expr_to(st, r, base + 1);
                self.emit(st, encode(Op::Concat, dst, base, 2));
            }
            Some(BinRes::Compare(k)) => {
                let a = self.expr_reg(st, l);
                let b = self.expr_reg(st, r);
                self.compare(st, op, k, dst, a, b);
            }
            Some(BinRes::Eq) => {
                let a = self.expr_reg(st, l);
                let b = self.expr_reg(st, r);
                self.emit(st, encode(if op == BinOp::Ne { Op::Ne } else { Op::Eq }, dst, a, b));
            }
            Some(BinRes::Bitwise) => {
                let a = self.expr_reg(st, l);
                let b = self.expr_reg(st, r);
                self.emit(st, encode(bit_op(op), dst, a, b));
            }
            Some(BinRes::Method(dispatch, swapped)) => {
                let base = self.alloc_n(st, 2);
                if swapped {
                    self.expr_to(st, r, base);
                    self.expr_to(st, l, base + 1);
                } else {
                    self.expr_to(st, l, base);
                    self.expr_to(st, r, base + 1);
                }
                self.call_dispatch(st, &dispatch, base, 2);
                if dst != base {
                    self.emit(st, encode(Op::Move, dst, base, 0));
                }
            }
            None => {
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
        }
        self.release_to(st, mark);
    }

    fn compare(&mut self, st: &mut FnState, op: BinOp, k: ArithKind, dst: u8, a: u8, b: u8) {
        let (lt, le, eq) = match k {
            ArithKind::Int => (Op::LtI, Op::LeI, Op::EqI),
            ArithKind::Float => (Op::LtF, Op::LeF, Op::EqF),
            ArithKind::Generic => (Op::Lt, Op::Le, Op::Eq),
        };
        match op {
            BinOp::Lt => {
                self.emit(st, encode(lt, dst, a, b));
            }
            BinOp::Le => {
                self.emit(st, encode(le, dst, a, b));
            }
            BinOp::Gt => {
                self.emit(st, encode(lt, dst, b, a));
            }
            BinOp::Ge => {
                self.emit(st, encode(le, dst, b, a));
            }
            BinOp::Eq => {
                self.emit(st, encode(eq, dst, a, b));
            }
            BinOp::Ne => {
                if k == ArithKind::Generic {
                    self.emit(st, encode(Op::Ne, dst, a, b));
                } else {
                    self.emit(st, encode(eq, dst, a, b));
                    self.emit(st, encode(Op::Not, dst, dst, 0));
                }
            }
            BinOp::Cmp => {
                self.emit(st, encode(Op::Cmp, dst, a, b));
            }
            _ => unreachable!(),
        }
    }

    /// Emits a condition, returning the jumps taken when it is false.
    fn cond_jump(&mut self, st: &mut FnState, cond: &Expr) -> Vec<usize> {
        let mark = st.next;
        let out = match &cond.kind {
            ExprKind::Binary(BinOp::And, l, r) if matches!(self.ck.bins.get(&cond.id), Some(BinRes::Logic)) => {
                let mut f = self.cond_jump(st, l);
                f.extend(self.cond_jump(st, r));
                f
            }
            ExprKind::Binary(BinOp::Or, l, r) if matches!(self.ck.bins.get(&cond.id), Some(BinRes::Logic)) => {
                let lf = self.cond_jump(st, l);
                let to_body = self.jmp(st);
                for j in lf {
                    self.patch(st, j);
                }
                let rf = self.cond_jump(st, r);
                self.patch(st, to_body);
                rf
            }
            ExprKind::Unary(UnOp::Not, x) => {
                let r = self.alloc(st);
                self.expr_to(st, x, r);
                vec![self.emit_jump(st, Op::JmpIf, r)]
            }
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r)
                if matches!(r.kind, ExprKind::Nil) || matches!(l.kind, ExprKind::Nil) =>
            {
                let x = if matches!(r.kind, ExprKind::Nil) { l } else { r };
                let reg = self.expr_reg(st, x);
                vec![self.emit_jump(st, if *op == BinOp::Eq { Op::JmpIfNotNil } else { Op::JmpIfNil }, reg)]
            }
            _ => {
                let r = self.alloc(st);
                self.expr_to(st, cond, r);
                vec![self.emit_jump(st, Op::JmpIfNot, r)]
            }
        };
        self.release_to(st, mark);
        out
    }

    // ---- calls -------------------------------------------------------------------------------

    fn call_dispatch(&mut self, st: &mut FnState, d: &Dispatch, base: u8, nargs: usize) {
        match d {
            Dispatch::Static(f) => {
                self.emit(st, encode(Op::Call, base, nargs as u8, 0));
                self.emit(st, *f);
            }
            Dispatch::Virtual(_, slot) => {
                self.emit(st, encode(Op::CallVirtual, base, nargs as u8, 0));
                self.emit(st, *slot);
            }
            Dispatch::Dynamic(name, fallback) => {
                let ic = self.ic(st, name, *fallback);
                self.emit(st, encode(Op::CallMethod, base, nargs as u8, 0));
                self.emit(st, ic);
            }
        }
    }

    fn arg_values<'e>(&self, slots: &[ArgSlot], args: &'e [Arg], block: Option<&'e Expr>) -> Vec<Option<&'e Expr>> {
        slots
            .iter()
            .map(|s| match s {
                ArgSlot::Arg(i) => Some(&args[*i].value),
                ArgSlot::Block => block,
                ArgSlot::Missing => None,
            })
            .collect()
    }

    fn fill_args(&mut self, st: &mut FnState, base: u8, vals: &[Option<&Expr>]) {
        for (i, v) in vals.iter().enumerate() {
            let r = base + i as u8;
            match v {
                Some(x) => self.expr_to(st, x, r),
                None => {
                    self.emit(st, encode(Op::LoadEmpty, r, 0, 0));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(&mut self, st: &mut FnState, e: &Expr, callee: Option<&Expr>, recv: Option<&Expr>, args: &[Arg], block: Option<&Expr>, dst: u8) {
        let ck = self.ck;
        let mark = st.next;
        let Some(res) = ck.calls.get(&e.id).cloned() else {
            // Paths that resolved to nothing executable (errors were reported).
            self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            return;
        };
        match res {
            CallRes::Fn { dispatch, recv: has_recv, args: slots, safe } => {
                let vals = self.arg_values(&slots, args, block);
                let n = vals.len() + has_recv as usize;
                let base = self.alloc_n(st, n.max(1));
                let mut skip = None;
                if has_recv {
                    if let Some(rv) = recv {
                        self.expr_to(st, rv, base);
                        if safe {
                            skip = Some(self.emit_jump(st, Op::JmpIfNil, base));
                        }
                    } else if let Some(c) = callee {
                        self.expr_to(st, c, base);
                    }
                }
                self.fill_args(st, base + has_recv as u8, &vals);
                self.call_dispatch(st, &dispatch, base, n);
                if let Some(j) = skip {
                    self.patch(st, j);
                }
                if dst != base {
                    self.emit(st, encode(Op::Move, dst, base, 0));
                }
            }
            CallRes::Closure { args: slots } => {
                let vals = self.arg_values(&slots, args, block);
                let base = self.alloc_n(st, vals.len() + 1);
                if let Some(c) = callee {
                    self.expr_to(st, c, base);
                }
                self.fill_args(st, base + 1, &vals);
                self.emit(st, encode(Op::CallClosure, base, vals.len() as u8, 0));
                if dst != base {
                    self.emit(st, encode(Op::Move, dst, base, 0));
                }
            }
            CallRes::Construct { ctor, args: slots, .. } => {
                let vals = self.arg_values(&slots, args, block);
                let base = self.alloc_n(st, vals.len().max(1));
                self.fill_args(st, base, &vals);
                self.emit(st, encode(Op::Call, base, vals.len() as u8, 0));
                self.emit(st, ctor);
                if dst != base {
                    self.emit(st, encode(Op::Move, dst, base, 0));
                }
            }
            CallRes::Variant { class, args: slots } => {
                let vals = self.arg_values(&slots, args, block);
                let base = self.alloc_n(st, vals.len().max(1));
                self.fill_args(st, base, &vals);
                let obj = self.alloc(st);
                self.emit(st, encode(Op::New, obj, 0, 0));
                self.emit(st, class);
                for i in 0..vals.len() {
                    self.emit(st, encode(Op::SetField, obj, i as u8, base + i as u8));
                }
                self.emit(st, encode(Op::Move, dst, obj, 0));
            }
            CallRes::Field { index, safe } => {
                let r = recv.map(|rv| self.expr_reg(st, rv)).unwrap_or(0);
                if safe {
                    self.emit(st, encode(Op::Move, dst, r, 0));
                    let j = self.emit_jump(st, Op::JmpIfNil, dst);
                    self.emit(st, encode(Op::GetField, dst, r, index as u8));
                    self.patch(st, j);
                } else {
                    self.emit(st, encode(Op::GetField, dst, r, index as u8));
                }
            }
            CallRes::TupleField(i) => {
                let r = recv.map(|rv| self.expr_reg(st, rv)).unwrap_or(0);
                self.emit(st, encode(Op::TupleGet, dst, r, i as u8));
            }
            CallRes::Global(g) => {
                self.emit(st, encode(Op::GetGlobal, dst, 0, 0));
                self.emit(st, g);
            }
            CallRes::FnValue(f) => {
                self.emit(st, encode(Op::Closure, dst, 0, 0));
                self.emit(st, f);
            }
            CallRes::Freeze => {
                if let Some(a) = args.first() {
                    let r = self.expr_reg(st, &a.value);
                    self.emit(st, encode(Op::Freeze, dst, r, 0));
                }
            }
            CallRes::DynMember(name) => {
                let r = match recv {
                    Some(rv) => self.expr_reg(st, rv),
                    None => {
                        let r = self.alloc_n(st, 1);
                        self.emit(st, encode(Op::LoadNil, r, 0, 0));
                        r
                    }
                };
                let ic = self.ic(st, &name, None);
                self.emit(st, encode(Op::DynGet, dst, r, 0));
                self.emit(st, ic);
            }
            CallRes::ListGet | CallRes::IndexGet | CallRes::None => {
                self.emit(st, encode(Op::LoadNil, dst, 0, 0));
            }
        }
        self.release_to(st, mark);
    }

    fn try_expr(&mut self, st: &mut FnState, e: &Expr, x: &Expr, dst: u8) {
        let ck = self.ck;
        let mark = st.next;
        let v = self.alloc(st);
        self.expr_to(st, x, v);
        let is_result = ck.calls.contains_key(&e.id);
        
        let ok_jump = if is_result {
            // Ok has tag 0.
            let t = self.alloc(st);
            self.emit(st, encode(Op::EnumTag, t, v, 0));
            let z = self.alloc(st);
            self.emit(st, encode_sbx(Op::LoadInt, z, 0));
            self.emit(st, encode(Op::EqI, t, t, z));
            self.emit_jump(st, Op::JmpIf, t)
        } else {
            self.emit_jump(st, Op::JmpIfNotNil, v)
        };
        // Propagate: return the Err value (or nil) from the function.
        self.unlock_withs(st, 0);
        match ck.control.get(&e.id) {
            Some(Control::ReturnUnwind(fl)) => {
                let f = self.alloc(st);
                self.load_local(st, *fl, f);
                self.emit(st, encode(Op::Unwind, 1, f, v));
            }
            _ => {
                self.emit(st, encode(Op::Return, v, 0, 0));
            }
        }
        self.patch(st, ok_jump);
        if is_result {
            self.emit(st, encode(Op::GetField, dst, v, 0));
        } else if dst != v {
            self.emit(st, encode(Op::Move, dst, v, 0));
        }
        self.release_to(st, mark);
    }

    // ---- loops -------------------------------------------------------------------------------

    fn for_loop(&mut self, st: &mut FnState, e: &Expr, pattern: &Pattern, iter: &Expr, body: &Block, dst: u8) {
        let ck = self.ck;
        let mark = st.next;
        let kind = ck.fors.get(&e.id).cloned().unwrap_or(ForKind::List);
        match kind {
            ForKind::Range => {
                // Evaluate the range's bounds without allocating when it is a literal range.
                let (lo, hi, excl) = match &iter.kind {
                    ExprKind::Range { lo, hi, exclusive } => {
                        let lr = self.alloc(st);
                        self.expr_to(st, lo, lr);
                        let hr = self.alloc(st);
                        self.expr_to(st, hi, hr);
                        (lr, hr, Some(*exclusive))
                    }
                    _ => {
                        let r = self.expr_reg(st, iter);
                        let lr = self.alloc(st);
                        let hr = self.alloc(st);
                        let ex = self.alloc(st);
                        // first, last, exclusive fields of the Range instance.
                        self.emit(st, encode(Op::GetField, lr, r, 0));
                        self.emit(st, encode(Op::GetField, hr, r, 1));
                        self.emit(st, encode(Op::GetField, ex, r, 2));
                        // Normalize to exclusive: if !exclusive { hi += 1 }.
                        let j = self.emit_jump(st, Op::JmpIf, ex);
                        self.emit(st, encode(Op::AddIK, hr, hr, 1));
                        self.patch(st, j);
                        (lr, hr, Some(true))
                    }
                };
                let i = self.alloc(st);
                self.emit(st, encode(Op::Move, i, lo, 0));
                let c = self.alloc(st);
                let start = st.code.len();
                self.emit(st, encode(if excl == Some(true) { Op::LtI } else { Op::LeI }, c, i, hi));
                let exit = self.emit_jump(st, Op::JmpIfNot, c);
                st.loops.push(LoopLabels { start, breaks: vec![exit], continues: Vec::new(), result: None, with_depth: st.withs.len(), continue_patch: true });
                let mut fails = Vec::new();
                self.pattern(st, pattern, i, &mut fails);
                let t = self.alloc(st);
                self.block_to(st, body, t);
                let lp_conts: Vec<usize> = std::mem::take(&mut st.loops.last_mut().unwrap().continues);
                for cj in lp_conts {
                    self.patch(st, cj);
                }
                self.emit(st, encode(Op::AddIK, i, i, 1));
                self.jmp_back(st, start);
                let lp = st.loops.pop().unwrap();
                for b in lp.breaks {
                    self.patch(st, b);
                }
            }
            ForKind::List | ForKind::Via(_) => {
                let list = self.alloc(st);
                match &kind {
                    ForKind::Via(d) => {
                        let base = self.alloc_n(st, 1);
                        self.expr_to(st, iter, base);
                        self.call_dispatch(st, d, base, 1);
                        self.emit(st, encode(Op::Move, list, base, 0));
                    }
                    _ => self.expr_to(st, iter, list),
                }
                let i = self.alloc(st);
                self.emit(st, encode_sbx(Op::LoadInt, i, 0));
                let n = self.alloc(st);
                let c = self.alloc(st);
                let start = st.code.len();
                self.emit(st, encode(Op::ListLen, n, list, 0));
                self.emit(st, encode(Op::LtI, c, i, n));
                let exit = self.emit_jump(st, Op::JmpIfNot, c);
                st.loops.push(LoopLabels { start, breaks: vec![exit], continues: Vec::new(), result: None, with_depth: st.withs.len(), continue_patch: true });
                let item = self.alloc(st);
                self.emit(st, encode(Op::ListGet, item, list, i));
                let mut fails = Vec::new();
                self.pattern(st, pattern, item, &mut fails);
                let t = self.alloc(st);
                self.block_to(st, body, t);
                let lp_conts: Vec<usize> = std::mem::take(&mut st.loops.last_mut().unwrap().continues);
                for cj in lp_conts {
                    self.patch(st, cj);
                }
                self.emit(st, encode(Op::AddIK, i, i, 1));
                self.jmp_back(st, start);
                let lp = st.loops.pop().unwrap();
                for b in lp.breaks {
                    self.patch(st, b);
                }
            }
            ForKind::Each(d) => {
                let Some(info) = ck.for_bodies.get(&e.id).cloned() else { return };
                let base = self.alloc_n(st, 2);
                self.expr_to(st, iter, base);
                let fid = self.gen_closure_fn(st, &info, body, Some(pattern));
                self.emit(st, encode(Op::Closure, base + 1, 0, 0));
                self.emit(st, fid);
                self.call_dispatch(st, &d, base, 2);
            }
        }
        self.release_to(st, mark);
        self.emit(st, encode(Op::LoadNil, dst, 0, 0));
    }

    // ---- patterns ----------------------------------------------------------------------------

    /// Tests `p` against the value in `subj`, binding on success; failure jumps are collected.
    fn pattern(&mut self, st: &mut FnState, p: &Pattern, subj: u8, fails: &mut Vec<usize>) {
        let ck = self.ck;
        match (&p.kind, ck.pats.get(&p.id).cloned()) {
            (PatternKind::Wildcard, _) => {}
            (PatternKind::Bind(_), Some(PatRes::Bind(l))) => {
                self.store_local(st, l, subj, true);
            }
            (PatternKind::Literal(e), _) => {
                let t = self.alloc(st);
                self.expr_to(st, e, t);
                let kind = match (&e.kind, ck.types.shallow_resolve(ck.ty_of(e.id))) {
                    (ExprKind::Int(_), t) if t == ck.types.int => Op::EqI,
                    _ => Op::Eq,
                };
                if matches!(e.kind, ExprKind::Nil) {
                    fails.push(self.emit_jump(st, Op::JmpIfNotNil, subj));
                } else {
                    self.emit(st, encode(kind, t, subj, t));
                    fails.push(self.emit_jump(st, Op::JmpIfNot, t));
                }
            }
            (PatternKind::Range { lo, hi, exclusive }, _) => {
                let isint = self.alloc(st);
                let rt = self.int_rt();
                self.emit(st, encode(Op::Is, isint, subj, 0));
                self.emit(st, rt);
                fails.push(self.emit_jump(st, Op::JmpIfNot, isint));
                let a = self.alloc(st);
                self.expr_to(st, lo, a);
                self.emit(st, encode(Op::LeI, a, a, subj));
                fails.push(self.emit_jump(st, Op::JmpIfNot, a));
                let b = self.alloc(st);
                self.expr_to(st, hi, b);
                self.emit(st, encode(if *exclusive { Op::LtI } else { Op::LeI }, b, subj, b));
                fails.push(self.emit_jump(st, Op::JmpIfNot, b));
            }
            (PatternKind::Tuple(ps), _) => {
                // The subject may be optional or a union: check it is a tuple first.
                let is_tuple = matches!(ck.types.get(ck.types.shallow_resolve(ck.ty_of_pattern_subject(p))), Ty::Tuple(_));
                if !is_tuple {
                    let t = self.alloc(st);
                    self.emit(st, encode(Op::IsKind, t, subj, KIND_TUPLE));
                    fails.push(self.emit_jump(st, Op::JmpIfNot, t));
                }
                for (i, sp) in ps.iter().enumerate() {
                    let t = self.alloc(st);
                    self.emit(st, encode(Op::TupleGet, t, subj, i as u8));
                    self.pattern(st, sp, t, fails);
                }
            }
            (PatternKind::Typed { .. }, Some(PatRes::Type { rt, bind })) => {
                let t = self.alloc(st);
                self.emit(st, encode(Op::Is, t, subj, 0));
                self.emit(st, rt);
                fails.push(self.emit_jump(st, Op::JmpIfNot, t));
                if let Some(l) = bind {
                    self.store_local(st, l, subj, true);
                }
            }
            (PatternKind::Ctor { args, .. }, Some(PatRes::Variant { class, tag })) => {
                // Check the variant: by tag when the subject is exactly the enum.
                let t = self.alloc(st);
                let rt = self.variant_rt(class);
                self.emit(st, encode(Op::Is, t, subj, 0));
                self.emit(st, rt);
                let _ = tag;
                fails.push(self.emit_jump(st, Op::JmpIfNot, t));
                for (i, sp) in args.iter().enumerate() {
                    if matches!(sp.kind, PatternKind::Wildcard) {
                        continue;
                    }
                    let f = self.alloc(st);
                    self.emit(st, encode(Op::GetField, f, subj, i as u8));
                    self.pattern(st, sp, f, fails);
                }
            }
            (PatternKind::Ctor { .. }, Some(PatRes::Type { rt, .. })) => {
                let t = self.alloc(st);
                self.emit(st, encode(Op::Is, t, subj, 0));
                self.emit(st, rt);
                fails.push(self.emit_jump(st, Op::JmpIfNot, t));
            }
            (PatternKind::Or(alts), _) => {
                let mut oks = Vec::new();
                for (i, a) in alts.iter().enumerate() {
                    let mut af = Vec::new();
                    self.pattern(st, a, subj, &mut af);
                    if i + 1 < alts.len() {
                        oks.push(self.jmp(st));
                        for f in af {
                            self.patch(st, f);
                        }
                    } else {
                        fails.extend(af);
                    }
                }
                for o in oks {
                    self.patch(st, o);
                }
            }
            _ => {}
        }
    }

    fn int_rt(&mut self) -> u32 {
        self.rt_index(RtType::Int)
    }

    fn variant_rt(&mut self, class: ClassId) -> u32 {
        self.rt_index(RtType::Class(class))
    }

    fn rt_index(&mut self, t: RtType) -> u32 {
        if let Some(i) = self.prog.rt_types.iter().position(|x| *x == t) {
            return i as u32;
        }
        self.prog.rt_types.push(t);
        self.prog.rt_types.len() as u32 - 1
    }

    fn match_expr(&mut self, st: &mut FnState, subject: &Expr, arms: &[ast::MatchArm], dst: u8) {
        let mark = st.next;
        let s = self.alloc(st);
        self.expr_to(st, subject, s);
        let mut ends = Vec::new();
        for arm in arms {
            let arm_mark = st.next;
            let saved: Vec<LocalId> = st.regs.keys().copied().collect();
            let mut fails = Vec::new();
            self.pattern(st, &arm.pattern, s, &mut fails);
            if let Some(g) = &arm.guard {
                fails.extend(self.cond_jump(st, g));
            }
            self.line(st, arm.body.span);
            match &arm.body.kind {
                ExprKind::Assign { .. } | ExprKind::Let { .. } => {
                    self.stmt(st, &arm.body);
                    self.emit(st, encode(Op::LoadNil, dst, 0, 0));
                }
                _ => self.expr_to(st, &arm.body, dst),
            }
            ends.push(self.jmp(st));
            for f in fails {
                self.patch(st, f);
            }
            st.regs.retain(|l, _| saved.contains(l));
            self.release_to(st, arm_mark);
        }
        // Unreachable for exhaustive matches.
        let k = self.konst(st, Const::Str("no match arm matched".into()));
        let t = self.alloc(st);
        self.emit(st, encode_bx(Op::LoadK, t, k));
        self.emit(st, encode(Op::Panic, t, 0, 0));
        for e in ends {
            self.patch(st, e);
        }
        self.release_to(st, mark);
    }
}

fn catallaxy_runtime_fits(i: i64) -> bool {
    ((i << 16) >> 16) == i
}

fn arith_op(op: BinOp, k: ArithKind) -> Op {
    match (k, op) {
        (ArithKind::Int, BinOp::Add) => Op::AddI,
        (ArithKind::Int, BinOp::Sub) => Op::SubI,
        (ArithKind::Int, BinOp::Mul) => Op::MulI,
        (ArithKind::Int, BinOp::Div) => Op::DivI,
        (ArithKind::Int, BinOp::Mod) => Op::ModI,
        (ArithKind::Int, BinOp::Pow) => Op::PowI,
        (ArithKind::Float, BinOp::Add) => Op::AddF,
        (ArithKind::Float, BinOp::Sub) => Op::SubF,
        (ArithKind::Float, BinOp::Mul) => Op::MulF,
        (ArithKind::Float, BinOp::Div) => Op::DivF,
        (ArithKind::Float, BinOp::Mod) => Op::ModF,
        (ArithKind::Float, BinOp::Pow) => Op::PowF,
        (_, BinOp::Add) => Op::Add,
        (_, BinOp::Sub) => Op::Sub,
        (_, BinOp::Mul) => Op::Mul,
        (_, BinOp::Div) => Op::Div,
        (_, BinOp::Mod) => Op::Mod,
        (_, BinOp::Pow) => Op::Pow,
        (_, BinOp::BitAnd) => Op::BAnd,
        (_, BinOp::BitOr) => Op::BOr,
        (_, BinOp::BitXor) => Op::BXor,
        (_, BinOp::Shl) => Op::Shl,
        (_, BinOp::Shr) => Op::Shr,
        _ => Op::Add,
    }
}

fn bit_op(op: BinOp) -> Op {
    match op {
        BinOp::BitAnd => Op::BAnd,
        BinOp::BitOr => Op::BOr,
        BinOp::BitXor => Op::BXor,
        BinOp::Shl => Op::Shl,
        _ => Op::Shr,
    }
}
