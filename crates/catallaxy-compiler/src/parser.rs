//! Hand-written recursive-descent parser with Pratt expression parsing.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::lexer::{Kw, Lexer, StrPiece, Tok, Token};

pub struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    file: u32,
    next_id: &'a mut u32,
    pub diags: Vec<Diagnostic>,
    /// Inside an `if`/`while`/`for`/`match` header: `{` does not start a trailing block.
    no_block: bool,
    /// How deeply the parser has recursed (expressions, types and patterns nested in each other).
    nesting: u32,
    /// Set once E0901 is reported, so a deep input reports it once.
    too_deep: bool,
    /// The deepest `nesting` reached, and the longest operator or postfix chain one level built:
    /// together they bound the tree's depth, so that [`limit_depth`] measures only trees that
    /// might be too deep.
    max_nesting: u32,
    max_chain: u32,
}

/// How deeply expressions, types and patterns may nest (`((((x))))`, `[[[x]]]`, `if` in `if`).
/// The parser, checker and code generator recurse over the nesting; the limit keeps them within
/// the compiler's stack (see [`crate::util::COMPILER_STACK`]) instead of overflowing it.
pub const MAX_NESTING: u32 = 1000;

/// How deep the syntax tree of one statement or declaration may be. Unlike [`MAX_NESTING`], this
/// counts operator and method chains (`a + b + c` nests `a + b` inside the second `+`), which
/// the parser builds without recursing but later passes walk recursively.
pub const MAX_DEPTH: u32 = 10_000;

type PResult<T> = Result<T, ()>;

const BLOCK_START: &str = "{";

impl<'a> Parser<'a> {
    pub fn new(toks: Vec<Token>, file: u32, next_id: &'a mut u32) -> Self {
        Parser {
            toks,
            pos: 0,
            file,
            next_id,
            diags: Vec::new(),
            no_block: false,
            nesting: 0,
            too_deep: false,
            max_nesting: 0,
            max_chain: 0,
        }
    }

    // ---- token helpers -----------------------------------------------------------------------

    fn tok(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_tok(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn is_sym(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Sym(x) if *x == s)
    }
    fn is_kw(&self, k: Kw) -> bool {
        matches!(self.tok(), Tok::Kw(x) if *x == k)
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn is_newline(&self) -> bool {
        matches!(self.tok(), Tok::Newline)
    }
    fn skip_newlines(&mut self) {
        while matches!(self.tok(), Tok::Newline) || self.is_sym(";") {
            self.bump();
        }
    }
    fn at_eof(&self) -> bool {
        matches!(self.tok(), Tok::Eof)
    }

    fn describe(&self) -> String {
        match self.tok() {
            Tok::Int(s) => format!("integer `{s}`"),
            Tok::Float(f) => format!("float `{f}`"),
            Tok::Str(_) => "string literal".into(),
            Tok::Ident(s) => format!("identifier `{s}`"),
            Tok::Kw(k) => format!("keyword `{}`", format!("{k:?}").to_lowercase()),
            Tok::Sym(s) => format!("`{s}`"),
            Tok::Newline => "end of line".into(),
            Tok::Eof => "end of file".into(),
        }
    }

    fn error<T>(&mut self, msg: impl Into<String>) -> PResult<T> {
        // Past the nesting limit the parser skips to the end; what is missing there is no news.
        if !self.too_deep {
            let span = self.span();
            self.diags.push(Diagnostic::error("E0100", msg, span));
        }
        Err(())
    }

    fn expect_sym(&mut self, s: &str) -> PResult<Span> {
        if self.is_sym(s) {
            Ok(self.bump().span)
        } else {
            let what = self.describe();
            let span = self.span();
            let mut d = Diagnostic::error("E0101", format!("expected `{s}`, found {what}"), span);
            if s == ")" || s == "]" || s == "}" {
                d = d.suggest(format!("insert `{s}`"), span.empty_at(), s);
            }
            if !self.too_deep {
                self.diags.push(d);
            }
            Err(())
        }
    }

    fn expect_ident(&mut self) -> PResult<Ident> {
        match self.tok().clone() {
            Tok::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            _ => {
                let what = self.describe();
                self.error(format!("expected a name, found {what}"))
            }
        }
    }

    /// A method or member name: an identifier or any keyword (`x.spawn`, `fn match(self)`).
    /// After `.` or `fn` inside a type there is no ambiguity, so keywords are allowed there.
    fn expect_member_name(&mut self) -> PResult<Ident> {
        if let Tok::Kw(k) = self.tok().clone() {
            let span = self.bump().span;
            return Ok(Ident { name: crate::lexer::keyword_text(k).to_string(), span });
        }
        self.expect_ident()
    }

    fn id(&mut self) -> NodeId {
        let id = *self.next_id;
        *self.next_id += 1;
        id
    }

    fn mk(&mut self, kind: ExprKind, span: Span) -> Expr {
        Expr { id: self.id(), kind, span }
    }

    /// Counts one more link of an operator or postfix chain built by one parse level.
    fn chain_link(&mut self, links: &mut u32) {
        *links += 1;
        self.max_chain = self.max_chain.max(*links);
    }

    /// Runs `f` one nesting level deeper, or reports E0901 past [`MAX_NESTING`].
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.nesting >= MAX_NESTING {
            if !self.too_deep {
                self.too_deep = true;
                let span = self.span();
                self.diags.push(too_deep(span, "nested", MAX_NESTING));
            }
            return Err(());
        }
        self.nesting += 1;
        self.max_nesting = self.max_nesting.max(self.nesting);
        let r = f(self);
        self.nesting -= 1;
        r
    }

    /// Skips to a plausible statement boundary after an error. Always makes progress.
    fn recover(&mut self) {
        if self.too_deep {
            // Past the nesting limit the rest is unbalanced brackets and their errors: stop.
            self.pos = self.toks.len() - 1;
            return;
        }
        let start = self.pos;
        self.recover_inner();
        if self.pos == start && !self.at_eof() && !self.is_sym("}") {
            self.bump();
        }
    }

    fn recover_inner(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.tok() {
                Tok::Eof => return,
                Tok::Newline if depth <= 0 => {
                    self.bump();
                    return;
                }
                Tok::Sym("{") | Tok::Sym("(") | Tok::Sym("[") => depth += 1,
                Tok::Sym("}") | Tok::Sym(")") | Tok::Sym("]") => {
                    if depth <= 0 {
                        return;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.bump();
        }
    }

    fn end_stmt(&mut self) -> PResult<()> {
        if self.is_newline() || self.is_sym(";") {
            self.bump();
            Ok(())
        } else if self.is_sym("}") || self.at_eof() {
            Ok(())
        } else {
            let what = self.describe();
            let span = self.span();
            self.diags.push(
                Diagnostic::error("E0102", format!("expected end of statement, found {what}"), span)
                    .note("statements are separated by newlines or `;`"),
            );
            Err(())
        }
    }

    // ---- module and declarations ----------------------------------------------------------

    pub fn parse_module(&mut self) -> Module {
        let mut items = Vec::new();
        self.skip_newlines();
        while !self.at_eof() {
            match self.parse_item() {
                Ok(item) => {
                    items.push(item);
                    if self.end_stmt().is_err() {
                        self.recover();
                    }
                }
                Err(()) => self.recover(),
            }
            if self.is_sym("}") || self.is_sym(")") || self.is_sym("]") {
                let span = self.span();
                self.diags.push(Diagnostic::error("E0103", format!("unmatched {}", self.describe()), span));
                self.bump();
            }
            self.skip_newlines();
        }
        Module { items, file: self.file }
    }

    fn parse_item(&mut self) -> PResult<Item> {
        let start = self.span();
        let is_pub = self.eat_kw(Kw::Pub);
        match self.tok() {
            Tok::Kw(Kw::Fn) | Tok::Kw(Kw::Native) => Ok(Item::Fn(self.parse_fn(is_pub, start, false)?)),
            Tok::Kw(Kw::Class) => Ok(Item::Class(self.parse_class(is_pub, ClassModifier::Plain, start)?)),
            Tok::Kw(Kw::Frozen) if matches!(self.peek_tok(1), Tok::Kw(Kw::Class)) => {
                self.bump();
                Ok(Item::Class(self.parse_class(is_pub, ClassModifier::Frozen, start)?))
            }
            Tok::Kw(Kw::Shared) if matches!(self.peek_tok(1), Tok::Kw(Kw::Class)) => {
                self.bump();
                Ok(Item::Class(self.parse_class(is_pub, ClassModifier::Shared, start)?))
            }
            Tok::Kw(Kw::Interface) => Ok(Item::Interface(self.parse_interface(is_pub, start)?)),
            Tok::Kw(Kw::Enum) => Ok(Item::Enum(self.parse_enum(is_pub, start)?)),
            Tok::Kw(Kw::Extend) => Ok(Item::Extend(self.parse_extend(is_pub, start)?)),
            Tok::Kw(Kw::Import) => {
                if is_pub {
                    return self.error("imports cannot be `pub`");
                }
                Ok(Item::Import(self.parse_import()?))
            }
            Tok::Kw(Kw::Const) => {
                self.bump();
                let name = self.expect_ident()?;
                let ty = if self.eat_sym(":") { Some(self.parse_type()?) } else { None };
                self.expect_sym("=")?;
                let value = self.parse_expr()?;
                let span = start.to(value.span);
                Ok(Item::Const(ConstDecl { name, is_pub, ty, value, span }))
            }
            Tok::Kw(Kw::Var) => {
                self.bump();
                let name = self.expect_ident()?;
                if !self.is_sym(":") {
                    let span = name.span.after();
                    self.diags.push(
                        Diagnostic::error("E0104", "module variables need a type annotation", name.span).suggest(
                            "add a type",
                            span,
                            ": Int",
                        ),
                    );
                    return Err(());
                }
                self.bump();
                let ty = self.parse_type()?;
                self.expect_sym("=")?;
                let value = self.parse_expr()?;
                let span = start.to(value.span);
                Ok(Item::Var(VarDecl { name, is_pub, ty, value, span }))
            }
            _ => {
                if is_pub {
                    return self.error("expected a declaration after `pub`");
                }
                Ok(Item::Stmt(self.parse_stmt()?))
            }
        }
    }

    fn parse_import(&mut self) -> PResult<ImportDecl> {
        let start = self.bump().span;
        let mut path = vec![self.expect_ident()?];
        let mut items = None;
        while self.eat_sym(".") {
            if self.is_sym("{") {
                self.bump();
                let mut list = Vec::new();
                self.skip_newlines();
                while !self.is_sym("}") {
                    list.push(self.expect_ident()?);
                    self.skip_newlines();
                    if !self.eat_sym(",") {
                        break;
                    }
                    self.skip_newlines();
                }
                self.skip_newlines();
                self.expect_sym("}")?;
                items = Some(list);
                break;
            }
            path.push(self.expect_ident()?);
        }
        let mut alias = None;
        if self.is_kw(Kw::As) {
            if items.is_some() {
                return self.error("`as` renames a module; `import a.b.{...}` cannot have one");
            }
            self.bump();
            alias = Some(self.expect_ident()?);
        }
        Ok(ImportDecl { path, items, alias, span: start.to(self.prev_span()) })
    }

    fn parse_type_params(&mut self) -> PResult<Vec<TypeParam>> {
        let mut out = Vec::new();
        if !self.eat_sym("[") {
            return Ok(out);
        }
        loop {
            let name = self.expect_ident()?;
            let mut bounds = Vec::new();
            if self.eat_sym(":") {
                bounds.push(self.parse_type_no_union()?);
                while self.eat_sym("&") {
                    bounds.push(self.parse_type_no_union()?);
                }
            }
            out.push(TypeParam { name, bounds });
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym("]")?;
        Ok(out)
    }

    /// Method or function name, allowing operator names inside classes.
    fn parse_fn_name(&mut self, allow_ops: bool) -> PResult<Ident> {
        if allow_ops {
            if let Tok::Sym(s) = self.tok().clone() {
                let span = self.span();
                let name = match s {
                    "[" => {
                        self.bump();
                        self.expect_sym("]")?;
                        if self.is_sym("=") && !self.toks[self.pos].space_before {
                            self.bump();
                            "[]="
                        } else {
                            "[]"
                        }
                    }
                    "+" | "-" | "*" | "/" | "%" | "**" | "==" | "<" | "<=" | ">" | ">=" | "<=>" | "&" | "|" | "^"
                    | "<<" | ">>" => {
                        self.bump();
                        s
                    }
                    _ => return self.error("expected a method name"),
                };
                return Ok(Ident { name: name.to_string(), span: span.to(self.prev_span()) });
            }
            return self.expect_member_name();
        }
        self.expect_ident()
    }

    fn parse_fn(&mut self, is_pub: bool, start: Span, in_type: bool) -> PResult<FnDecl> {
        let is_native = self.eat_kw(Kw::Native);
        if !self.eat_kw(Kw::Fn) {
            return self.error("expected `fn`");
        }
        let name = self.parse_fn_name(in_type)?;
        let type_params = self.parse_type_params()?;
        self.expect_sym("(")?;
        let mut self_kind = SelfKind::None;
        let mut params = Vec::new();
        self.skip_newlines();
        if self.is_kw(Kw::SelfValue) || (self.is_kw(Kw::Mut) && matches!(self.peek_tok(1), Tok::Kw(Kw::SelfValue))) {
            self_kind = if self.eat_kw(Kw::Mut) { SelfKind::Mut } else { SelfKind::Ref };
            self.bump();
            if !self.is_sym(")") {
                self.expect_sym(",")?;
            }
        }
        self.skip_newlines();
        while !self.is_sym(")") {
            params.push(self.parse_param()?);
            self.skip_newlines();
            if !self.eat_sym(",") {
                break;
            }
            self.skip_newlines();
        }
        self.expect_sym(")")?;
        let ret = if self.eat_sym("->") { Some(self.parse_type()?) } else { None };
        let body = if self.is_sym("{") { Some(self.parse_block()?) } else { None };
        let span = start.to(self.prev_span());
        Ok(FnDecl { name, is_pub, is_native, type_params, self_kind, params, ret, body, span, doc: None })
    }

    fn parse_param(&mut self) -> PResult<Param> {
        let start = self.span();
        let is_move = self.eat_kw(Kw::Move);
        let name = self.expect_ident()?;
        let ty = if self.eat_sym(":") { Some(self.parse_type()?) } else { None };
        let default = if self.eat_sym("=") { Some(self.parse_expr()?) } else { None };
        Ok(Param { name, ty, default, is_move, span: start.to(self.prev_span()) })
    }

    fn parse_class(&mut self, is_pub: bool, modifier: ClassModifier, start: Span) -> PResult<ClassDecl> {
        self.bump(); // class
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        let superclass = if self.eat_sym("<") { Some(self.parse_type_no_union()?) } else { None };
        self.expect_sym("{")?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut consts = Vec::new();
        self.skip_newlines();
        while !self.is_sym("}") && !self.at_eof() {
            let mstart = self.span();
            let r = (|| -> PResult<()> {
                let mpub = self.eat_kw(Kw::Pub);
                if self.eat_kw(Kw::Const) {
                    let cname = self.expect_ident()?;
                    let ty = if self.eat_sym(":") { Some(self.parse_type()?) } else { None };
                    self.expect_sym("=")?;
                    let value = self.parse_expr()?;
                    let span = mstart.to(value.span);
                    consts.push(ConstDecl { name: cname, is_pub: true, ty, value, span });
                } else if self.is_kw(Kw::Fn) || self.is_kw(Kw::Native) {
                    methods.push(self.parse_fn(mpub, mstart, true)?);
                } else {
                    let fname = self.expect_ident()?;
                    self.expect_sym(":")?;
                    let ty = self.parse_type()?;
                    let default = if self.eat_sym("=") { Some(self.parse_expr()?) } else { None };
                    fields.push(FieldDecl { name: fname, ty, default, span: mstart.to(self.prev_span()) });
                }
                self.end_stmt()
            })();
            if r.is_err() {
                self.recover();
            }
            self.skip_newlines();
        }
        self.expect_sym("}")?;
        Ok(ClassDecl {
            name,
            is_pub,
            modifier,
            type_params,
            superclass,
            fields,
            methods,
            consts,
            span: start.to(self.prev_span()),
            doc: None,
        })
    }

    fn parse_interface(&mut self, is_pub: bool, start: Span) -> PResult<InterfaceDecl> {
        self.bump();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect_sym("{")?;
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.is_sym("}") && !self.at_eof() {
            let mstart = self.span();
            let r = (|| -> PResult<()> {
                let mpub = self.eat_kw(Kw::Pub);
                methods.push(self.parse_fn(mpub, mstart, true)?);
                self.end_stmt()
            })();
            if r.is_err() {
                self.recover();
            }
            self.skip_newlines();
        }
        self.expect_sym("}")?;
        Ok(InterfaceDecl { name, is_pub, type_params, methods, span: start.to(self.prev_span()), doc: None })
    }

    fn parse_enum(&mut self, is_pub: bool, start: Span) -> PResult<EnumDecl> {
        self.bump();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect_sym("{")?;
        let mut variants = Vec::new();
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.is_sym("}") && !self.at_eof() {
            let mstart = self.span();
            let r = (|| -> PResult<()> {
                if self.is_kw(Kw::Fn) || self.is_kw(Kw::Pub) || self.is_kw(Kw::Native) {
                    let mpub = self.eat_kw(Kw::Pub);
                    methods.push(self.parse_fn(mpub, mstart, true)?);
                    return self.end_stmt();
                }
                let vname = self.expect_ident()?;
                let mut vfields = Vec::new();
                if self.eat_sym("(") {
                    while !self.is_sym(")") {
                        let named = matches!(self.tok(), Tok::Ident(_)) && matches!(self.peek_tok(1), Tok::Sym(":"));
                        let fname = if named {
                            let n = self.expect_ident()?;
                            self.bump();
                            Some(n)
                        } else {
                            None
                        };
                        vfields.push((fname, self.parse_type()?));
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym(")")?;
                }
                variants.push(VariantDecl { name: vname, fields: vfields, span: mstart.to(self.prev_span()) });
                if !self.eat_sym(",") {
                    self.end_stmt()?;
                }
                Ok(())
            })();
            if r.is_err() {
                self.recover();
            }
            self.skip_newlines();
        }
        self.expect_sym("}")?;
        Ok(EnumDecl { name, is_pub, type_params, variants, methods, span: start.to(self.prev_span()), doc: None })
    }

    fn parse_extend(&mut self, is_pub: bool, start: Span) -> PResult<ExtendDecl> {
        self.bump();
        let type_params = self.parse_type_params()?;
        let target = self.parse_type_no_union()?;
        self.expect_sym("{")?;
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.is_sym("}") && !self.at_eof() {
            let mstart = self.span();
            let r = (|| -> PResult<()> {
                let mpub = self.eat_kw(Kw::Pub);
                methods.push(self.parse_fn(mpub || is_pub, mstart, true)?);
                self.end_stmt()
            })();
            if r.is_err() {
                self.recover();
            }
            self.skip_newlines();
        }
        self.expect_sym("}")?;
        Ok(ExtendDecl { is_pub, type_params, target, methods, span: start.to(self.prev_span()) })
    }

    // ---- types -------------------------------------------------------------------------------

    pub fn parse_type(&mut self) -> PResult<TypeExpr> {
        self.nested(Self::parse_type_inner)
    }

    fn parse_type_inner(&mut self) -> PResult<TypeExpr> {
        let first = self.parse_type_inter()?;
        if !self.is_sym("|") {
            return Ok(first);
        }
        let mut parts = vec![first];
        while self.eat_sym("|") {
            parts.push(self.parse_type_inter()?);
        }
        let span = parts[0].span.to(parts.last().unwrap().span);
        Ok(TypeExpr { kind: TypeExprKind::Union(parts), span })
    }

    fn parse_type_no_union(&mut self) -> PResult<TypeExpr> {
        self.nested(Self::parse_type_postfix)
    }

    fn parse_type_inter(&mut self) -> PResult<TypeExpr> {
        let first = self.parse_type_postfix()?;
        if !self.is_sym("&") {
            return Ok(first);
        }
        let mut parts = vec![first];
        while self.eat_sym("&") {
            parts.push(self.parse_type_postfix()?);
        }
        let span = parts[0].span.to(parts.last().unwrap().span);
        Ok(TypeExpr { kind: TypeExprKind::Inter(parts), span })
    }

    fn parse_type_postfix(&mut self) -> PResult<TypeExpr> {
        let mut t = self.parse_type_primary()?;
        while self.is_sym("?") && !self.toks[self.pos].space_before {
            let end = self.bump().span;
            let span = t.span.to(end);
            t = TypeExpr { kind: TypeExprKind::Optional(Box::new(t)), span };
        }
        Ok(t)
    }

    fn parse_type_primary(&mut self) -> PResult<TypeExpr> {
        let start = self.span();
        match self.tok().clone() {
            Tok::Ident(_) => {
                let mut path = vec![self.expect_ident()?];
                while self.is_sym(".") && matches!(self.peek_tok(1), Tok::Ident(_)) {
                    self.bump();
                    path.push(self.expect_ident()?);
                }
                let mut args = Vec::new();
                if self.is_sym("[") && !self.toks[self.pos].space_before {
                    self.bump();
                    loop {
                        args.push(self.parse_type()?);
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym("]")?;
                }
                Ok(TypeExpr { kind: TypeExprKind::Named { path, args }, span: start.to(self.prev_span()) })
            }
            Tok::Kw(Kw::Nil) => {
                self.bump();
                let path = vec![Ident { name: "Nil".into(), span: start }];
                Ok(TypeExpr { kind: TypeExprKind::Named { path, args: vec![] }, span: start })
            }
            Tok::Kw(Kw::SelfType) => {
                self.bump();
                Ok(TypeExpr { kind: TypeExprKind::SelfType, span: start })
            }
            Tok::Kw(Kw::Frozen) => {
                self.bump();
                let inner = self.parse_type_postfix()?;
                let span = start.to(inner.span);
                Ok(TypeExpr { kind: TypeExprKind::Frozen(Box::new(inner)), span })
            }
            Tok::Kw(Kw::Fn) => {
                self.bump();
                self.expect_sym("(")?;
                let mut params = Vec::new();
                while !self.is_sym(")") {
                    params.push(self.parse_type()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                let ret = if self.eat_sym("->") { Some(Box::new(self.parse_type_postfix()?)) } else { None };
                Ok(TypeExpr { kind: TypeExprKind::Fn { params, ret }, span: start.to(self.prev_span()) })
            }
            Tok::Sym("(") => {
                self.bump();
                let mut elems = Vec::new();
                let mut trailing_comma = false;
                while !self.is_sym(")") {
                    elems.push(self.parse_type()?);
                    trailing_comma = false;
                    if !self.eat_sym(",") {
                        break;
                    }
                    trailing_comma = true;
                }
                self.expect_sym(")")?;
                if elems.len() == 1 && !trailing_comma {
                    return Ok(elems.pop().unwrap());
                }
                Ok(TypeExpr { kind: TypeExprKind::Tuple(elems), span: start.to(self.prev_span()) })
            }
            Tok::Sym("{") => {
                self.bump();
                let mut methods = Vec::new();
                self.skip_newlines();
                while !self.is_sym("}") {
                    let s = self.span();
                    methods.push(self.parse_fn(false, s, true)?);
                    self.skip_newlines();
                }
                self.expect_sym("}")?;
                Ok(TypeExpr { kind: TypeExprKind::Inline(methods), span: start.to(self.prev_span()) })
            }
            _ => {
                let what = self.describe();
                self.error(format!("expected a type, found {what}"))
            }
        }
    }

    // ---- statements and blocks ---------------------------------------------------------------

    pub fn parse_block(&mut self) -> PResult<Block> {
        let start = self.expect_sym("{")?;
        let saved = self.no_block;
        self.no_block = false;
        let stmts = self.parse_stmts_until_brace();
        self.no_block = saved;
        let end = self.expect_sym("}")?;
        Ok(Block { stmts, span: start.to(end) })
    }

    fn parse_stmts_until_brace(&mut self) -> Vec<Expr> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.is_sym("}") && !self.at_eof() {
            match self.parse_stmt() {
                Ok(s) => {
                    stmts.push(s);
                    if self.end_stmt().is_err() {
                        self.recover();
                    }
                }
                Err(()) => self.recover(),
            }
            self.skip_newlines();
        }
        stmts
    }

    pub fn parse_stmt(&mut self) -> PResult<Expr> {
        let start = self.span();
        // `name: Type = value` declaration.
        if matches!(self.tok(), Tok::Ident(_)) && matches!(self.peek_tok(1), Tok::Sym(":")) {
            let name = self.expect_ident()?;
            self.bump();
            let ty = self.parse_type()?;
            self.expect_sym("=")?;
            let value = self.parse_expr()?;
            let span = start.to(value.span);
            let target = self.mk(ExprKind::Ident(name.name), name.span);
            return Ok(self.mk(
                ExprKind::Assign { target: Box::new(target), op: None, value: Box::new(value), ty: Some(ty) },
                span,
            ));
        }
        if self.is_kw(Kw::Let) {
            self.bump();
            let pattern = self.parse_pattern()?;
            self.expect_sym("=")?;
            let value = self.parse_expr()?;
            let span = start.to(value.span);
            return Ok(self.mk(ExprKind::Let { pattern, value: Box::new(value) }, span));
        }
        let e = self.parse_expr()?;
        let op = match self.tok() {
            Tok::Sym("=") => Some(None),
            Tok::Sym("+=") => Some(Some(BinOp::Add)),
            Tok::Sym("-=") => Some(Some(BinOp::Sub)),
            Tok::Sym("*=") => Some(Some(BinOp::Mul)),
            Tok::Sym("/=") => Some(Some(BinOp::Div)),
            Tok::Sym("%=") => Some(Some(BinOp::Mod)),
            Tok::Sym("**=") => Some(Some(BinOp::Pow)),
            Tok::Sym("<<=") => Some(Some(BinOp::Shl)),
            Tok::Sym(">>=") => Some(Some(BinOp::Shr)),
            Tok::Sym("|=") => Some(Some(BinOp::BitOr)),
            Tok::Sym("&=") => Some(Some(BinOp::BitAnd)),
            Tok::Sym("^=") => Some(Some(BinOp::BitXor)),
            Tok::Sym("||=") => Some(Some(BinOp::Or)),
            Tok::Sym("&&=") => Some(Some(BinOp::And)),
            Tok::Sym("??=") => Some(Some(BinOp::Coalesce)),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            match &e.kind {
                ExprKind::Ident(_) | ExprKind::Member { parens: false, safe: false, .. } | ExprKind::Index { .. } => {}
                _ => {
                    self.diags.push(Diagnostic::error("E0105", "invalid assignment target", e.span));
                    return Err(());
                }
            }
            self.skip_newlines();
            let value = self.parse_expr()?;
            let span = start.to(value.span);
            return Ok(self.mk(ExprKind::Assign { target: Box::new(e), op, value: Box::new(value), ty: None }, span));
        }
        Ok(e)
    }

    // ---- expressions -------------------------------------------------------------------------

    pub fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_prec(1)
    }

    fn binop(&self) -> Option<(BinOp, u8, bool)> {
        // (op, precedence, right-assoc)
        Some(match self.tok() {
            Tok::Sym("??") => (BinOp::Coalesce, 1, true),
            Tok::Sym("||") | Tok::Kw(Kw::Or) => (BinOp::Or, 2, false),
            Tok::Sym("&&") | Tok::Kw(Kw::And) => (BinOp::And, 3, false),
            Tok::Sym("==") => (BinOp::Eq, 5, false),
            Tok::Sym("!=") => (BinOp::Ne, 5, false),
            Tok::Sym("<") => (BinOp::Lt, 5, false),
            Tok::Sym("<=") => (BinOp::Le, 5, false),
            Tok::Sym(">") => (BinOp::Gt, 5, false),
            Tok::Sym(">=") => (BinOp::Ge, 5, false),
            Tok::Sym("<=>") => (BinOp::Cmp, 5, false),
            Tok::Kw(Kw::In) => (BinOp::In, 5, false),
            Tok::Sym("|") => (BinOp::BitOr, 7, false),
            Tok::Sym("^") => (BinOp::BitXor, 7, false),
            Tok::Sym("&") => (BinOp::BitAnd, 8, false),
            Tok::Sym("<<") => (BinOp::Shl, 9, false),
            Tok::Sym(">>") => (BinOp::Shr, 9, false),
            Tok::Sym("+") => (BinOp::Add, 10, false),
            Tok::Sym("-") => (BinOp::Sub, 10, false),
            Tok::Sym("*") => (BinOp::Mul, 11, false),
            Tok::Sym("/") => (BinOp::Div, 11, false),
            Tok::Sym("%") => (BinOp::Mod, 11, false),
            Tok::Sym("**") => (BinOp::Pow, 13, true),
            _ => return None,
        })
    }

    fn parse_prec(&mut self, min: u8) -> PResult<Expr> {
        self.nested(|p| p.parse_prec_inner(min))
    }

    fn parse_prec_inner(&mut self, min: u8) -> PResult<Expr> {
        let mut lhs = if min <= 4 && (self.is_kw(Kw::Not)) {
            let start = self.bump().span;
            let e = self.parse_prec(4)?;
            let span = start.to(e.span);
            self.mk(ExprKind::Unary(UnOp::Not, Box::new(e)), span)
        } else {
            self.parse_unary()?
        };
        let mut links = 0;
        loop {
            self.chain_link(&mut links);
            // Ranges (precedence 6, non-associative).
            if min <= 6 && (self.is_sym("..") || self.is_sym("...")) {
                let exclusive = self.is_sym("...");
                self.bump();
                let hi = self.parse_prec(7)?;
                let span = lhs.span.to(hi.span);
                lhs = self.mk(ExprKind::Range { lo: Box::new(lhs), hi: Box::new(hi), exclusive }, span);
                continue;
            }
            // `is` type test (precedence 5).
            if min <= 5 && self.is_kw(Kw::Is) {
                self.bump();
                let ty = self.parse_type_no_union()?;
                let span = lhs.span.to(ty.span);
                lhs = self.mk(ExprKind::Is { expr: Box::new(lhs), ty }, span);
                continue;
            }
            let Some((op, prec, right)) = self.binop() else { break };
            if prec < min {
                break;
            }
            self.bump();
            self.skip_newlines_in_expr();
            let rhs = self.parse_prec(if right { prec } else { prec + 1 })?;
            let span = lhs.span.to(rhs.span);
            lhs = self.mk(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span);
            if prec == 5 {
                // Comparisons do not chain.
                if let Some((_, 5, _)) = self.binop() {
                    let span = self.span();
                    self.diags.push(
                        Diagnostic::error("E0106", "comparison operators cannot be chained", span)
                            .note("use `and` to combine comparisons"),
                    );
                    return Err(());
                }
            }
        }
        Ok(lhs)
    }

    fn skip_newlines_in_expr(&mut self) {
        // The lexer already drops newlines after binary operators; nothing to do.
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.tok() {
            Tok::Sym("-") => Some(UnOp::Neg),
            Tok::Sym("!") => Some(UnOp::Not),
            Tok::Sym("~") => Some(UnOp::BitNot),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            // `-2 ** 2` is `-(2 ** 2)`.
            let e = if op == UnOp::Neg { self.parse_prec(13)? } else { self.nested(Self::parse_unary)? };
            let span = start.to(e.span);
            // Fold negative literals.
            if op == UnOp::Neg {
                if let ExprKind::Int(s) = &e.kind {
                    if !s.starts_with('-') {
                        return Ok(self.mk(ExprKind::Int(format!("-{s}")), span));
                    }
                }
                if let ExprKind::Float(f) = e.kind {
                    return Ok(self.mk(ExprKind::Float(-f), span));
                }
            }
            return Ok(self.mk(ExprKind::Unary(op, Box::new(e)), span));
        }
        let e = self.parse_postfix()?;
        if self.is_sym("**") {
            // Handled by the binary loop with higher precedence than unary minus.
        }
        Ok(e)
    }

    fn parse_args(&mut self) -> PResult<Vec<Arg>> {
        // At `(`.
        self.bump();
        let saved = self.no_block;
        self.no_block = false;
        let mut args = Vec::new();
        while !self.is_sym(")") {
            let name = if matches!(self.tok(), Tok::Ident(_)) && matches!(self.peek_tok(1), Tok::Sym(":")) {
                let n = self.expect_ident()?;
                self.bump();
                Some(n)
            } else {
                None
            };
            let value = self.parse_expr()?;
            args.push(Arg { name, value });
            if !self.eat_sym(",") {
                break;
            }
        }
        self.no_block = saved;
        self.expect_sym(")")?;
        Ok(args)
    }

    fn can_take_block(&mut self) -> bool {
        if self.no_block && self.is_sym(BLOCK_START) && matches!(self.peek_tok(1), Tok::Sym("|") | Tok::Sym("||")) {
            let span = self.span();
            self.diags.push(
                Diagnostic::error(
                    "E0109",
                    "a block argument cannot follow a call in an `if`/`while`/`for`/`match` header",
                    span,
                )
                .note("wrap the call in parentheses: `for x in (xs.map { |v| ... }) {`"),
            );
            // Parse it as the block argument anyway so the rest of the header still makes sense.
            return true;
        }
        !self.no_block && self.is_sym(BLOCK_START)
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        // `(recv.f)(x)` calls the value of `recv.f`; only an unparenthesized `recv.f(x)` is a method call.
        let mut grouped = self.is_sym("(");
        let mut e = self.parse_primary()?;
        let mut links = 0;
        loop {
            self.chain_link(&mut links);
            let tight = !self.toks[self.pos].space_before;
            let was_grouped = std::mem::take(&mut grouped);
            match self.tok() {
                Tok::Sym("(") if tight => {
                    let args = self.parse_args()?;
                    let block = if self.can_take_block() { Some(Box::new(self.parse_block_arg()?)) } else { None };
                    let span = e.span.to(self.prev_span());
                    e = match e.kind {
                        ExprKind::Member { recv, name, args: a, block: None, parens: false, safe }
                            if a.is_empty() && !was_grouped =>
                        {
                            self.mk(ExprKind::Member { recv, name, args, block, parens: true, safe }, span)
                        }
                        _ => self.mk(ExprKind::Call { callee: Box::new(e), args, block }, span),
                    };
                }
                Tok::Sym("[") if tight => {
                    self.bump();
                    let saved = self.no_block;
                    self.no_block = false;
                    let mut args = Vec::new();
                    while !self.is_sym("]") {
                        args.push(self.parse_expr()?);
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.no_block = saved;
                    self.expect_sym("]")?;
                    let span = e.span.to(self.prev_span());
                    e = self.mk(ExprKind::Index { recv: Box::new(e), args }, span);
                }
                Tok::Sym(".") | Tok::Sym("?.") => {
                    let safe = self.is_sym("?.");
                    self.bump();
                    // Tuple field `t.0`.
                    if let Tok::Int(n) = self.tok().clone() {
                        let span = self.bump().span;
                        let name = Ident { name: n, span };
                        let span = e.span.to(span);
                        e = self.mk(
                            ExprKind::Member {
                                recv: Box::new(e),
                                name,
                                args: vec![],
                                block: None,
                                parens: false,
                                safe,
                            },
                            span,
                        );
                        continue;
                    }
                    let name = self.expect_member_name()?;
                    let mut args = Vec::new();
                    let mut parens = false;
                    if self.is_sym("(") && !self.toks[self.pos].space_before {
                        args = self.parse_args()?;
                        parens = true;
                    }
                    let block = if self.can_take_block() { Some(Box::new(self.parse_block_arg()?)) } else { None };
                    let span = e.span.to(self.prev_span());
                    e = self.mk(ExprKind::Member { recv: Box::new(e), name, args, block, parens, safe }, span);
                }
                Tok::Sym("!") if tight => {
                    let end = self.bump().span;
                    let span = e.span.to(end);
                    e = self.mk(ExprKind::Unwrap(Box::new(e)), span);
                }
                Tok::Sym("?") if tight => {
                    let end = self.bump().span;
                    let span = e.span.to(end);
                    e = self.mk(ExprKind::Try(Box::new(e)), span);
                }
                Tok::Kw(Kw::As) => {
                    self.bump();
                    if !(self.is_sym("?") && !self.toks[self.pos].space_before) {
                        return self.error("expected `as?` (checked conversion)");
                    }
                    self.bump();
                    let ty = self.parse_type_no_union()?;
                    let span = e.span.to(ty.span);
                    e = self.mk(ExprKind::AsCast { expr: Box::new(e), ty }, span);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// A trailing block argument: `{ |params| body }` or `{ body }`.
    fn parse_block_arg(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.bump(); // {
        let params = if self.is_sym("|") || self.is_sym("||") { self.parse_closure_params()? } else { Vec::new() };
        let saved = self.no_block;
        self.no_block = false;
        let stmts = self.parse_stmts_until_brace();
        self.no_block = saved;
        let end = self.expect_sym("}")?;
        let span = start.to(end);
        let body = Block { stmts, span };
        Ok(self.mk(ExprKind::Closure { params, ret: None, body, fn_syntax: false }, span))
    }

    fn parse_closure_params(&mut self) -> PResult<Vec<ClosureParam>> {
        if self.eat_sym("||") {
            return Ok(Vec::new());
        }
        self.expect_sym("|")?;
        let mut params = Vec::new();
        while !self.is_sym("|") {
            let name = self.expect_ident()?;
            let ty = if self.eat_sym(":") { Some(self.parse_type_inter()?) } else { None };
            params.push(ClosureParam { name, ty });
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym("|")?;
        Ok(params)
    }

    fn parse_string(&mut self, pieces: Vec<StrPiece>, span: Span) -> PResult<Expr> {
        let mut parts = Vec::new();
        for p in pieces {
            match p {
                StrPiece::Lit(s) => parts.push(StrPart::Lit(s)),
                StrPiece::Code(code, offset) => parts.push(self.parse_interpolation(&code, offset)?),
            }
        }
        Ok(self.mk(ExprKind::Str(parts), span))
    }

    /// Parses `#{expr}` or `#{expr:spec}`. No expression continues with a top-level `:` (named
    /// arguments, `Dict` entries and parameter types all sit inside brackets), so the first `:` the
    /// expression parser stops at starts the spec, which runs to the closing brace.
    fn parse_interpolation(&mut self, code: &str, offset: u32) -> PResult<StrPart> {
        let (toks, diags) = Lexer::new(code, self.file, offset).tokenize();
        let mut sub = Parser::new(toks, self.file, self.next_id);
        (sub.nesting, sub.too_deep) = (self.nesting, self.too_deep);
        sub.skip_newlines();
        let r = sub.parse_expr();
        // (Field by field: `sub` borrows `self.next_id`.)
        self.too_deep |= sub.too_deep;
        self.max_nesting = self.max_nesting.max(sub.max_nesting);
        self.max_chain = self.max_chain.max(sub.max_chain);
        let colon = match sub.tok() {
            Tok::Sym(":" | "::") if r.is_ok() => Some((sub.span().start - offset) as usize),
            _ => None,
        };
        let Some(colon) = colon else {
            self.diags.extend(diags);
            sub.skip_newlines();
            if r.is_ok() && !sub.at_eof() {
                let sp = sub.span();
                sub.diags.push(Diagnostic::error("E0107", "unexpected tokens in interpolation", sp));
            }
            let d = std::mem::take(&mut sub.diags);
            self.diags.extend(d);
            return Ok(StrPart::Expr(r?));
        };
        // Re-lex the expression alone: the spec is not Catallaxy source (`08d` is not a number).
        let (toks, diags) = Lexer::new(&code[..colon], self.file, offset).tokenize();
        self.diags.extend(diags);
        let mut sub = Parser::new(toks, self.file, self.next_id);
        (sub.nesting, sub.too_deep) = (self.nesting, self.too_deep);
        sub.skip_newlines();
        let r = sub.parse_expr();
        // (Field by field: `sub` borrows `self.next_id`.)
        self.too_deep |= sub.too_deep;
        self.max_nesting = self.max_nesting.max(sub.max_nesting);
        self.max_chain = self.max_chain.max(sub.max_chain);
        sub.skip_newlines();
        if r.is_ok() && !sub.at_eof() {
            let sp = sub.span();
            sub.diags.push(Diagnostic::error("E0107", "unexpected tokens in interpolation", sp));
        }
        let d = std::mem::take(&mut sub.diags);
        self.diags.extend(d);
        let expr = r?;
        let spec_text = &code[colon + 1..];
        let spec_start = offset + colon as u32 + 1;
        let spec_span = Span { file: self.file, start: spec_start, end: spec_start + spec_text.len() as u32 };
        match catallaxy_vm::fmtspec::parse(spec_text) {
            Ok(spec) => Ok(StrPart::Fmt(expr, spec, spec_span)),
            Err(e) => {
                let sp = if e.start == e.end {
                    spec_span
                } else {
                    Span { file: self.file, start: spec_start + e.start as u32, end: spec_start + e.end as u32 }
                };
                self.diags.push(Diagnostic::error("E0110", format!("invalid format spec: {}", e.msg), sp));
                Ok(StrPart::Expr(expr))
            }
        }
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        match self.tok().clone() {
            Tok::Int(s) => {
                self.bump();
                Ok(self.mk(ExprKind::Int(s), start))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(self.mk(ExprKind::Float(f), start))
            }
            Tok::Str(pieces) => {
                self.bump();
                self.parse_string(pieces, start)
            }
            Tok::Ident(name) => {
                self.bump();
                Ok(self.mk(ExprKind::Ident(name), start))
            }
            Tok::Kw(Kw::True) => {
                self.bump();
                Ok(self.mk(ExprKind::Bool(true), start))
            }
            Tok::Kw(Kw::False) => {
                self.bump();
                Ok(self.mk(ExprKind::Bool(false), start))
            }
            Tok::Kw(Kw::Nil) => {
                self.bump();
                Ok(self.mk(ExprKind::Nil, start))
            }
            Tok::Kw(Kw::SelfValue) => {
                self.bump();
                Ok(self.mk(ExprKind::SelfValue, start))
            }
            Tok::Kw(Kw::SelfType) => {
                self.bump();
                Ok(self.mk(ExprKind::Ident("Self".into()), start))
            }
            Tok::Sym("(") => {
                self.bump();
                let saved = self.no_block;
                self.no_block = false;
                if self.eat_sym(")") {
                    self.no_block = saved;
                    let span = start.to(self.prev_span());
                    self.diags.push(Diagnostic::error("E0108", "empty tuple `()` is not a value; use `nil`", span));
                    return Err(());
                }
                let first = self.parse_expr()?;
                if self.eat_sym(",") {
                    let mut elems = vec![first];
                    while !self.is_sym(")") {
                        elems.push(self.parse_expr()?);
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.no_block = saved;
                    self.expect_sym(")")?;
                    return Ok(self.mk(ExprKind::Tuple(elems), start.to(self.prev_span())));
                }
                self.no_block = saved;
                self.expect_sym(")")?;
                let mut e = first;
                e.span = start.to(self.prev_span());
                Ok(e)
            }
            Tok::Sym("[") => self.parse_list_or_dict(),
            Tok::Sym("{") => {
                if matches!(self.peek_tok(1), Tok::Sym("|") | Tok::Sym("||")) {
                    return self.parse_block_arg();
                }
                let b = self.parse_block()?;
                let span = b.span;
                Ok(self.mk(ExprKind::Block(b), span))
            }
            Tok::Kw(Kw::Fn) => {
                self.bump();
                self.expect_sym("(")?;
                let mut params = Vec::new();
                while !self.is_sym(")") {
                    let name = self.expect_ident()?;
                    let ty = if self.eat_sym(":") { Some(self.parse_type()?) } else { None };
                    params.push(ClosureParam { name, ty });
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                let ret = if self.eat_sym("->") { Some(self.parse_type()?) } else { None };
                let body = self.parse_block()?;
                let span = start.to(body.span);
                Ok(self.mk(ExprKind::Closure { params, ret, body, fn_syntax: true }, span))
            }
            Tok::Kw(Kw::If) => self.parse_if(),
            Tok::Kw(Kw::While) => {
                self.bump();
                let cond = self.parse_cond()?;
                let body = self.parse_block()?;
                let span = start.to(body.span);
                Ok(self.mk(ExprKind::While { cond: Box::new(cond), body }, span))
            }
            Tok::Kw(Kw::Loop) => {
                self.bump();
                let body = self.parse_block()?;
                let span = start.to(body.span);
                Ok(self.mk(ExprKind::Loop { body }, span))
            }
            Tok::Kw(Kw::For) => {
                self.bump();
                let pattern = self.parse_pattern()?;
                if !self.eat_kw(Kw::In) {
                    return self.error("expected `in` after the loop variable");
                }
                let iter = self.parse_cond()?;
                let body = self.parse_block()?;
                let span = start.to(body.span);
                Ok(self.mk(ExprKind::For { pattern, iter: Box::new(iter), body }, span))
            }
            Tok::Kw(Kw::Match) => self.parse_match(),
            Tok::Kw(Kw::Return) => {
                self.bump();
                let value = if self.expr_follows() { Some(Box::new(self.parse_expr()?)) } else { None };
                let span = start.to(self.prev_span());
                Ok(self.mk(ExprKind::Return(value), span))
            }
            Tok::Kw(Kw::Break) => {
                self.bump();
                let value = if self.expr_follows() { Some(Box::new(self.parse_expr()?)) } else { None };
                let span = start.to(self.prev_span());
                Ok(self.mk(ExprKind::Break(value), span))
            }
            Tok::Kw(Kw::Continue) => {
                self.bump();
                Ok(self.mk(ExprKind::Continue, start))
            }
            Tok::Kw(Kw::Spawn) => {
                self.bump();
                let is_move = self.eat_kw(Kw::Move);
                let body = self.parse_block()?;
                let span = start.to(body.span);
                Ok(self.mk(ExprKind::Spawn { is_move, body }, span))
            }
            Tok::Kw(Kw::Move) => {
                self.bump();
                let e = self.parse_postfix()?;
                let span = start.to(e.span);
                Ok(self.mk(ExprKind::Move(Box::new(e)), span))
            }
            Tok::Kw(Kw::With) => {
                self.bump();
                match self.tok() {
                    Tok::Ident(n) if n == "lock" => {
                        self.bump();
                    }
                    _ => return self.error("expected `lock(...)` after `with`"),
                }
                self.expect_sym("(")?;
                let obj = self.parse_expr()?;
                self.expect_sym(")")?;
                let body = self.parse_block()?;
                let span = start.to(body.span);
                Ok(self.mk(ExprKind::WithLock { obj: Box::new(obj), body }, span))
            }
            Tok::Sym(".") => {
                // `.Variant` shorthand in expression position (enum constructors).
                self.bump();
                let name = self.expect_ident()?;
                let span = start.to(name.span);
                let recv = self.mk(ExprKind::Ident("_".into()), start);
                Ok(self.mk(
                    ExprKind::Member {
                        recv: Box::new(recv),
                        name,
                        args: vec![],
                        block: None,
                        parens: false,
                        safe: false,
                    },
                    span,
                ))
            }
            _ => {
                let what = self.describe();
                self.error(format!("expected an expression, found {what}"))
            }
        }
    }

    /// True if an expression can start at the current token (for `return`/`break` values).
    fn expr_follows(&self) -> bool {
        !matches!(self.tok(), Tok::Newline | Tok::Eof | Tok::Sym("}") | Tok::Sym(";") | Tok::Sym(")") | Tok::Sym(","))
            && !matches!(self.tok(), Tok::Kw(Kw::If) if false)
    }

    fn parse_cond(&mut self) -> PResult<Expr> {
        let saved = self.no_block;
        self.no_block = true;
        let r = self.parse_expr();
        self.no_block = saved;
        r
    }

    fn parse_if(&mut self) -> PResult<Expr> {
        let start = self.bump().span; // if
        if self.eat_kw(Kw::Let) {
            let pattern = self.parse_pattern()?;
            self.expect_sym("=")?;
            let value = self.parse_cond()?;
            let then = self.parse_block()?;
            let else_ = self.parse_else()?;
            let span = start.to(self.prev_span());
            return Ok(self.mk(ExprKind::IfLet { pattern, value: Box::new(value), then, else_ }, span));
        }
        let cond = self.parse_cond()?;
        let then = self.parse_block()?;
        let else_ = self.parse_else()?;
        let span = start.to(self.prev_span());
        Ok(self.mk(ExprKind::If { cond: Box::new(cond), then, else_ }, span))
    }

    fn parse_else(&mut self) -> PResult<Option<Box<Expr>>> {
        // Allow `}\nelse {`.
        let save = self.pos;
        while self.is_newline() {
            self.bump();
        }
        if !self.eat_kw(Kw::Else) {
            self.pos = save;
            return Ok(None);
        }
        if self.is_kw(Kw::If) {
            return Ok(Some(Box::new(self.parse_if()?)));
        }
        let b = self.parse_block()?;
        let span = b.span;
        Ok(Some(Box::new(self.mk(ExprKind::Block(b), span))))
    }

    fn parse_match(&mut self) -> PResult<Expr> {
        let start = self.bump().span;
        let subject = self.parse_cond()?;
        self.expect_sym("{")?;
        let saved = self.no_block;
        self.no_block = false;
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.is_sym("}") && !self.at_eof() {
            let astart = self.span();
            let r = (|| -> PResult<()> {
                let pattern = self.parse_pattern()?;
                let guard = if self.eat_kw(Kw::If) { Some(self.parse_expr()?) } else { None };
                self.expect_sym("=>")?;
                let body = self.parse_stmt()?;
                arms.push(MatchArm { pattern, guard, body, span: astart.to(self.prev_span()) });
                if !self.eat_sym(",") && !self.is_sym("}") {
                    self.end_stmt()?;
                }
                Ok(())
            })();
            if r.is_err() {
                self.recover();
            }
            self.skip_newlines();
        }
        self.no_block = saved;
        self.expect_sym("}")?;
        let span = start.to(self.prev_span());
        Ok(self.mk(ExprKind::Match { subject: Box::new(subject), arms }, span))
    }

    fn parse_list_or_dict(&mut self) -> PResult<Expr> {
        let start = self.bump().span; // [
        let saved = self.no_block;
        self.no_block = false;
        let r = (|| {
            if self.is_sym(":") && matches!(self.peek_tok(1), Tok::Sym("]")) {
                self.bump();
                self.bump();
                return Ok(self.mk(ExprKind::Dict(vec![]), start.to(self.prev_span())));
            }
            if self.eat_sym("]") {
                return Ok(self.mk(ExprKind::List(vec![]), start.to(self.prev_span())));
            }
            let first_key = self.parse_dict_key()?;
            if self.eat_sym(":") {
                let mut entries = Vec::new();
                let v = self.parse_expr()?;
                entries.push((first_key, v));
                while self.eat_sym(",") {
                    if self.is_sym("]") {
                        break;
                    }
                    let k = self.parse_dict_key()?;
                    self.expect_sym(":")?;
                    let v = self.parse_expr()?;
                    entries.push((k, v));
                }
                self.expect_sym("]")?;
                return Ok(self.mk(ExprKind::Dict(entries), start.to(self.prev_span())));
            }
            let mut elems = vec![first_key];
            while self.eat_sym(",") {
                if self.is_sym("]") {
                    break;
                }
                elems.push(self.parse_expr()?);
            }
            self.expect_sym("]")?;
            Ok(self.mk(ExprKind::List(elems), start.to(self.prev_span())))
        })();
        self.no_block = saved;
        r
    }

    /// A dict key: a bare identifier followed by `:` is a string key.
    fn parse_dict_key(&mut self) -> PResult<Expr> {
        if let Tok::Ident(name) = self.tok().clone() {
            if matches!(self.peek_tok(1), Tok::Sym(":")) {
                let span = self.bump().span;
                return Ok(self.mk(ExprKind::Str(vec![StrPart::Lit(name)]), span));
            }
        }
        self.parse_expr()
    }

    // ---- patterns ----------------------------------------------------------------------------

    pub fn parse_pattern(&mut self) -> PResult<Pattern> {
        self.nested(Self::parse_pattern_inner)
    }

    fn parse_pattern_inner(&mut self) -> PResult<Pattern> {
        let first = self.parse_pattern_single()?;
        if !self.is_sym("|") {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat_sym("|") {
            alts.push(self.parse_pattern_single()?);
        }
        let span = alts[0].span.to(alts.last().unwrap().span);
        Ok(Pattern { kind: PatternKind::Or(alts), span, id: self.id() })
    }

    fn literal_pattern_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        let neg = self.eat_sym("-");
        let e = match self.tok().clone() {
            Tok::Int(s) => {
                self.bump();
                self.mk(ExprKind::Int(if neg { format!("-{s}") } else { s }), start.to(self.prev_span()))
            }
            Tok::Float(f) => {
                self.bump();
                self.mk(ExprKind::Float(if neg { -f } else { f }), start.to(self.prev_span()))
            }
            Tok::Str(p) if !neg => {
                self.bump();
                self.parse_string(p, start)?
            }
            Tok::Kw(Kw::True) if !neg => {
                self.bump();
                self.mk(ExprKind::Bool(true), start)
            }
            Tok::Kw(Kw::False) if !neg => {
                self.bump();
                self.mk(ExprKind::Bool(false), start)
            }
            Tok::Kw(Kw::Nil) if !neg => {
                self.bump();
                self.mk(ExprKind::Nil, start)
            }
            _ => {
                let what = self.describe();
                return self.error(format!("expected a pattern, found {what}"));
            }
        };
        Ok(e)
    }

    fn parse_pattern_single(&mut self) -> PResult<Pattern> {
        let start = self.span();
        let kind = match self.tok().clone() {
            Tok::Ident(name) if name == "_" => {
                self.bump();
                PatternKind::Wildcard
            }
            Tok::Ident(name) if name.starts_with(|c: char| c.is_lowercase() || c == '_') => {
                let ident = self.expect_ident()?;
                if self.eat_sym(":") {
                    let ty = self.parse_type_no_union()?;
                    PatternKind::Typed { name: ident, ty }
                } else {
                    PatternKind::Bind(ident)
                }
            }
            Tok::Ident(_) => {
                let mut path = vec![self.expect_ident()?];
                while self.is_sym(".") && matches!(self.peek_tok(1), Tok::Ident(_)) {
                    self.bump();
                    path.push(self.expect_ident()?);
                }
                let (args, has_parens) = self.parse_pattern_args()?;
                // Generic type arguments in a type pattern: `List[Int]`.
                PatternKind::Ctor { path, args, has_parens, shorthand: false }
            }
            Tok::Sym(".") => {
                self.bump();
                let name = self.expect_ident()?;
                let (args, has_parens) = self.parse_pattern_args()?;
                PatternKind::Ctor { path: vec![name], args, has_parens, shorthand: true }
            }
            Tok::Sym("(") => {
                self.bump();
                let mut elems = Vec::new();
                while !self.is_sym(")") {
                    elems.push(self.parse_pattern()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                if elems.len() == 1 {
                    return Ok(elems.pop().unwrap());
                }
                PatternKind::Tuple(elems)
            }
            _ => {
                let lo = self.literal_pattern_expr()?;
                if self.is_sym("..") || self.is_sym("...") {
                    let exclusive = self.is_sym("...");
                    self.bump();
                    let hi = self.literal_pattern_expr()?;
                    PatternKind::Range { lo: Box::new(lo), hi: Box::new(hi), exclusive }
                } else {
                    PatternKind::Literal(Box::new(lo))
                }
            }
        };
        Ok(Pattern { kind, span: start.to(self.prev_span()), id: self.id() })
    }

    fn parse_pattern_args(&mut self) -> PResult<(Vec<Pattern>, bool)> {
        if !(self.is_sym("(") && !self.toks[self.pos].space_before) {
            return Ok((Vec::new(), false));
        }
        self.bump();
        let mut args = Vec::new();
        while !self.is_sym(")") {
            args.push(self.parse_pattern()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok((args, true))
    }
}

/// E0901: the input nests deeper than the compiler supports.
fn too_deep(span: Span, what: &str, limit: u32) -> Diagnostic {
    Diagnostic::error("E0901", format!("expression {what} too deeply (the limit is {limit} levels)"), span)
        .note("split it up, for example by assigning parts to local variables")
}

/// Parses a whole file.
pub fn parse(src: &str, file: u32, next_id: &mut u32) -> (Module, Vec<Diagnostic>) {
    let (toks, mut diags) = Lexer::new(src, file, 0).tokenize();
    let mut p = Parser::new(toks, file, next_id);
    let mut m = p.parse_module();
    diags.extend(p.diags);
    // Each nesting level adds its chains' links and a few nodes (a statement, `not`, unary
    // operators, the primary) to a path through the tree.
    if (p.max_nesting + 1).saturating_mul(p.max_chain + 8) > MAX_DEPTH {
        limit_depth(&mut m, &mut diags);
    }
    (m, diags)
}

/// Replaces each statement or initializer whose tree is deeper than [`MAX_DEPTH`] with an error
/// node, reporting E0901 at it, so that no later pass recurses that deep. The depth is measured
/// without recursion.
fn limit_depth(m: &mut Module, diags: &mut Vec<Diagnostic>) {
    fn depth(root: &Expr) -> u32 {
        let mut max = 0;
        let mut stack = vec![(root, 1u32)];
        while let Some((e, d)) = stack.pop() {
            max = max.max(d);
            if d > MAX_DEPTH {
                break;
            }
            stack.extend(crate::query::children(e).into_iter().map(|c| (c, d + 1)));
        }
        max
    }
    fn check(e: &mut Expr, diags: &mut Vec<Diagnostic>) {
        if depth(e) > MAX_DEPTH {
            diags.push(too_deep(e.span, "nested", MAX_DEPTH));
            let deep = std::mem::replace(&mut e.kind, ExprKind::Error);
            drop_iteratively(deep);
        }
    }
    fn block(b: &mut Block, diags: &mut Vec<Diagnostic>) {
        b.stmts.iter_mut().for_each(|e| check(e, diags));
    }
    fn func(f: &mut FnDecl, diags: &mut Vec<Diagnostic>) {
        for p in &mut f.params {
            if let Some(d) = &mut p.default {
                check(d, diags);
            }
        }
        if let Some(b) = &mut f.body {
            block(b, diags);
        }
    }
    for it in &mut m.items {
        match it {
            Item::Fn(f) => func(f, diags),
            Item::Class(c) => {
                for f in &mut c.fields {
                    if let Some(d) = &mut f.default {
                        check(d, diags);
                    }
                }
                c.consts.iter_mut().for_each(|k| check(&mut k.value, diags));
                c.methods.iter_mut().for_each(|f| func(f, diags));
            }
            Item::Interface(i) => i.methods.iter_mut().for_each(|f| func(f, diags)),
            Item::Enum(e) => e.methods.iter_mut().for_each(|f| func(f, diags)),
            Item::Extend(x) => x.methods.iter_mut().for_each(|f| func(f, diags)),
            Item::Const(c) => check(&mut c.value, diags),
            Item::Var(v) => check(&mut v.value, diags),
            Item::Stmt(e) => check(e, diags),
            Item::Import(_) => {}
        }
    }
}

/// Drops a deep tree without recursing: each node's sub-expressions are taken out and dropped
/// from a work list. (Patterns and types inside it nest only as deep as the parser recursed.)
fn drop_iteratively(kind: ExprKind) {
    fn block(b: Block, out: &mut Vec<ExprKind>) {
        out.extend(b.stmts.into_iter().map(|e| e.kind));
    }
    fn args(a: Vec<Arg>, out: &mut Vec<ExprKind>) {
        out.extend(a.into_iter().map(|a| a.value.kind));
    }
    let mut stack = vec![kind];
    while let Some(k) = stack.pop() {
        let out = &mut stack;
        match k {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Nil
            | ExprKind::Ident(_)
            | ExprKind::SelfValue
            | ExprKind::Continue
            | ExprKind::Error => {}
            ExprKind::Str(parts) => {
                for p in parts {
                    if let StrPart::Expr(x) | StrPart::Fmt(x, ..) = p {
                        out.push(x.kind);
                    }
                }
            }
            ExprKind::List(xs) | ExprKind::Tuple(xs) => out.extend(xs.into_iter().map(|x| x.kind)),
            ExprKind::Dict(kvs) => {
                for (k, v) in kvs {
                    out.push(k.kind);
                    out.push(v.kind);
                }
            }
            ExprKind::Unary(_, x)
            | ExprKind::Unwrap(x)
            | ExprKind::Try(x)
            | ExprKind::Move(x)
            | ExprKind::Is { expr: x, .. }
            | ExprKind::AsCast { expr: x, .. } => out.push(x.kind),
            ExprKind::Binary(_, a, b) | ExprKind::Range { lo: a, hi: b, .. } => {
                out.push(a.kind);
                out.push(b.kind);
            }
            ExprKind::Call { callee: recv, args: a, block: blk }
            | ExprKind::Member { recv, args: a, block: blk, .. } => {
                out.push(recv.kind);
                args(a, out);
                if let Some(b) = blk {
                    out.push(b.kind);
                }
            }
            ExprKind::Index { recv, args: xs } => {
                out.push(recv.kind);
                out.extend(xs.into_iter().map(|x| x.kind));
            }
            ExprKind::Closure { body, .. }
            | ExprKind::Loop { body }
            | ExprKind::Block(body)
            | ExprKind::Spawn { body, .. } => block(body, out),
            ExprKind::If { cond: x, then, else_ } | ExprKind::IfLet { value: x, then, else_, .. } => {
                out.push(x.kind);
                block(then, out);
                if let Some(e) = else_ {
                    out.push(e.kind);
                }
            }
            ExprKind::While { cond: x, body }
            | ExprKind::For { iter: x, body, .. }
            | ExprKind::WithLock { obj: x, body } => {
                out.push(x.kind);
                block(body, out);
            }
            ExprKind::Match { subject, arms } => {
                out.push(subject.kind);
                for a in arms {
                    if let Some(g) = a.guard {
                        out.push(g.kind);
                    }
                    out.push(a.body.kind);
                }
            }
            ExprKind::Return(x) | ExprKind::Break(x) => {
                if let Some(x) = x {
                    out.push(x.kind);
                }
            }
            ExprKind::Assign { target, value, .. } => {
                out.push(target.kind);
                out.push(value.kind);
            }
            ExprKind::Let { value, .. } => out.push(value.kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Module {
        let mut id = 0;
        let (m, d) = parse(src, 0, &mut id);
        assert!(d.is_empty(), "{src}\n{d:#?}");
        m
    }

    #[test]
    fn parses_spec_sample() {
        ok(r#"
interface Named { fn name(self) -> String }

class User {
  full_name: String
  email: String?
  fn name(self) -> String { self.full_name }
}

pub fn greet(x: Named) -> String { "Hello, #{x.name()}" }

fn shout(x) { x.name().upcase() }        # private: x's interface is inferred

users = load_users()                     # List[User], inferred
emails = users.map { |u| u.email ?? "none" }
pub fn total(xs: Enumerable[Int]) -> Int { xs.reduce(0) { |a, b| a + b } }
pub fn log_all(items: Enumerable[Display & Hashable]) { 1 }
d = [host: "localhost", port: 8080]
e = [:]
"#);
    }

    #[test]
    fn control_flow_and_patterns() {
        ok(r#"
fn f(s: Shape) -> Float {
  match s {
    .Circle(r) => 3.14 * r * r
    Shape.Rect(w, h) if w > 0.0 => w * h
    _ => 0.0
  }
}
for (i, x) in xs.each_with_index() {
  if x > 3 and not done { break } else if x == 2 { continue }
}
if let u = find(1) { puts(u) }
x: Int? = nil
t = spawn move { work(data) }
r = d["k"] as? String ?? "none"
v = a.b?.c!.d
"#);
    }

    #[test]
    fn method_chains_across_lines() {
        let m = ok("xs\n  .map { |x| x * 2 }\n  .select { |x| x > 2 }\n");
        assert_eq!(m.items.len(), 1);
    }

    #[test]
    fn if_condition_does_not_take_block() {
        let m = ok("if xs.empty { 1 } else { 2 }\n");
        match &m.items[0] {
            Item::Stmt(Expr { kind: ExprKind::If { cond, .. }, .. }) => {
                assert!(matches!(cond.kind, ExprKind::Member { block: None, .. }))
            }
            other => panic!("{other:?}"),
        }
    }
}
