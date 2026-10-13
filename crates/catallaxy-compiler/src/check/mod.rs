//! The type checker.
//!
//! Phases: [`collect`] declares every module's items and resolves signatures; [`body`] checks
//! function bodies (bidirectional, local inference); the results are side tables keyed by AST node
//! id that [`crate::codegen`] turns into bytecode.

pub mod body;
pub mod caps;
pub mod collect;
pub mod env;
pub mod expr;
pub mod lookup;
pub mod pat;
pub mod rel;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{self, NodeId};
use crate::diag::{Diagnostic, Span};
use crate::types::*;
use catallaxy_vm::bytecode::RtType;
pub use env::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NameRes {
    Local(LocalId),
    Global(GlobalId),
    Fn(FnId),
    Class(ClassId),
    Enum(EnumId),
    Iface(IfaceId),
    Module(ModId),
    Variant(EnumId, u32),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CtxKind {
    Function,
    Closure,
    /// The body of a `for` over a user `Enumerable`, compiled as a block passed to `each`.
    /// `break`, `continue` and `return` inside it unwind to the enclosing frames.
    ForBody,
    /// A `spawn` block or `par_*` block: captures are copied and must be thread-safe.
    Spawn {
        is_move: bool,
    },
}

pub struct FnCtx {
    pub fn_id: FnId,
    pub kind: CtxKind,
    pub ret: TyId,
    pub scopes: Vec<FxHashMap<String, LocalId>>,
    pub narrow: Vec<FxHashMap<LocalId, TyId>>,
    /// Loop nesting inside this context: each entry collects `break` value types.
    pub loops: Vec<LoopCtx>,
    pub captures: Vec<LocalId>,
    pub returns: Vec<TyId>,
    /// Local holding this frame's id, when a nested for-body needs to unwind to it.
    pub frame_local: Option<LocalId>,
    pub self_local: Option<LocalId>,
    /// Locals whose values were moved away.
    pub moved: FxHashSet<LocalId>,
    pub span: Span,
}

pub struct Checker {
    pub types: Types,
    pub diags: Vec<Diagnostic>,
    pub mods: Vec<ModInfo>,
    pub classes: FxHashMap<ClassId, ClassInfo>,
    pub next_class_id: u32,
    pub variant_of_class: FxHashMap<ClassId, (EnumId, u32)>,
    pub enums: Vec<EnumInfo>,
    pub ifaces: Vec<IfaceInfo>,
    pub exts: Vec<ExtendInfo>,
    pub fns: Vec<FnInfo>,
    pub params: Vec<ParamInfo>,
    pub globals: Vec<GlobalInfo>,
    pub opens: Vec<OpenInfo>,
    pub locals: Vec<LocalInfo>,

    // Side tables keyed by node id.
    pub expr_ty: FxHashMap<NodeId, TyId>,
    pub names: FxHashMap<NodeId, NameRes>,
    pub calls: FxHashMap<NodeId, CallRes>,
    pub bins: FxHashMap<NodeId, BinRes>,
    pub fors: FxHashMap<NodeId, ForKind>,
    pub assigns: FxHashMap<NodeId, AssignRes>,
    pub pats: FxHashMap<NodeId, PatRes>,
    pub coerce: FxHashMap<NodeId, Coercion>,
    pub closures: FxHashMap<NodeId, ClosureInfo>,
    /// `for` loops whose body is a block: the loop node → closure info.
    pub for_bodies: FxHashMap<NodeId, ClosureInfo>,
    /// Locals declared by assignments / lets / patterns, keyed by the declaring node.
    pub decl_locals: FxHashMap<NodeId, LocalId>,
    /// Locals known to refer to the same owned object (`b = a`), by group: freezing or moving one
    /// applies to all of them. Aliases through fields and container elements are not tracked;
    /// the runtime ownership checks cover those.
    pub alias_group: FxHashMap<LocalId, u32>,
    pub next_alias_group: u32,
    /// Locals of each function (parameters first), by function id.
    pub fn_locals: FxHashMap<FnId, Vec<LocalId>>,
    pub frame_locals: FxHashMap<FnId, LocalId>,
    /// Runtime type descriptors for `is` / `as?` / patterns.
    pub rt_types: Vec<RtType>,
    rt_type_map: FxHashMap<String, u32>,
    pub rt_ifaces: Vec<Vec<String>>,

    pub ctxs: Vec<FnCtx>,
    pub cur_module: ModId,
    /// Interfaces visible everywhere (from the prelude).
    pub prelude_ifaces: Vec<IfaceId>,
    pub satisfies_cache: FxHashMap<(TyId, TyId), bool>,
    pub satisfies_stack: Vec<(TyId, TyId)>,
    /// When set, `Open` requirements are recorded against this function.
    pub open_owner: Option<FnId>,
    pub well_known: WellKnown,
    /// Bodies already checked (main functions, constructors).
    pub checked_bodies: FxHashSet<FnId>,
    pub binding_log: Vec<VarId>,
    pub suppress_diags: u32,
    /// Non-local control flow inside `for` bodies compiled as blocks.
    pub control: FxHashMap<NodeId, Control>,
    /// (caller, callee) pairs of method calls on `self`, for inferring mutating methods.
    pub self_calls: Vec<(FnId, FnId)>,
    /// Calls of user methods on frozen receivers, validated once mutating flags are known.
    pub frozen_calls: Vec<(Span, FnId, String)>,
    /// Closures checked as parallel blocks (`spawn`, `par_*`): node → is_move.
    pub spawn_nodes: FxHashMap<NodeId, bool>,
    /// Captures of `spawn move` blocks that are moved (not sendable): (block node, local).
    pub moved_captures: FxHashSet<(NodeId, LocalId)>,
    pub resolved_classes: FxHashSet<ClassId>,
    pub pattern_subjects: FxHashMap<NodeId, TyId>,
    /// Type parameters instantiated by the last `instantiate_sig` (for bound checks).
    pub last_inst: Vec<(ParamId, TyId)>,
    pub last_inst_subst: FxHashMap<ParamId, TyId>,
    pub pending_bounds: Vec<(ModId, Vec<ParamId>, Vec<ast::TypeParam>, collect::TypeScope)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// `break` out of a `for` body block: unwind to the frame in this local.
    BreakUnwind(LocalId),
    /// `continue` in a `for` body block: return from the block.
    ContinueReturn,
    /// `return` inside a `for` body block: unwind to the function frame in this local.
    ReturnUnwind(LocalId),
}

pub struct LoopCtx {
    pub breaks: Vec<TyId>,
    /// The `for` body block itself (break/continue unwind instead of jumping).
    pub is_foreach: bool,
}

#[derive(Default)]
pub struct WellKnown {
    pub result: Option<EnumId>,
    pub enumerable: Option<IfaceId>,
    pub comparable: Option<IfaceId>,
    pub display: Option<IfaceId>,
}

impl Checker {
    pub fn new() -> Self {
        Checker {
            types: Types::new(),
            diags: Vec::new(),
            mods: Vec::new(),
            classes: FxHashMap::default(),
            next_class_id: builtin::FIRST_USER,
            variant_of_class: FxHashMap::default(),
            enums: Vec::new(),
            ifaces: Vec::new(),
            exts: Vec::new(),
            fns: Vec::new(),
            params: Vec::new(),
            globals: Vec::new(),
            opens: Vec::new(),
            locals: Vec::new(),
            expr_ty: FxHashMap::default(),
            names: FxHashMap::default(),
            calls: FxHashMap::default(),
            bins: FxHashMap::default(),
            fors: FxHashMap::default(),
            assigns: FxHashMap::default(),
            pats: FxHashMap::default(),
            coerce: FxHashMap::default(),
            closures: FxHashMap::default(),
            for_bodies: FxHashMap::default(),
            decl_locals: FxHashMap::default(),
            alias_group: FxHashMap::default(),
            next_alias_group: 0,
            fn_locals: FxHashMap::default(),
            frame_locals: FxHashMap::default(),
            rt_types: Vec::new(),
            rt_type_map: FxHashMap::default(),
            rt_ifaces: Vec::new(),
            ctxs: Vec::new(),
            cur_module: 0,
            prelude_ifaces: Vec::new(),
            satisfies_cache: FxHashMap::default(),
            satisfies_stack: Vec::new(),
            open_owner: None,
            well_known: WellKnown::default(),
            checked_bodies: FxHashSet::default(),
            binding_log: Vec::new(),
            suppress_diags: 0,
            control: FxHashMap::default(),
            self_calls: Vec::new(),
            frozen_calls: Vec::new(),
            spawn_nodes: FxHashMap::default(),
            moved_captures: FxHashSet::default(),
            resolved_classes: FxHashSet::default(),
            pattern_subjects: FxHashMap::default(),
            last_inst: Vec::new(),
            last_inst_subst: FxHashMap::default(),
            pending_bounds: Vec::new(),
        }
    }

    // ---- diagnostics -----------------------------------------------------------------------

    pub fn err(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        if self.suppress_diags == 0 {
            self.diags.push(Diagnostic::error(code, msg, span));
        }
    }

    pub fn emit(&mut self, d: Diagnostic) {
        if self.suppress_diags == 0 {
            self.diags.push(d);
        }
    }

    pub fn has_errors(&self) -> bool {
        self.diags.iter().any(|d| d.is_error())
    }

    // ---- small helpers ---------------------------------------------------------------------

    pub fn new_param(&mut self, name: &str, bounds: Vec<TyId>) -> ParamId {
        self.params.push(ParamInfo { name: name.to_string(), bounds });
        self.params.len() as ParamId - 1
    }

    pub fn param_ty(&mut self, p: ParamId) -> TyId {
        self.types.intern(Ty::Param(p))
    }

    pub fn new_local(&mut self, name: &str, ty: TyId, span: Span, is_param: bool) -> LocalId {
        let ctx = self.ctxs.len() as u32 - 1;
        self.locals.push(LocalInfo {
            name: name.to_string(),
            ty,
            ctx,
            captured: false,
            assigns: 1,
            is_param,
            span,
            used: false,
        });
        let id = self.locals.len() as LocalId - 1;
        let fid = self.ctxs.last().unwrap().fn_id;
        self.fn_locals.entry(fid).or_default().push(id);
        id
    }

    pub fn ctx(&mut self) -> &mut FnCtx {
        self.ctxs.last_mut().expect("no function context")
    }

    pub fn class(&self, id: ClassId) -> &ClassInfo {
        &self.classes[&id]
    }

    pub fn record_ty(&mut self, id: NodeId, ty: TyId) {
        self.expr_ty.insert(id, ty);
    }

    pub fn ty_of(&self, id: NodeId) -> TyId {
        *self.expr_ty.get(&id).unwrap_or(&self.types.error)
    }

    /// Interns a runtime type descriptor.
    pub fn rt_type(&mut self, t: RtType) -> u32 {
        let key = format!("{t:?}");
        if let Some(&i) = self.rt_type_map.get(&key) {
            return i;
        }
        let i = self.rt_types.len() as u32;
        self.rt_types.push(t);
        self.rt_type_map.insert(key, i);
        i
    }

    pub fn iface_rt_index(&mut self, iid: IfaceId) -> u32 {
        if let Some(i) = self.ifaces[iid as usize].rt_index {
            return i;
        }
        let names: Vec<String> = self.ifaces[iid as usize]
            .methods
            .iter()
            .filter(|&&f| !self.fns[f as usize].is_default)
            .map(|&f| self.fns[f as usize].name.clone())
            .collect();
        let i = self.rt_ifaces.len() as u32;
        self.rt_ifaces.push(names);
        self.ifaces[iid as usize].rt_index = Some(i);
        i
    }

    /// Runtime descriptor for a static type (used by `is`, `as?` and type patterns).
    pub fn rt_of(&mut self, t: TyId) -> RtType {
        let t = self.types.resolve(t);
        match self.types.get(t).clone() {
            Ty::Class(c, args) => match c {
                builtin::INT => RtType::Int,
                builtin::FLOAT => RtType::Float,
                builtin::BOOL => RtType::Bool,
                builtin::NIL => RtType::Nil,
                builtin::STRING => RtType::Str,
                builtin::LIST => RtType::List(Box::new(args.first().map(|&a| self.rt_of(a)).unwrap_or(RtType::Any))),
                builtin::SET => RtType::Set(Box::new(args.first().map(|&a| self.rt_of(a)).unwrap_or(RtType::Any))),
                builtin::DICT => RtType::Dict(
                    Box::new(args.first().map(|&a| self.rt_of(a)).unwrap_or(RtType::Any)),
                    Box::new(args.get(1).map(|&a| self.rt_of(a)).unwrap_or(RtType::Any)),
                ),
                _ => RtType::Class(c),
            },
            Ty::Enum(e, _) => RtType::Enum(e),
            Ty::Tuple(ts) => RtType::Tuple(ts.iter().map(|&x| self.rt_of(x)).collect()),
            Ty::Union(ts) => {
                let nil = self.types.nil;
                if ts.len() == 2 && ts.contains(&nil) {
                    let other = if ts[0] == nil { ts[1] } else { ts[0] };
                    RtType::Optional(Box::new(self.rt_of(other)))
                } else {
                    RtType::Union(ts.iter().map(|&x| self.rt_of(x)).collect())
                }
            }
            Ty::Fn(..) => RtType::Fn,
            Ty::Iface(i, _) => RtType::Iface(self.iface_rt_index(i)),
            Ty::Frozen(inner) => self.rt_of(inner),
            _ => RtType::Any,
        }
    }

    // ---- variable binding log for trial unification ---------------------------------------

    pub fn checkpoint(&self) -> usize {
        self.binding_log.len()
    }

    pub fn rollback(&mut self, cp: usize) {
        while self.binding_log.len() > cp {
            let v = self.binding_log.pop().unwrap();
            self.types.vars[v as usize] = None;
        }
    }

    pub fn bind_var(&mut self, v: VarId, t: TyId) {
        self.types.vars[v as usize] = Some(t);
        self.binding_log.push(v);
    }

    /// Runs `f` without recording diagnostics, rolling back bindings if it returns false.
    pub fn trial(&mut self, f: impl FnOnce(&mut Self) -> bool) -> bool {
        let cp = self.checkpoint();
        self.suppress_diags += 1;
        let ok = f(self);
        self.suppress_diags -= 1;
        if !ok {
            self.rollback(cp);
        }
        ok
    }
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

/// Re-export for submodules.
pub(crate) use crate::diag::Diagnostic as Diag;

pub fn _unused(_: &ast::Module) {}
