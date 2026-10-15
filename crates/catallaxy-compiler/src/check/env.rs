//! The checker's semantic model: declarations, scopes, and the side tables codegen consumes.

use rustc_hash::FxHashMap;

use crate::ast::{self, NodeId, SelfKind};
use crate::diag::Span;
use crate::types::*;

pub type FnId = u32;
pub type ModId = u32;
pub type LocalId = u32;
pub type ExtId = u32;
pub type GlobalId = u32;

#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub name: String,
    pub bounds: Vec<TyId>,
}

#[derive(Clone, Debug)]
pub struct PInfo {
    pub name: String,
    pub ty: TyId,
    pub has_default: bool,
    pub is_move: bool,
    pub span: Span,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    None,
    Class(ClassId),
    Enum(EnumId),
    Iface(IfaceId),
    Extend(ExtId),
}

/// Where a function's AST lives.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeclRef {
    /// Module item index.
    Item(ModId, usize),
    /// Method `j` of the type declared by item `i`.
    Method(ModId, usize, usize),
    /// A closure expression (checked with its enclosing body).
    Closure(NodeId),
    /// Compiler-generated (field getters, main bodies).
    Synthetic,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CheckState {
    Unchecked,
    InProgress,
    Done,
}

#[derive(Clone, Debug)]
pub enum Synth {
    None,
    /// `fn field(self) { self.field }`
    Getter {
        field: usize,
    },
    /// A module's top-level statements.
    Main(ModId),
    /// Evaluates class field defaults then runs `init` (generated `Class.new`).
    Constructor(ClassId),
}

#[derive(Clone, Debug)]
pub struct FnInfo {
    pub name: String,
    pub module: ModId,
    pub owner: Owner,
    pub owner_params: Vec<ParamId>,
    pub type_params: Vec<ParamId>,
    pub self_kind: SelfKind,
    /// Type of `self` inside the body.
    pub self_ty: TyId,
    pub params: Vec<PInfo>,
    pub ret: TyId,
    pub ret_annotated: bool,
    pub is_pub: bool,
    pub is_native: bool,
    pub native_name: Option<String>,
    pub mutating: bool,
    pub decl: DeclRef,
    pub synth: Synth,
    pub span: Span,
    pub implicit: Option<ImplicitSig>,
    pub has_untyped_params: bool,
    pub state: CheckState,
    /// Runtime function id.
    pub func_id: u32,
    pub is_default: bool,
    pub doc: Option<String>,
}

/// The inferred signature of a function with untyped parameters.
#[derive(Clone, Debug, Default)]
pub struct ImplicitSig {
    pub params: Vec<TyId>,
    pub ret: TyId,
}

#[derive(Clone, Debug)]
pub struct OpenInfo {
    /// The untyped parameter this open type is (or is derived from).
    pub param_name: String,
    /// For a derived open, the expression it stands for (`p.name`, `p + _`); `None` for the
    /// parameter itself.
    pub label: Option<String>,
    pub reqs: Vec<OpenReq>,
    pub uppers: Vec<(TyId, Span)>,
}

/// One structural requirement on an implicit-generic parameter (or on a value derived from one),
/// recorded with the types the body uses it at. Each call site checks every requirement against
/// the argument's type with the same rules as typed code, and maps `ret` to the type it produces.
#[derive(Clone, Debug)]
pub enum OpenReq {
    /// `x.name(args)` or `x.name`. `named` holds argument labels (`None` for positional) for the
    /// leading `named.len()` entries of `args`; with `block`, the last entry is a trailing block.
    Method {
        name: String,
        args: Vec<TyId>,
        named: Vec<Option<String>>,
        block: bool,
        ret: TyId,
        span: Span,
    },
    /// `x op rhs`.
    BinOp {
        op: ast::BinOp,
        rhs: TyId,
        ret: TyId,
        span: Span,
    },
    /// `lhs op x` where `lhs` is not open.
    BinOpRhs {
        op: ast::BinOp,
        lhs: TyId,
        ret: TyId,
        span: Span,
    },
    Unary {
        op: ast::UnOp,
        ret: TyId,
        span: Span,
    },
    Index {
        args: Vec<TyId>,
        ret: TyId,
        span: Span,
    },
    /// `x[index] = value` (or `x[index] op= value`).
    SetIndex {
        index: TyId,
        value: TyId,
        op: Option<ast::BinOp>,
        span: Span,
    },
    Iter {
        elem: TyId,
        span: Span,
    },
    Call {
        args: Vec<TyId>,
        ret: TyId,
        span: Span,
    },
}

impl OpenReq {
    /// The open type standing for this requirement's result, if any.
    pub fn result(&self) -> Option<TyId> {
        match self {
            OpenReq::Method { ret, .. }
            | OpenReq::BinOp { ret, .. }
            | OpenReq::BinOpRhs { ret, .. }
            | OpenReq::Unary { ret, .. }
            | OpenReq::Index { ret, .. }
            | OpenReq::Call { ret, .. } => Some(*ret),
            OpenReq::Iter { elem, .. } => Some(*elem),
            OpenReq::SetIndex { .. } => None,
        }
    }

    /// The types this requirement is used at (its inputs, not its result).
    pub fn inputs(&self) -> Vec<TyId> {
        match self {
            OpenReq::Method { args, .. } | OpenReq::Index { args, .. } | OpenReq::Call { args, .. } => args.clone(),
            OpenReq::BinOp { rhs, .. } => vec![*rhs],
            OpenReq::BinOpRhs { lhs, .. } => vec![*lhs],
            OpenReq::SetIndex { index, value, .. } => vec![*index, *value],
            OpenReq::Unary { .. } | OpenReq::Iter { .. } => Vec::new(),
        }
    }

    /// The same requirement with its inputs replaced (in `inputs` order) and its result `ret`.
    pub fn with(&self, inputs: &[TyId], ret: TyId) -> OpenReq {
        let mut r = self.clone();
        match &mut r {
            OpenReq::Method { args, ret: rr, .. }
            | OpenReq::Index { args, ret: rr, .. }
            | OpenReq::Call { args, ret: rr, .. } => {
                args.copy_from_slice(inputs);
                *rr = ret;
            }
            OpenReq::BinOp { rhs, ret: rr, .. } => {
                *rhs = inputs[0];
                *rr = ret;
            }
            OpenReq::BinOpRhs { lhs, ret: rr, .. } => {
                *lhs = inputs[0];
                *rr = ret;
            }
            OpenReq::SetIndex { index, value, .. } => {
                *index = inputs[0];
                *value = inputs[1];
            }
            OpenReq::Unary { ret: rr, .. } => *rr = ret,
            OpenReq::Iter { elem, .. } => *elem = ret,
        }
        r
    }
}

#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub name: String,
    pub ty: TyId,
    /// Owning class and index of the field declaration in that class's AST (for defaults).
    pub decl_class: ClassId,
    pub decl_index: usize,
    pub has_default: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub id: ClassId,
    pub name: String,
    pub module: ModId,
    pub item: Option<usize>,
    pub params: Vec<ParamId>,
    pub modifier: ast::ClassModifier,
    pub superclass: Option<TyId>,
    /// All fields, inherited first.
    pub fields: Vec<FieldInfo>,
    /// Methods declared in this class (not inherited).
    pub methods: FxHashMap<String, FnId>,
    pub consts: FxHashMap<String, GlobalId>,
    pub is_builtin: bool,
    pub is_pub: bool,
    pub span: Span,
    pub subclasses: Vec<ClassId>,
    pub init: Option<FnId>,
    pub constructor: Option<FnId>,
    pub getters: FxHashMap<String, FnId>,
    /// Methods overridden in some subclass (dispatched through the vtable).
    pub vslots: FxHashMap<String, u32>,
    pub vtable: Vec<FnId>,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VariantInfo {
    pub name: String,
    pub fields: Vec<(Option<String>, TyId)>,
    pub class_id: ClassId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumInfo {
    pub id: EnumId,
    pub name: String,
    pub module: ModId,
    pub item: usize,
    pub params: Vec<ParamId>,
    pub variants: Vec<VariantInfo>,
    pub methods: FxHashMap<String, FnId>,
    pub is_pub: bool,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct IfaceInfo {
    pub id: IfaceId,
    pub name: String,
    pub module: ModId,
    pub item: Option<usize>,
    pub params: Vec<ParamId>,
    /// The `Self` parameter used in default method bodies.
    pub self_param: ParamId,
    pub methods: Vec<FnId>,
    pub by_name: FxHashMap<String, FnId>,
    pub is_pub: bool,
    pub span: Span,
    /// Runtime interface index (for `is` checks), assigned on demand.
    pub rt_index: Option<u32>,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExtendInfo {
    pub module: ModId,
    pub item: usize,
    pub params: Vec<ParamId>,
    pub target: TyId,
    pub methods: FxHashMap<String, FnId>,
    pub is_pub: bool,
    /// Prelude extensions of a builtin type's own generic form become its class methods.
    pub is_class_methods: bool,
}

#[derive(Clone, Debug)]
pub struct GlobalInfo {
    pub name: String,
    pub ty: TyId,
    pub is_const: bool,
    pub module: ModId,
    /// Runtime global index.
    pub index: u32,
    pub is_pub: bool,
    pub span: Span,
    pub state: CheckState,
    /// Where the initializer lives: (module, item) or a class constant (class, name).
    pub init: GlobalInit,
}

#[derive(Clone, Debug)]
pub enum GlobalInit {
    Item(ModId, usize),
    ClassConst(ClassId, usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScopeItem {
    Class(ClassId),
    Enum(EnumId),
    Iface(IfaceId),
    Fn(FnId),
    Global(GlobalId),
    Module(ModId),
    /// An enum variant usable unqualified (`Ok`, `Err`).
    Variant(EnumId, u32),
}

#[derive(Clone)]
pub struct ModInfo {
    pub name: String,
    pub file: u32,
    pub ast: ast::Module,
    /// Declared items.
    pub items: FxHashMap<String, ScopeItem>,
    /// Items brought in by imports.
    pub imported: FxHashMap<String, (ScopeItem, Span)>,
    pub imports: Vec<ModId>,
    pub extends: Vec<ExtId>,
    pub main_fn: FnId,
    pub is_prelude: bool,
}

#[derive(Clone, Debug)]
pub struct LocalInfo {
    pub name: String,
    pub ty: TyId,
    /// The function context that owns the variable.
    pub ctx: u32,
    pub captured: bool,
    pub assigns: u32,
    /// Assigned inside a closure or block, which may run during any call: its narrowing ends at
    /// the next call (`Checker::drop_call_narrowings`). Found by the function's pre-scan
    /// (`AssignScan`) or when such an assignment is checked.
    pub closure_assigned: bool,
    /// Assigned only by contained closures (see `check::scan`): the declaring spans of the
    /// locals holding them, and their names. Its narrowing ends at a direct call of one of
    /// those locals.
    pub runners: Vec<(Span, String)>,
    /// Assigned more than once somewhere in its function (by name, from the pre-scan, so later
    /// assignments count too): a closure cannot rely on a narrowing of it.
    pub reassigned: bool,
    pub is_param: bool,
    pub span: Span,
    pub used: bool,
}

impl LocalInfo {
    /// A captured variable that is reassigned lives in a heap cell.
    pub fn is_cell(&self) -> bool {
        self.captured && self.assigns > 1
    }
}

/// How a call or member access is performed.
#[derive(Clone, Debug)]
pub enum Dispatch {
    /// Direct call to a known function.
    Static(FnId),
    /// Through the receiver's vtable.
    Virtual(FnId, u32),
    /// By name at runtime, with an optional default implementation.
    Dynamic(String, Option<FnId>),
}

/// An argument slot of a resolved call, in parameter order.
#[derive(Clone, Debug)]
pub enum ArgSlot {
    /// Index into the call's positional/named argument list.
    Arg(usize),
    /// The trailing block.
    Block,
    /// Not provided: the callee uses its default.
    Missing,
}

#[derive(Clone, Debug)]
pub enum CallRes {
    /// A function or method call. `recv` is true when the receiver is passed as the first argument.
    Fn {
        dispatch: Dispatch,
        recv: bool,
        args: Vec<ArgSlot>,
        safe: bool,
    },
    /// Calling a closure value (the callee expression).
    Closure {
        args: Vec<ArgSlot>,
    },
    /// `Class.new(...)`.
    Construct {
        class: ClassId,
        ctor: FnId,
        args: Vec<ArgSlot>,
    },
    /// Enum variant construction.
    Variant {
        class: ClassId,
        args: Vec<ArgSlot>,
    },
    /// Field read (member without parens).
    Field {
        index: u32,
        safe: bool,
    },
    TupleField(u32),
    /// A module-level item accessed through a module or class path.
    Global(GlobalId),
    /// A function referenced as a value.
    FnValue(FnId),
    /// `freeze(x)`.
    Freeze,
    /// `E.values()` on an enum whose variants are all payload-less: the variants' classes, in
    /// declaration order.
    EnumValues(Vec<ClassId>),
    /// Reading an element: List[Int] fast path.
    ListGet,
    /// Generic `x[i]`.
    IndexGet,
    /// Dynamic member access on `Dyn` (by name).
    DynMember(String),
    /// Nothing to do at runtime (type-only path, e.g. a module prefix).
    None,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArithKind {
    Int,
    Float,
    Generic,
}

#[derive(Clone, Debug)]
pub enum BinRes {
    Arith(ArithKind),
    /// String + String.
    Concat,
    /// Comparison with typed fast path.
    Compare(ArithKind),
    /// Structural equality.
    Eq,
    /// A method implements the operator (user overloads, Comparable defaults, `in`).
    Method(Dispatch, bool),
    Logic,
    Coalesce,
    Bitwise,
}

#[derive(Clone, Debug)]
pub enum ForKind {
    List,
    Range,
    /// Iterate a list produced by calling a zero-argument method (dict entries, string chars).
    Via(Dispatch),
    /// Call `each` with the body as a block.
    Each(Dispatch),
}

#[derive(Clone, Debug)]
pub enum AssignRes {
    Local(LocalId),
    Global(GlobalId),
    Field(u32),
    /// `x[i] = v` via `[]=`.
    Index(Dispatch),
    /// Generic `x[i] = v` (lists/dicts at runtime).
    IndexGeneric,
}

#[derive(Clone, Debug)]
pub enum PatRes {
    Bind(LocalId),
    /// Enum variant: runtime class id and tag.
    Variant {
        class: ClassId,
        tag: u32,
    },
    /// Runtime type test against `rt_types[idx]`, optionally binding.
    Type {
        rt: u32,
        bind: Option<LocalId>,
    },
    Literal,
    Wildcard,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Coercion {
    IntToFloat,
}

/// A closure's compiled function and captures.
#[derive(Clone, Debug)]
pub struct ClosureInfo {
    pub fn_id: FnId,
    pub captures: Vec<LocalId>,
    pub params: Vec<LocalId>,
    /// Block parameters destructured from one tuple argument.
    pub destructure: bool,
    /// Frame id local for non-local exits from a `for` body block.
    pub is_for_body: bool,
}
