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

/// A point to roll the checker's inference state back to (see [`Checker::trial`]).
#[derive(Copy, Clone, Debug)]
pub struct Checkpoint {
    bindings: usize,
    conforms: usize,
}

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

/// A field path rooted at a local (or `self`): `n.next` is `(n, ["next"])`.
pub type FieldPath = (LocalId, Vec<String>);

/// The narrowings of a context: of locals, and of field paths (see `FnCtx`).
pub type NarrowState = (Vec<FxHashMap<LocalId, TyId>>, Vec<FxHashMap<FieldPath, (TyId, bool)>>);

/// The narrowings in effect at one point, without layers.
pub type FlatNarrow = (FxHashMap<LocalId, TyId>, FxHashMap<FieldPath, (TyId, bool)>);

/// What a narrowing applies to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NKey {
    Local(LocalId),
    /// A field path. `stable` when no call can change it: every field on the path belongs to a
    /// frozen class or is read through a frozen value.
    Path(FieldPath, bool),
}

impl From<LocalId> for NKey {
    fn from(l: LocalId) -> Self {
        NKey::Local(l)
    }
}

#[derive(Clone)]
pub struct FnCtx {
    pub fn_id: FnId,
    pub kind: CtxKind,
    pub ret: TyId,
    pub scopes: Vec<FxHashMap<String, LocalId>>,
    pub narrow: Vec<FxHashMap<LocalId, TyId>>,
    /// Narrowed field paths, layered like `narrow`: (type, stable). They apply only in this
    /// context (a closure may run after the field changed) and are dropped by any assignment to
    /// a field of the same name, a reassignment of the root, or (unless stable) any call.
    pub path_narrow: Vec<FxHashMap<FieldPath, (TyId, bool)>>,
    /// Loop nesting inside this context: each entry collects `break` value types.
    pub loops: Vec<LoopCtx>,
    pub captures: Vec<LocalId>,
    pub returns: Vec<TyId>,
    /// Local holding this frame's id, when a nested for-body needs to unwind to it.
    pub frame_local: Option<LocalId>,
    pub self_local: Option<LocalId>,
    /// Locals whose values were moved away.
    pub moved: FxHashSet<LocalId>,
    /// The assignments in this context's body (closures included), for the locals it declares.
    pub scan: std::sync::Arc<AssignScan>,
    pub span: Span,
}

/// What a function, closure or `for` body (closures in it included) assigns, found before it is
/// checked: assignments that come later in the text may still run first, through closures,
/// loops and fibers.
#[derive(Default, Debug)]
pub struct AssignScan {
    /// Names assigned inside a closure or `spawn` block.
    pub in_closures: FxHashSet<String>,
    /// How many times each name is assigned (declarations included).
    pub counts: FxHashMap<String, u32>,
}

impl AssignScan {
    pub fn new<'a>(stmts: impl IntoIterator<Item = &'a ast::Expr>) -> Self {
        let mut scan = AssignScan::default();
        let mut stack: Vec<(&ast::Expr, bool)> = stmts.into_iter().map(|x| (x, false)).collect();
        while let Some((x, in_closure)) = stack.pop() {
            if let ast::ExprKind::Assign { target, .. } = &x.kind {
                if let ast::ExprKind::Ident(n) = &target.kind {
                    *scan.counts.entry(n.clone()).or_default() += 1;
                    if in_closure {
                        scan.in_closures.insert(n.clone());
                    }
                }
            }
            let in_closure =
                in_closure || matches!(x.kind, ast::ExprKind::Closure { .. } | ast::ExprKind::Spawn { .. });
            stack.extend(crate::query::children(x).into_iter().map(|c| (c, in_closure)));
        }
        scan
    }
}

#[derive(Clone)]
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
    /// For a method call dispatched by name at runtime (on an interface or a type parameter's
    /// bound), the interface member the checker resolved it to. Editor
    /// queries (hover, definition, references) use it; codegen does not.
    pub dyn_targets: FxHashMap<NodeId, FnId>,
    /// Conversions of a concrete type to an interface type that the checker kept (not rolled
    /// back with a failed trial): arguments, assignments, returns, element coercions and bounds.
    /// Each entry keys `conform_impls`. Editor queries (references and rename of interface
    /// members) use it; codegen does not.
    pub conform_log: Vec<(TyId, IfaceId)>,
    /// For each conversion of `conform_log`, the methods of the type that implement the
    /// interface's members: (interface member, implementing method).
    pub conform_impls: FxHashMap<(TyId, IfaceId), Vec<(FnId, FnId)>>,
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
    /// Bumped whenever a call drops unstable field-path narrowings.
    pub path_epoch: u32,
    /// Narrowings of locals that hold throughout the loops being checked (innermost last):
    /// every assignment to the local in the loop must keep its value within the narrowed type.
    /// (local, narrowed type, number of contexts at the loop).
    pub loop_facts: Vec<(LocalId, TyId, usize)>,
    /// Locals whose narrowing a loop being checked dropped, because it assigns them a value that
    /// may not fit it (for diagnostics).
    pub loop_dropped: Vec<LocalId>,
    /// Set to make the next `check_block` record the narrowings at its end in `block_end`.
    pub capture_block_end: bool,
    pub block_end: Option<FlatNarrow>,
    /// Every method name of every type (built on first use, for loop pre-scans).
    pub method_names: FxHashSet<String>,
    /// While set, member names that the loop pre-scan read as fields because no type had a
    /// method of that name are collected in `method_name_misses`: a later module declaring such
    /// a method would have changed the scan (see `driver::Prelude`).
    pub record_method_name_misses: bool,
    pub method_name_misses: FxHashSet<String>,
    /// When set, `Open` requirements are recorded against this function.
    pub open_owner: Option<FnId>,
    pub well_known: WellKnown,
    /// Bodies already checked (main functions, constructors).
    pub checked_bodies: FxHashSet<FnId>,
    pub binding_log: Vec<VarId>,
    pub suppress_diags: u32,
    /// Errors raised while diagnostics were suppressed (lets trial checks detect failure).
    pub suppressed_errors: u32,
    /// Non-local control flow inside `for` bodies compiled as blocks.
    pub control: FxHashMap<NodeId, Control>,
    /// (caller, callee) pairs of method calls on `self`, for inferring mutating methods.
    pub self_calls: Vec<(FnId, FnId)>,
    /// Calls of user methods on frozen receivers, validated once mutating flags are known.
    pub frozen_calls: Vec<(Span, FnId, String)>,
    /// (overridden, override) method pairs. An override that mutates `self` makes the method it
    /// overrides mutating too, since a call through the superclass may run it.
    pub overrides: Vec<(FnId, FnId)>,
    /// How many of `overrides` have had their signatures compared (`check_overrides`).
    pub overrides_checked: usize,
    /// Set while resolving a type-parameter bound: the interface named at its top level is a
    /// bound, not a value type (see `self_param_use`).
    pub in_bound_top: bool,
    /// Interfaces with `Self` in a parameter (or other input) position: the method and the span
    /// of that `Self`, or `None`. Computed on demand.
    pub self_param_ifaces: FxHashMap<IfaceId, Option<(String, Span)>>,
    /// Closures checked as parallel blocks (`spawn`, `par_*`): node → is_move.
    pub spawn_nodes: FxHashMap<NodeId, bool>,
    /// Captures of `spawn move` blocks that are moved (not sendable): (block node, local).
    pub moved_captures: FxHashSet<(NodeId, LocalId)>,
    pub resolved_classes: FxHashSet<ClassId>,
    pub pattern_subjects: FxHashMap<NodeId, TyId>,
    /// Type parameters instantiated by the last `instantiate_sig` (for bound checks).
    pub last_inst: Vec<(ParamId, TyId)>,
    pub last_inst_subst: FxHashMap<ParamId, TyId>,
    /// For a union receiver: each member's method with its instantiated parameters and result,
    /// from the last `instantiate_sig` (empty otherwise).
    pub last_sigs: Vec<(lookup::MethodRef, Vec<PInfo>, TyId)>,
    pub pending_bounds: Vec<(ModId, Vec<ParamId>, Vec<ast::TypeParam>, collect::TypeScope)>,
    /// Instantiations of generic classes, enums and interfaces whose bounds are checked once
    /// every body has been checked (inferred type arguments are known by then).
    pub pending_insts: Vec<PendingInst>,
    pub pending_inst_seen: FxHashSet<(Span, Vec<TyId>)>,
}

/// A use of a generic type with these type arguments, for [`Checker::check_instance_bounds`].
#[derive(Clone, Debug)]
pub struct PendingInst {
    pub module: ModId,
    pub name: String,
    pub params: Vec<ParamId>,
    pub args: Vec<TyId>,
    pub span: Span,
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

#[derive(Clone)]
pub struct LoopCtx {
    pub breaks: Vec<TyId>,
    /// The `for` body block itself (break/continue unwind instead of jumping).
    pub is_foreach: bool,
}

#[derive(Default, Clone)]
pub struct WellKnown {
    pub result: Option<EnumId>,
    pub enumerable: Option<IfaceId>,
    pub comparable: Option<IfaceId>,
    pub display: Option<IfaceId>,
    /// The prelude's `compare_values`: the runtime `<=>` of builtin comparable values (tuples
    /// and lists), the fallback when `<=>` is dispatched dynamically on one of them.
    pub compare_values: Option<FnId>,
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
            dyn_targets: FxHashMap::default(),
            conform_log: Vec::new(),
            conform_impls: FxHashMap::default(),
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
            path_epoch: 0,
            loop_facts: Vec::new(),
            loop_dropped: Vec::new(),
            capture_block_end: false,
            block_end: None,
            method_names: FxHashSet::default(),
            record_method_name_misses: false,
            method_name_misses: FxHashSet::default(),
            open_owner: None,
            well_known: WellKnown::default(),
            checked_bodies: FxHashSet::default(),
            binding_log: Vec::new(),
            suppress_diags: 0,
            suppressed_errors: 0,
            control: FxHashMap::default(),
            self_calls: Vec::new(),
            frozen_calls: Vec::new(),
            overrides: Vec::new(),
            overrides_checked: 0,
            in_bound_top: false,
            self_param_ifaces: FxHashMap::default(),
            spawn_nodes: FxHashMap::default(),
            moved_captures: FxHashSet::default(),
            resolved_classes: FxHashSet::default(),
            pattern_subjects: FxHashMap::default(),
            last_inst: Vec::new(),
            last_inst_subst: FxHashMap::default(),
            last_sigs: Vec::new(),
            pending_bounds: Vec::new(),
            pending_insts: Vec::new(),
            pending_inst_seen: FxHashSet::default(),
        }
    }

    // ---- diagnostics -----------------------------------------------------------------------

    pub fn err(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        if self.suppress_diags == 0 {
            self.diags.push(Diagnostic::error(code, msg, span));
        } else {
            self.suppressed_errors += 1;
        }
    }

    pub fn emit(&mut self, d: Diagnostic) {
        if self.suppress_diags == 0 {
            self.diags.push(d);
        } else if d.is_error() {
            self.suppressed_errors += 1;
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
        let scan = &self.ctxs.last().unwrap().scan;
        let closure_assigned = scan.in_closures.contains(name);
        let reassigned = scan.counts.get(name).is_some_and(|&n| n + u32::from(is_param) > 1);
        self.locals.push(LocalInfo {
            name: name.to_string(),
            ty,
            ctx,
            captured: false,
            assigns: 1,
            closure_assigned,
            reassigned,
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

    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint { bindings: self.binding_log.len(), conforms: self.conform_log.len() }
    }

    pub fn rollback(&mut self, cp: Checkpoint) {
        while self.binding_log.len() > cp.bindings {
            let v = self.binding_log.pop().unwrap();
            self.types.vars[v as usize] = None;
        }
        self.conform_log.truncate(cp.conforms);
    }

    /// The (interface member, implementing method) pairs of every kept conversion to an
    /// interface type, without duplicates. For editor queries.
    pub fn conformances(&self) -> Vec<(FnId, FnId)> {
        let mut seen: FxHashSet<(TyId, IfaceId)> = FxHashSet::default();
        let mut out: FxHashSet<(FnId, FnId)> = FxHashSet::default();
        for key in &self.conform_log {
            if seen.insert(*key) {
                out.extend(self.conform_impls.get(key).into_iter().flatten().copied());
            }
        }
        let mut out: Vec<(FnId, FnId)> = out.into_iter().collect();
        out.sort_unstable();
        out
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
