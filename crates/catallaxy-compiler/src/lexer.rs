//! Lexer: source text to tokens.
//!
//! Newlines are significant statement terminators except (a) inside `(` or `[`, (b) after a token
//! that cannot end an expression (binary operators, `,`, `=`, `->`, `.`), and (c) before a line that
//! starts with `.` or `?.` (method chains). String interpolations are kept as raw source pieces and
//! parsed by the parser with their original offsets.

use crate::diag::{Diagnostic, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Int(String),
    Float(f64),
    Str(Vec<StrPiece>),
    Ident(String),
    Kw(Kw),
    Sym(&'static str),
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrPiece {
    Lit(String),
    /// Interpolated source text and its byte offset in the file.
    Code(String, u32),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kw {
    And,
    As,
    Break,
    Class,
    Const,
    Continue,
    Else,
    Enum,
    Extend,
    False,
    Fn,
    For,
    Frozen,
    If,
    Import,
    In,
    Interface,
    Is,
    Let,
    Loop,
    Match,
    Move,
    Mut,
    Native,
    Nil,
    Not,
    Or,
    Pub,
    Return,
    SelfValue,
    SelfType,
    Shared,
    Spawn,
    True,
    Var,
    While,
    With,
}

pub fn keyword(s: &str) -> Option<Kw> {
    Some(match s {
        "and" => Kw::And,
        "as" => Kw::As,
        "break" => Kw::Break,
        "class" => Kw::Class,
        "const" => Kw::Const,
        "continue" => Kw::Continue,
        "else" => Kw::Else,
        "enum" => Kw::Enum,
        "extend" => Kw::Extend,
        "false" => Kw::False,
        "fn" => Kw::Fn,
        "for" => Kw::For,
        "frozen" => Kw::Frozen,
        "if" => Kw::If,
        "import" => Kw::Import,
        "in" => Kw::In,
        "interface" => Kw::Interface,
        "is" => Kw::Is,
        "let" => Kw::Let,
        "loop" => Kw::Loop,
        "match" => Kw::Match,
        "move" => Kw::Move,
        "mut" => Kw::Mut,
        "native" => Kw::Native,
        "nil" => Kw::Nil,
        "not" => Kw::Not,
        "or" => Kw::Or,
        "pub" => Kw::Pub,
        "return" => Kw::Return,
        "self" => Kw::SelfValue,
        "Self" => Kw::SelfType,
        "shared" => Kw::Shared,
        "spawn" => Kw::Spawn,
        "true" => Kw::True,
        "var" => Kw::Var,
        "while" => Kw::While,
        "with" => Kw::With,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// True if whitespace or a newline precedes this token.
    pub space_before: bool,
}

/// A `#` comment (or the `#!` shebang line), collected for tooling such as the formatter.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub span: Span,
    /// The comment text including the leading `#`, without the line terminator.
    pub text: String,
}

/// Operators, longest first.
pub(crate) const SYMBOLS: &[&str] = &[
    "...", "<=>", "**=", "<<=", ">>=", "??=", "||=", "&&=", "==", "!=", "<=", ">=", "&&", "||", "??", "?.", "->", "=>",
    "..", "+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=", "<<", ">>", "**", "::", "+", "-", "*", "/", "%", "=", "<",
    ">", "!", "?", ".", ",", ":", ";", "(", ")", "[", "]", "{", "}", "|", "&", "^", "~", "@",
];

/// Tokens after which a newline does not end the statement.
pub(crate) fn continues_line(t: &Tok) -> bool {
    match t {
        Tok::Sym(s) => matches!(
            *s,
            "+" | "-"
                | "*"
                | "/"
                | "%"
                | "**"
                | "=="
                | "!="
                | "<"
                | "<="
                | ">"
                | ">="
                | "<=>"
                | "&&"
                | "||"
                | "??"
                | "="
                | "+="
                | "-="
                | "*="
                | "/="
                | "%="
                | "**="
                | "<<="
                | ">>="
                | "??="
                | "||="
                | "&&="
                | "|="
                | "&="
                | "^="
                | "->"
                | "=>"
                | ","
                | "."
                | "?."
                | "&"
                | "^"
                | "<<"
                | ">>"
                | ".."
                | "..."
                | "::"
        ),
        Tok::Kw(k) => matches!(k, Kw::And | Kw::Or | Kw::Not | Kw::In | Kw::Is | Kw::As),
        _ => false,
    }
}

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    file: u32,
    base: u32,
    out: Vec<Token>,
    brackets: Vec<u8>,
    pub diags: Vec<Diagnostic>,
    /// Comments seen so far, in source order. The parser ignores them.
    pub comments: Vec<Comment>,
}

impl<'a> Lexer<'a> {
    /// Lexes `src`, whose first byte sits at offset `base` of file `file`.
    pub fn new(src: &'a str, file: u32, base: u32) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            file,
            base,
            out: Vec::new(),
            brackets: Vec::new(),
            diags: Vec::new(),
            comments: Vec::new(),
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, self.base + start as u32, self.base + end as u32)
    }

    fn err(&mut self, start: usize, end: usize, code: &'static str, msg: impl Into<String>) {
        self.diags.push(Diagnostic::error(code, msg, self.span(start, end)));
    }

    fn peek(&self) -> u8 {
        *self.bytes.get(self.pos).unwrap_or(&0)
    }
    fn peek_at(&self, n: usize) -> u8 {
        *self.bytes.get(self.pos + n).unwrap_or(&0)
    }

    fn push(&mut self, tok: Tok, start: usize, space_before: bool) {
        let span = self.span(start, self.pos);
        self.out.push(Token { tok, span, space_before });
    }

    fn newlines_significant(&self) -> bool {
        !matches!(self.brackets.last(), Some(b'(') | Some(b'['))
    }

    pub fn tokenize(mut self) -> (Vec<Token>, Vec<Diagnostic>) {
        self.run();
        (self.out, self.diags)
    }

    /// Like [`Lexer::tokenize`], but also returns the comments.
    pub fn tokenize_with_comments(mut self) -> (Vec<Token>, Vec<Diagnostic>, Vec<Comment>) {
        self.run();
        (self.out, self.diags, self.comments)
    }

    fn comment(&mut self) {
        let start = self.pos;
        while self.pos < self.bytes.len() && self.peek() != b'\n' {
            self.pos += 1;
        }
        let text = self.src[start..self.pos].trim_end_matches('\r').to_string();
        let span = self.span(start, start + text.len());
        self.comments.push(Comment { span, text });
    }

    fn run(&mut self) {
        if self.src.starts_with("#!") {
            self.comment();
        }
        let mut space = true;
        loop {
            // Whitespace and comments.
            let mut saw_newline = false;
            let mut newline_at = 0;
            loop {
                match self.peek() {
                    b' ' | b'\t' | b'\r' => {
                        self.pos += 1;
                        space = true;
                    }
                    b'\n' => {
                        if !saw_newline {
                            newline_at = self.pos;
                        }
                        saw_newline = true;
                        self.pos += 1;
                        space = true;
                    }
                    b'#' => self.comment(),
                    b'\\' if self.peek_at(1) == b'\n' => {
                        // Explicit line continuation.
                        self.pos += 2;
                        space = true;
                    }
                    _ => break,
                }
            }
            if saw_newline && self.newlines_significant() {
                let cont_prev = self.out.last().map(|t| continues_line(&t.tok)).unwrap_or(true);
                // `.method` continues a chain; `.Variant` (uppercase) starts a pattern or value.
                let next_is_chain =
                    (self.peek() == b'.' && self.peek_at(1) != b'.' && !self.peek_at(1).is_ascii_uppercase())
                        || (self.peek() == b'?' && self.peek_at(1) == b'.');
                let prev_is_nl = matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None);
                if !cont_prev && !next_is_chain && !prev_is_nl {
                    let span = self.span(newline_at, newline_at + 1);
                    self.out.push(Token { tok: Tok::Newline, span, space_before: true });
                }
            }
            if self.pos >= self.bytes.len() {
                let span = self.span(self.pos, self.pos);
                if !matches!(self.out.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
                    self.out.push(Token { tok: Tok::Newline, span, space_before: true });
                }
                self.out.push(Token { tok: Tok::Eof, span, space_before: true });
                return;
            }
            let start = self.pos;
            let c = self.peek();
            if c.is_ascii_digit() {
                self.number(space);
            } else if c == b'_'
                || c.is_ascii_alphabetic()
                || (c >= 0x80 && self.src[self.pos..].chars().next().is_some_and(|ch| ch.is_alphabetic()))
            {
                self.ident(space);
            } else if c == b'"' {
                self.string(space);
            } else if c == b'\'' {
                self.raw_string(space);
            } else {
                let rest = &self.src[self.pos..];
                match SYMBOLS.iter().find(|s| rest.starts_with(**s)) {
                    Some(sym) => {
                        self.pos += sym.len();
                        match *sym {
                            "(" | "[" | "{" => self.brackets.push(sym.as_bytes()[0]),
                            ")" | "]" | "}" => {
                                self.brackets.pop();
                            }
                            _ => {}
                        }
                        self.push(Tok::Sym(sym), start, space);
                    }
                    None => {
                        let ch = self.src[self.pos..].chars().next().unwrap();
                        self.pos += ch.len_utf8();
                        self.err(start, self.pos, "E0001", format!("unexpected character `{ch}`"));
                    }
                }
            }
            space = false;
        }
    }

    fn number(&mut self, space: bool) {
        let start = self.pos;
        let radix = if self.peek() == b'0' {
            match self.peek_at(1) {
                b'x' | b'X' => 16,
                b'b' | b'B' => 2,
                b'o' | b'O' => 8,
                _ => 10,
            }
        } else {
            10
        };
        if radix != 10 {
            self.pos += 2;
            let ds = self.pos;
            while self.peek().is_ascii_hexdigit() || self.peek() == b'_' {
                self.pos += 1;
            }
            let digits: String = self.src[ds..self.pos].chars().filter(|&c| c != '_').collect();
            match num_bigint::BigInt::parse_bytes(digits.as_bytes(), radix) {
                Some(n) => self.push(Tok::Int(n.to_string()), start, space),
                None => {
                    self.err(start, self.pos, "E0002", "invalid integer literal");
                    self.push(Tok::Int("0".into()), start, space);
                }
            }
            return;
        }
        while self.peek().is_ascii_digit() || (self.peek() == b'_' && self.peek_at(1).is_ascii_digit()) {
            self.pos += 1;
        }
        let mut is_float = false;
        // `1..2` is a range, `1.foo` a method call; only `1.5` is a float.
        if self.peek() == b'.' && self.peek_at(1).is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            while self.peek().is_ascii_digit() || (self.peek() == b'_' && self.peek_at(1).is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), b'e' | b'E')
            && (self.peek_at(1).is_ascii_digit()
                || (matches!(self.peek_at(1), b'+' | b'-') && self.peek_at(2).is_ascii_digit()))
        {
            is_float = true;
            self.pos += 2;
            while self.peek().is_ascii_digit() {
                self.pos += 1;
            }
        }
        let text: String = self.src[start..self.pos].chars().filter(|&c| c != '_').collect();
        if is_float {
            self.push(Tok::Float(text.parse().unwrap_or(0.0)), start, space);
        } else {
            self.push(Tok::Int(text), start, space);
        }
    }

    fn ident(&mut self, space: bool) {
        let start = self.pos;
        while let Some(ch) = self.src[self.pos..].chars().next() {
            if ch == '_' || ch.is_alphanumeric() {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        let tok = match keyword(text) {
            Some(k) => Tok::Kw(k),
            None => Tok::Ident(text.to_string()),
        };
        self.push(tok, start, space);
    }

    fn escape(&mut self, out: &mut String) {
        // At the backslash.
        let start = self.pos;
        self.pos += 1;
        let c = self.peek();
        self.pos += 1;
        match c {
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'r' => out.push('\r'),
            b'0' => out.push('\0'),
            b'\\' => out.push('\\'),
            b'"' => out.push('"'),
            b'\'' => out.push('\''),
            b'#' => out.push('#'),
            b'e' => out.push('\x1b'),
            b'u' => {
                if self.peek() == b'{' {
                    let hs = self.pos + 1;
                    let mut he = hs;
                    while he < self.bytes.len() && self.bytes[he] != b'}' {
                        he += 1;
                    }
                    let ch = u32::from_str_radix(&self.src[hs..he], 16).ok().and_then(char::from_u32);
                    self.pos = (he + 1).min(self.bytes.len());
                    match ch {
                        Some(ch) => out.push(ch),
                        None => self.err(start, self.pos, "E0003", "invalid unicode escape"),
                    }
                } else {
                    self.err(start, self.pos, "E0003", "expected `{` after `\\u`");
                }
            }
            _ => {
                self.err(start, self.pos, "E0003", format!("unknown escape `\\{}`", c as char));
            }
        }
    }

    fn string(&mut self, space: bool) {
        let start = self.pos;
        self.pos += 1;
        let mut pieces = Vec::new();
        let mut cur = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.err(start, self.pos, "E0004", "unterminated string literal");
                break;
            }
            match self.peek() {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => self.escape(&mut cur),
                b'#' if self.peek_at(1) == b'{' => {
                    if !cur.is_empty() {
                        pieces.push(StrPiece::Lit(std::mem::take(&mut cur)));
                    }
                    self.pos += 2;
                    let code_start = self.pos;
                    let mut depth = 1;
                    // Skip nested braces and strings inside the interpolation.
                    while self.pos < self.bytes.len() {
                        match self.peek() {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            b'"' => {
                                self.pos += 1;
                                while self.pos < self.bytes.len() && self.peek() != b'"' {
                                    if self.peek() == b'\\' {
                                        self.pos += 1;
                                    }
                                    self.pos += 1;
                                }
                            }
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    if self.pos >= self.bytes.len() {
                        self.err(start, self.pos, "E0004", "unterminated interpolation");
                        break;
                    }
                    let code = self.src[code_start..self.pos].to_string();
                    pieces.push(StrPiece::Code(code, self.base + code_start as u32));
                    self.pos += 1;
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    cur.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        if !cur.is_empty() || pieces.is_empty() {
            pieces.push(StrPiece::Lit(cur));
        }
        self.push(Tok::Str(pieces), start, space);
    }

    fn raw_string(&mut self, space: bool) {
        let start = self.pos;
        self.pos += 1;
        let mut s = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.err(start, self.pos, "E0004", "unterminated string literal");
                break;
            }
            match self.peek() {
                b'\'' => {
                    self.pos += 1;
                    break;
                }
                b'\\' if matches!(self.peek_at(1), b'\'' | b'\\') => {
                    s.push(self.peek_at(1) as char);
                    self.pos += 2;
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    s.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        self.push(Tok::Str(vec![StrPiece::Lit(s)]), start, space);
    }
}

pub fn lex(src: &str, file: u32) -> (Vec<Token>, Vec<Diagnostic>) {
    Lexer::new(src, file, 0).tokenize()
}

/// Lexes a whole file, keeping its comments.
pub fn lex_with_comments(src: &str, file: u32) -> (Vec<Token>, Vec<Diagnostic>, Vec<Comment>) {
    Lexer::new(src, file, 0).tokenize_with_comments()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        lex(s, 0).0.into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn newline_rules() {
        let t = toks("a = 1 +\n  2\nb = foo(1,\n 2)\nc\n  .d\n");
        let nl = t.iter().filter(|t| **t == Tok::Newline).count();
        assert_eq!(nl, 3, "{t:?}");
    }

    #[test]
    fn numbers_and_ranges() {
        let t = toks("1..2 1.5 3.times 0xff 1_000 2e3");
        assert_eq!(t[0], Tok::Int("1".into()));
        assert_eq!(t[1], Tok::Sym(".."));
        assert_eq!(t[3], Tok::Float(1.5));
        assert_eq!(t[4], Tok::Int("3".into()));
        assert_eq!(t[5], Tok::Sym("."));
        assert_eq!(t[7], Tok::Int("255".into()));
        assert_eq!(t[8], Tok::Int("1000".into()));
        assert_eq!(t[9], Tok::Float(2000.0));
    }

    #[test]
    fn non_letter_unicode_is_an_error_not_a_hang() {
        let (toks, diags) = lex("h\u{2e9}llo", 0);
        assert_eq!(diags.len(), 1);
        assert!(toks.len() >= 3);
    }

    #[test]
    fn collects_comments() {
        let (toks, _, comments) = lex_with_comments("#!/usr/bin/env catx\nx = 1 # one\n# two\r\ny\n", 0);
        let texts: Vec<&str> = comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["#!/usr/bin/env catx", "# one", "# two"]);
        assert_eq!(comments[1].span.start, 26);
        assert_eq!(toks.iter().filter(|t| t.tok == Tok::Newline).count(), 2);
    }

    #[test]
    fn interpolation() {
        let t = toks(r#""a #{b + "}"} c""#);
        match &t[0] {
            Tok::Str(p) => {
                assert_eq!(p.len(), 3);
                assert_eq!(p[1], StrPiece::Code("b + \"}\"".into(), 5));
            }
            other => panic!("{other:?}"),
        }
    }
}
