//! Abstract syntax tree. Every expression carries a unique id used by the checker's side tables.

use crate::diag::Span;

pub type NodeId = u32;

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub items: Vec<Item>,
    pub file: u32,
}

#[derive(Clone, Debug)]
pub enum Item {
    Fn(FnDecl),
    Class(ClassDecl),
    Interface(InterfaceDecl),
    Enum(EnumDecl),
    Extend(ExtendDecl),
    Import(ImportDecl),
    Const(ConstDecl),
    Var(VarDecl),
    Stmt(Expr),
}

#[derive(Clone, Debug)]
pub struct TypeParam {
    pub name: Ident,
    pub bounds: Vec<TypeExpr>,
}

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum SelfKind {
    /// Static function (no `self`).
    None,
    Ref,
    Mut,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub is_move: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: Ident,
    pub is_pub: bool,
    pub is_native: bool,
    pub type_params: Vec<TypeParam>,
    pub self_kind: SelfKind,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Option<Block>,
    pub span: Span,
    /// Leading `#` comment lines (documentation), for tooling.
    pub doc: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum ClassModifier {
    Plain,
    Frozen,
    Shared,
}

#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub name: Ident,
    pub ty: TypeExpr,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ClassDecl {
    pub name: Ident,
    pub is_pub: bool,
    pub modifier: ClassModifier,
    pub type_params: Vec<TypeParam>,
    pub superclass: Option<TypeExpr>,
    pub fields: Vec<FieldDecl>,
    pub methods: Vec<FnDecl>,
    /// Class-level constants (`Math.PI`).
    pub consts: Vec<ConstDecl>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InterfaceDecl {
    pub name: Ident,
    pub is_pub: bool,
    pub type_params: Vec<TypeParam>,
    pub methods: Vec<FnDecl>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VariantDecl {
    pub name: Ident,
    pub fields: Vec<(Option<Ident>, TypeExpr)>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: Ident,
    pub is_pub: bool,
    pub type_params: Vec<TypeParam>,
    pub variants: Vec<VariantDecl>,
    pub methods: Vec<FnDecl>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExtendDecl {
    pub is_pub: bool,
    pub type_params: Vec<TypeParam>,
    pub target: TypeExpr,
    pub methods: Vec<FnDecl>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ImportDecl {
    pub path: Vec<Ident>,
    pub items: Option<Vec<Ident>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ConstDecl {
    pub name: Ident,
    pub is_pub: bool,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct VarDecl {
    pub name: Ident,
    pub is_pub: bool,
    pub ty: TypeExpr,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeExprKind {
    /// `Name`, `mod.Name`, `Name[Args]`.
    Named { path: Vec<Ident>, args: Vec<TypeExpr> },
    Optional(Box<TypeExpr>),
    Union(Vec<TypeExpr>),
    Inter(Vec<TypeExpr>),
    Fn { params: Vec<TypeExpr>, ret: Option<Box<TypeExpr>> },
    Tuple(Vec<TypeExpr>),
    Inline(Vec<FnDecl>),
    Frozen(Box<TypeExpr>),
    SelfType,
    /// Placeholder produced after a parse error.
    Error,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub id: NodeId,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Cmp,
    And,
    Or,
    Coalesce,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    In,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Mod => "%",
            Pow => "**",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            Cmp => "<=>",
            And => "and",
            Or => "or",
            Coalesce => "??",
            BitAnd => "&",
            BitOr => "|",
            BitXor => "^",
            Shl => "<<",
            Shr => ">>",
            In => "in",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub enum StrPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ClosureParam {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(String),
    Float(f64),
    Str(Vec<StrPart>),
    Bool(bool),
    Nil,
    Ident(String),
    SelfValue,
    List(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
    Tuple(Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Range { lo: Box<Expr>, hi: Box<Expr>, exclusive: bool },
    /// `callee(args) { block }` where callee is not a member access.
    Call { callee: Box<Expr>, args: Vec<Arg>, block: Option<Box<Expr>> },
    /// `recv.name`, `recv.name(args)`, `recv?.name ...`.
    Member { recv: Box<Expr>, name: Ident, args: Vec<Arg>, block: Option<Box<Expr>>, parens: bool, safe: bool },
    Index { recv: Box<Expr>, args: Vec<Expr> },
    Closure { params: Vec<ClosureParam>, ret: Option<TypeExpr>, body: Block, fn_syntax: bool },
    If { cond: Box<Expr>, then: Block, else_: Option<Box<Expr>> },
    IfLet { pattern: Pattern, value: Box<Expr>, then: Block, else_: Option<Box<Expr>> },
    While { cond: Box<Expr>, body: Block },
    Loop { body: Block },
    For { pattern: Pattern, iter: Box<Expr>, body: Block },
    Match { subject: Box<Expr>, arms: Vec<MatchArm> },
    Block(Block),
    Return(Option<Box<Expr>>),
    Break(Option<Box<Expr>>),
    Continue,
    Assign { target: Box<Expr>, op: Option<BinOp>, value: Box<Expr>, ty: Option<TypeExpr> },
    Let { pattern: Pattern, value: Box<Expr> },
    Is { expr: Box<Expr>, ty: TypeExpr },
    AsCast { expr: Box<Expr>, ty: TypeExpr },
    Unwrap(Box<Expr>),
    Try(Box<Expr>),
    Spawn { is_move: bool, body: Block },
    Move(Box<Expr>),
    WithLock { obj: Box<Expr>, body: Block },
    /// Placeholder produced after a parse error.
    Error,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
    pub id: NodeId,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    Wildcard,
    Bind(Ident),
    /// A literal expression (number, string, bool, nil, negative number).
    Literal(Box<Expr>),
    Range { lo: Box<Expr>, hi: Box<Expr>, exclusive: bool },
    /// `Path.Variant(args)`, `Ok(x)`, `.Variant`, or a type name (resolved by the checker).
    Ctor { path: Vec<Ident>, args: Vec<Pattern>, has_parens: bool, shorthand: bool },
    Tuple(Vec<Pattern>),
    /// `name: Type` binding with a type test.
    Typed { name: Ident, ty: TypeExpr },
    Or(Vec<Pattern>),
}
