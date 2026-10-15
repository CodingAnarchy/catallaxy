//! Source formatter (`catx fmt`).
//!
//! The formatter works on the token stream plus the comments the lexer collects, not on the AST.
//! Newlines are significant in Catallaxy, so it keeps the source's line structure and only:
//!
//! - re-indents every line (two spaces per open bracket, one extra level for continuation lines),
//! - normalizes the spacing between tokens on a line,
//! - joins `}` / `else` onto one line,
//! - breaks a `{ ... }` block that makes its line longer than [`MAX_WIDTH`] columns,
//! - collapses runs of blank lines to one and drops blank lines just inside brackets,
//! - strips trailing whitespace and ends the file with exactly one newline.
//!
//! Token text (literals, strings with interpolations) is copied verbatim. As a safety net the output
//! is re-lexed and compared with the input: any difference other than statement separators is
//! reported as an internal error instead of returning changed code.

use std::collections::VecDeque;
use std::ops::Range;

use crate::diag::{Diagnostic, Span};
use crate::lexer::{self, continues_line, Comment, Kw, StrPiece, Tok, Token, SYMBOLS};
use crate::parser;

/// Lines longer than this have their `{ ... }` blocks broken across lines.
pub const MAX_WIDTH: usize = 100;

const INDENT: &str = "  ";

/// Formats a source file. Refuses (returning the diagnostics) if the file does not parse.
pub fn format_source(src: &str) -> Result<String, Vec<Diagnostic>> {
    crate::util::on_compiler_stack(|| format_on_this_stack(src))
}

fn format_on_this_stack(src: &str) -> Result<String, Vec<Diagnostic>> {
    let mut next_id = 0;
    let (_, diags) = parser::parse(src, 0, &mut next_id);
    let errors: Vec<Diagnostic> = diags.into_iter().filter(|d| d.is_error()).collect();
    if !errors.is_empty() {
        return Err(errors);
    }
    let (toks, _, comments) = lexer::lex_with_comments(src, 0);
    let out = Formatter::new(src, toks, comments).format();
    verify(src, &out)?;
    Ok(out)
}

// ---- token analysis -------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Role {
    Plain,
    /// Prefix `-`, `!`, `~`.
    Unary,
    /// Postfix `!` (unwrap) or `?` (try, optional type, `as?`).
    Postfix,
    /// The `|` opening closure parameters after `{`.
    PipeOpen,
    /// The `|` closing closure parameters.
    PipeClose,
    /// `||`: empty closure parameters after `{`.
    PipeEmpty,
    /// Part of an operator method name: `fn +(`, `fn [](`, `fn []=(`.
    OpName,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BraceKind {
    /// Statements: breaks at `;`.
    Block,
    /// `match` / `enum` bodies: may also break after `,`.
    List,
    /// `import a.{B, c}`: no padding inside the braces.
    Import,
}

/// Code tokens (no `Newline`/`Eof`) with the per-token facts spacing decisions need.
struct Analysis {
    toks: Vec<Token>,
    texts: Vec<String>,
    roles: Vec<Role>,
    /// A significant newline (statement boundary) precedes the token.
    nl_before: Vec<bool>,
    /// For brackets: the index of the matching bracket (or `usize::MAX`).
    matching: Vec<usize>,
    braces: Vec<BraceKind>,
}

fn sym(t: &Tok) -> Option<&'static str> {
    match t {
        Tok::Sym(s) => Some(s),
        _ => None,
    }
}

fn is_opener(t: &Tok) -> bool {
    matches!(sym(t), Some("(" | "[" | "{"))
}

fn is_closer(t: &Tok) -> bool {
    matches!(sym(t), Some(")" | "]" | "}"))
}

impl Analysis {
    fn new(src: &str, all: Vec<Token>) -> Self {
        let mut toks = Vec::with_capacity(all.len());
        let mut nl_before = Vec::with_capacity(all.len());
        let mut nl = true;
        for t in all {
            match t.tok {
                Tok::Newline | Tok::Eof => nl = true,
                _ => {
                    toks.push(t);
                    nl_before.push(std::mem::take(&mut nl));
                }
            }
        }
        let texts = toks.iter().map(|t| src[t.span.start as usize..t.span.end as usize].to_string()).collect();
        let n = toks.len();
        let mut a = Analysis {
            toks,
            texts,
            roles: vec![Role::Plain; n],
            nl_before,
            matching: vec![usize::MAX; n],
            braces: vec![BraceKind::Block; n],
        };
        a.analyze();
        a
    }

    fn tok(&self, i: usize) -> &Tok {
        &self.toks[i].tok
    }

    fn sym(&self, i: usize) -> Option<&'static str> {
        sym(self.tok(i))
    }

    fn tight(&self, i: usize) -> bool {
        !self.toks[i].space_before
    }

    /// True if token `i` can end an operand, so a following `(`, `[`, `!` or `?` may be postfix.
    fn operand_end(&self, i: usize) -> bool {
        match self.tok(i) {
            Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::Ident(_) => true,
            Tok::Kw(Kw::True | Kw::False | Kw::Nil | Kw::SelfValue | Kw::SelfType | Kw::Continue) => true,
            // A keyword used as a member name: `x.class`.
            Tok::Kw(_) => i > 0 && matches!(self.sym(i - 1), Some("." | "?.")),
            Tok::Sym(")" | "]" | "}") => true,
            Tok::Sym("!" | "?") => self.roles[i] == Role::Postfix,
            _ => self.roles[i] == Role::OpName,
        }
    }

    fn analyze(&mut self) {
        let n = self.toks.len();
        let mut stack: Vec<usize> = Vec::new();
        // Pending `match`/`enum` keyword per bracket depth: the next `{` at that depth is its body.
        let mut pending: Vec<bool> = vec![false];
        let mut i = 0;
        while i < n {
            let prev = i.checked_sub(1);
            let prev_sym = prev.and_then(|p| self.sym(p));
            match self.tok(i).clone() {
                Tok::Kw(Kw::Match | Kw::Enum) => *pending.last_mut().unwrap() = true,
                Tok::Kw(Kw::Fn) => {
                    // Operator method names.
                    let mut j = i + 1;
                    match self.toks.get(j).map(|t| &t.tok) {
                        Some(Tok::Sym("[")) => {
                            if matches!(self.toks.get(j + 1).map(|t| &t.tok), Some(Tok::Sym("]"))) {
                                self.roles[j] = Role::OpName;
                                self.roles[j + 1] = Role::OpName;
                                j += 2;
                                if j < n && self.sym(j) == Some("=") && self.tight(j) {
                                    self.roles[j] = Role::OpName;
                                }
                            }
                        }
                        Some(Tok::Sym(s)) if *s != "(" => self.roles[j] = Role::OpName,
                        _ => {}
                    }
                }
                Tok::Sym(s) if self.roles[i] == Role::Plain => match s {
                    "(" | "[" | "{" => {
                        if s == "{" {
                            if prev_sym == Some(".") {
                                self.braces[i] = BraceKind::Import;
                            } else if *pending.last().unwrap() {
                                self.braces[i] = BraceKind::List;
                            }
                            *pending.last_mut().unwrap() = false;
                            if i + 1 < n && self.sym(i + 1) == Some("||") {
                                self.roles[i + 1] = Role::PipeEmpty;
                            }
                            if i + 1 < n && self.sym(i + 1) == Some("|") {
                                self.roles[i + 1] = Role::PipeOpen;
                                let mut depth = 0i32;
                                for j in i + 2..n {
                                    match self.sym(j) {
                                        Some("(" | "[" | "{") => depth += 1,
                                        Some(")" | "]" | "}") => depth -= 1,
                                        Some("|") if depth == 0 => {
                                            self.roles[j] = Role::PipeClose;
                                            break;
                                        }
                                        _ => {}
                                    }
                                    if depth < 0 {
                                        break;
                                    }
                                }
                            }
                        }
                        stack.push(i);
                        pending.push(false);
                    }
                    ")" | "]" | "}" => {
                        if let Some(o) = stack.pop() {
                            self.matching[o] = i;
                            self.matching[i] = o;
                        }
                        if pending.len() > 1 {
                            pending.pop();
                        }
                    }
                    "-" | "~" if self.nl_before[i] || prev.is_none_or(|p| !self.operand_end(p)) => {
                        self.roles[i] = Role::Unary
                    }
                    "!" => {
                        self.roles[i] = if prev.is_some_and(|p| self.operand_end(p)) && self.tight(i) {
                            Role::Postfix
                        } else {
                            Role::Unary
                        }
                    }
                    "?" => {
                        let after_as = prev.is_some_and(|p| matches!(self.tok(p), Tok::Kw(Kw::As)));
                        if after_as || (prev.is_some_and(|p| self.operand_end(p)) && self.tight(i)) {
                            self.roles[i] = Role::Postfix;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            i += 1;
        }
    }

    /// Whether the parser's reading of token `i` depends on whether whitespace precedes it.
    fn tightness_matters(&self, i: usize) -> bool {
        matches!(self.sym(i), Some("(" | "[" | "!" | "?"))
            && i > 0
            && (self.operand_end(i - 1) || matches!(self.tok(i - 1), Tok::Kw(Kw::As)))
    }

    /// Whether a space goes between tokens `i - 1` and `i` when they share a line.
    fn space_before(&self, i: usize) -> bool {
        let want = self.space_rule(i);
        // Never glue two tokens into a different token (`x? .y` must not become `x?.y`).
        want || self.would_merge(i)
    }

    fn space_rule(&self, i: usize) -> bool {
        let a = i - 1;
        let (sa, sb) = (self.sym(a), self.sym(i));
        let (ra, rb) = (self.roles[a], self.roles[i]);
        if matches!(sb, Some(")" | "]" | "," | ";" | ":" | "::" | "?.")) {
            return false;
        }
        if sb == Some("}") {
            let open = self.matching[i];
            return sa != Some("{") && !(open != usize::MAX && self.braces[open] == BraceKind::Import);
        }
        if rb == Role::OpName {
            return ra != Role::OpName;
        }
        if matches!(sa, Some("(" | "[")) {
            return false;
        }
        if sa == Some("{") {
            return self.braces[a] != BraceKind::Import;
        }
        if matches!(sa, Some("." | "?." | "::")) {
            return false;
        }
        if sb == Some(".") {
            return !self.operand_end(a);
        }
        if matches!(sa, Some(".." | "...")) || matches!(sb, Some(".." | "...")) {
            return false;
        }
        if ra == Role::PipeOpen || rb == Role::PipeClose || ra == Role::Unary || rb == Role::Postfix {
            return false;
        }
        if ra == Role::OpName && (rb == Role::OpName || matches!(sb, Some("(" | "["))) {
            return false;
        }
        if matches!(sb, Some("(" | "[")) {
            // `fn(A) -> R`, `extend[T: Bound] Target`.
            if matches!(self.tok(a), Tok::Kw(Kw::Fn)) || (sb == Some("[") && matches!(self.tok(a), Tok::Kw(Kw::Extend)))
            {
                return false;
            }
            // A method name, including a keyword one: `fn loop(self)`.
            if matches!(self.tok(a), Tok::Ident(_) | Tok::Kw(_)) && a > 0 && matches!(self.tok(a - 1), Tok::Kw(Kw::Fn))
            {
                return false;
            }
            if self.operand_end(a) {
                return !self.tight(i);
            }
        }
        true
    }

    fn would_merge(&self, i: usize) -> bool {
        let (a, b) = (&self.texts[i - 1], &self.texts[i]);
        let word = |c: char| c == '_' || c.is_alphanumeric() || c == '"' || c == '\'';
        if a.ends_with(word) && b.starts_with(word) {
            return true;
        }
        if self.sym(i - 1).is_some() && self.sym(i).is_some() {
            let joined = format!("{a}{b}");
            let longest = SYMBOLS.iter().find(|s| joined.starts_with(**s)).map_or(0, |s| s.len());
            return longest != a.len();
        }
        false
    }
}

// ---- lines ----------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Line {
    /// Code tokens on the line (indices into `Analysis::toks`).
    toks: Range<usize>,
    /// Trailing or whole-line comment.
    comment: Option<usize>,
    /// Spaces between the code and a trailing comment in the source.
    comment_gap: usize,
    blank_before: bool,
    /// The source line ended with a `\` continuation.
    backslash: bool,
    /// Break the line's block even if it fits (`} else { ... }` after a broken `if` block).
    force_break: bool,
}

impl Line {
    fn new(blank_before: bool) -> Self {
        Line { toks: 0..0, comment: None, comment_gap: 1, blank_before, backslash: false, force_break: false }
    }
}

struct Frame {
    open_indent: i32,
    stmt_indent: i32,
}

struct Formatter<'a> {
    src: &'a str,
    a: Analysis,
    comments: Vec<Comment>,
    out: Vec<String>,
}

impl<'a> Formatter<'a> {
    fn new(src: &'a str, toks: Vec<Token>, comments: Vec<Comment>) -> Self {
        Formatter { src, a: Analysis::new(src, toks), comments, out: Vec::new() }
    }

    /// Splits the source into physical lines of tokens and comments.
    fn lines(&self) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::new();
        let mut cur: Option<Line> = None;
        let mut prev_end = 0usize;
        let (mut ti, mut ci) = (0, 0);
        loop {
            let tstart = self.a.toks.get(ti).map(|t| t.span.start as usize);
            let cstart = self.comments.get(ci).map(|c| c.span.start as usize);
            let (is_tok, start, end) = match (tstart, cstart) {
                (Some(t), Some(c)) if t < c => (true, t, self.a.toks[ti].span.end as usize),
                (Some(t), None) => (true, t, self.a.toks[ti].span.end as usize),
                (_, Some(c)) => (false, c, self.comments[ci].span.end as usize),
                (None, None) => break,
            };
            let gap = &self.src[prev_end..start];
            let newlines = gap.matches('\n').count();
            if newlines > 0 || cur.is_none() {
                if let Some(mut l) = cur.take() {
                    l.backslash = gap.contains("\\\n") || gap.contains("\\\r\n");
                    lines.push(l);
                }
                cur = Some(Line::new(newlines >= 2));
            }
            let line = cur.as_mut().unwrap();
            if is_tok {
                if line.toks.is_empty() {
                    line.toks = ti..ti + 1;
                } else {
                    line.toks.end = ti + 1;
                }
                ti += 1;
            } else {
                if !line.toks.is_empty() {
                    line.comment_gap = gap.chars().count().max(1);
                }
                line.comment = Some(ci);
                ci += 1;
            }
            prev_end = end;
        }
        lines.extend(cur);
        self.join_else(lines)
    }

    /// `}` on one line and `else` on the next become `} else`.
    fn join_else(&self, lines: Vec<Line>) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::with_capacity(lines.len());
        for l in lines {
            if let Some(prev) = out.last_mut() {
                let is_else = !l.toks.is_empty() && matches!(self.a.tok(l.toks.start), Tok::Kw(Kw::Else));
                if is_else
                    && !prev.toks.is_empty()
                    && prev.comment.is_none()
                    && !prev.backslash
                    && prev.toks.end == l.toks.start
                    && self.a.sym(prev.toks.end - 1) == Some("}")
                {
                    prev.toks.end = l.toks.end;
                    prev.comment = l.comment;
                    prev.comment_gap = l.comment_gap;
                    prev.backslash = l.backslash;
                    continue;
                }
            }
            out.push(l);
        }
        out
    }

    fn render(&self, toks: &Range<usize>) -> String {
        let mut s = String::new();
        for i in toks.clone() {
            if i != toks.start && self.a.space_before(i) {
                s.push(' ');
            }
            s.push_str(&self.a.texts[i]);
        }
        s
    }

    fn is_chain_start(&self, i: usize) -> bool {
        match self.a.sym(i) {
            Some("?.") => true,
            Some(".") => {
                !matches!(self.a.toks.get(i + 1).map(|t| &t.tok), Some(Tok::Ident(n)) if n.starts_with(|c: char| c.is_ascii_uppercase()))
            }
            _ => false,
        }
    }

    /// Breaks the outermost `{ ... }` block that opens and closes on the line.
    fn split(&self, line: &Line) -> Option<Vec<Line>> {
        let r = line.toks.clone();
        if r.clone().any(|i| self.a.texts[i].contains('\n')) {
            return None;
        }
        let mut depth = 0;
        let mut best: Option<(i32, usize, usize)> = None;
        for i in r.clone() {
            let t = self.a.tok(i);
            if is_closer(t) {
                depth -= 1;
            }
            if self.a.sym(i) == Some("{") {
                let m = self.a.matching[i];
                if m != usize::MAX && m < r.end && best.is_none_or(|(d, _, _)| depth < d) {
                    let body = self.body_start(i);
                    if body < m {
                        best = Some((depth, i, body));
                    }
                }
            }
            if is_opener(t) {
                depth += 1;
            }
        }
        let (_, open, body) = best?;
        let close = self.a.matching[open];
        let kind = self.a.braces[open];
        let mut out = vec![Line { toks: r.start..body, ..Line::new(line.blank_before) }];
        let mut start = body;
        let mut depth = 0;
        for i in body..close {
            let t = self.a.tok(i);
            if is_opener(t) {
                depth += 1;
            } else if is_closer(t) {
                depth -= 1;
            }
            if depth != 0 {
                continue;
            }
            let end = match self.a.sym(i) {
                Some(";") => i,
                Some(",") if kind != BraceKind::Block => i + 1,
                _ => continue,
            };
            if end > start {
                out.push(Line { toks: start..end, ..Line::new(false) });
            }
            start = i + 1;
        }
        if close > start {
            out.push(Line { toks: start..close, ..Line::new(false) });
        }
        let force_break = close + 1 < r.end && matches!(self.a.tok(close + 1), Tok::Kw(Kw::Else));
        out.push(Line {
            toks: close..r.end,
            comment: line.comment,
            comment_gap: line.comment_gap,
            blank_before: false,
            backslash: line.backslash,
            force_break,
        });
        Some(out)
    }

    /// First token of a block's body: after `{` and any closure parameters.
    fn body_start(&self, open: usize) -> usize {
        let next = open + 1;
        match self.a.roles.get(next) {
            Some(Role::PipeOpen) => {
                (next + 1..self.a.toks.len()).find(|&j| self.a.roles[j] == Role::PipeClose).map_or(next, |j| j + 1)
            }
            Some(Role::PipeEmpty) => next + 1,
            _ => next,
        }
    }

    fn push_line(&mut self, indent: i32, text: &str) {
        let mut s = INDENT.repeat(indent.max(0) as usize);
        s.push_str(text);
        self.out.push(s);
    }

    fn format(mut self) -> String {
        let mut queue: VecDeque<Line> = self.lines().into();
        let mut frames = vec![Frame { open_indent: -1, stmt_indent: 0 }];
        let mut pending: Vec<(usize, bool)> = Vec::new();
        let mut prev_last: Option<usize> = None;
        let mut prev_backslash = false;
        let mut after_open = true;
        while let Some(line) = queue.pop_front() {
            if line.toks.is_empty() {
                if let Some(c) = line.comment {
                    pending.push((c, line.blank_before));
                }
                continue;
            }
            let first = line.toks.start;
            let top = frames.last().unwrap();
            let closer_start = is_closer(self.a.tok(first));
            let continuation = prev_backslash
                || prev_last.is_some_and(|p| {
                    continues_line(self.a.tok(p)) && self.a.sym(p) != Some(",") && self.a.roles[p] != Role::PipeEmpty
                })
                || self.is_chain_start(first);
            let indent = if closer_start {
                top.open_indent
            } else if continuation {
                top.stmt_indent + 1
            } else {
                top.open_indent + 1
            };
            let code = self.render(&line.toks);
            if line.force_break || indent.max(0) as usize * INDENT.len() + code.chars().count() > MAX_WIDTH {
                if let Some(parts) = self.split(&line) {
                    for p in parts.into_iter().rev() {
                        queue.push_front(p);
                    }
                    continue;
                }
            }
            // Comments above this line.
            let comment_indent = if closer_start { top.open_indent + 1 } else { indent };
            for (c, blank) in std::mem::take(&mut pending) {
                if blank && !after_open && !self.out.is_empty() {
                    self.out.push(String::new());
                }
                let text = self.comments[c].text.trim_end().to_string();
                self.push_line(comment_indent, &text);
                after_open = false;
            }
            if line.blank_before && !closer_start && !after_open && !self.out.is_empty() {
                self.out.push(String::new());
            }
            let mut text = code;
            if let Some(c) = line.comment {
                text.push_str(&" ".repeat(line.comment_gap));
                text.push_str(self.comments[c].text.trim_end());
            }
            if line.backslash {
                text.push_str(" \\");
            }
            self.push_line(indent, &text);
            if !closer_start && !continuation {
                frames.last_mut().unwrap().stmt_indent = indent;
            }
            for i in line.toks.clone() {
                let t = self.a.tok(i);
                if is_opener(t) {
                    frames.push(Frame { open_indent: indent, stmt_indent: indent + 1 });
                } else if is_closer(t) && frames.len() > 1 {
                    frames.pop();
                }
            }
            let last = line.toks.end - 1;
            prev_last = Some(last);
            prev_backslash = line.backslash;
            after_open = is_opener(self.a.tok(last));
        }
        let indent = frames.last().unwrap().open_indent + 1;
        for (c, blank) in pending {
            if blank && !after_open && !self.out.is_empty() {
                self.out.push(String::new());
            }
            let text = self.comments[c].text.trim_end().to_string();
            self.push_line(indent, &text);
            after_open = false;
        }
        if self.out.is_empty() {
            return String::new();
        }
        let mut s = self.out.join("\n");
        s.push('\n');
        s
    }
}

// ---- verification ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Sig {
    Tok(Tok, Option<bool>),
    Sep,
}

/// What the parser sees: tokens, statement separators, and whitespace where it matters. Separators
/// next to braces, closure parameters and `else` are dropped because the parser skips them there.
fn signature(src: &str) -> (Vec<Sig>, Vec<Span>) {
    let (toks, _) = lexer::lex(src, 0);
    let mut raw: Vec<(Sig, Span)> = Vec::new();
    let mut code = Vec::new();
    for t in toks {
        match t.tok {
            Tok::Eof => {}
            Tok::Newline | Tok::Sym(";") => raw.push((Sig::Sep, t.span)),
            _ => {
                // Interpolations record their file offset, which moves when the line is re-indented.
                let tok = match &t.tok {
                    Tok::Str(pieces) => Tok::Str(
                        pieces
                            .iter()
                            .map(|p| match p {
                                StrPiece::Code(code, _) => StrPiece::Code(code.clone(), 0),
                                lit => lit.clone(),
                            })
                            .collect(),
                    ),
                    other => other.clone(),
                };
                raw.push((Sig::Tok(tok, None), t.span));
                code.push(t);
            }
        }
    }
    let a = Analysis::new(src, code);
    let mut k = 0;
    for (sig, _) in raw.iter_mut() {
        if let Sig::Tok(_, tight) = sig {
            if a.tightness_matters(k) {
                *tight = Some(a.tight(k));
            }
            k += 1;
        }
    }
    let mut out: Vec<(Sig, Span)> = Vec::new();
    for (sig, span) in raw {
        let after_sep_ok =
            !matches!(out.last(), None | Some((Sig::Sep, _)) | Some((Sig::Tok(Tok::Sym("{" | "|" | "||"), _), _)));
        match sig {
            Sig::Sep if !after_sep_ok => {}
            Sig::Tok(Tok::Sym("}") | Tok::Kw(Kw::Else), _) if matches!(out.last(), Some((Sig::Sep, _))) => {
                out.pop();
                out.push((sig, span));
            }
            _ => out.push((sig, span)),
        }
    }
    while matches!(out.last(), Some((Sig::Sep, _))) {
        out.pop();
    }
    out.into_iter().unzip()
}

fn verify(src: &str, formatted: &str) -> Result<(), Vec<Diagnostic>> {
    let (before, spans) = signature(src);
    let (after, _) = signature(formatted);
    if before == after {
        return Ok(());
    }
    let at = before.iter().zip(&after).position(|(x, y)| x != y).unwrap_or(before.len().min(after.len()));
    let span = spans.get(at).copied().unwrap_or(Span::new(0, 0, 0));
    Err(vec![Diagnostic::error("E0801", "internal formatter error: formatting would change the program", span)
        .note("please report this; the file was left unchanged")])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        let out = format_source(src).unwrap_or_else(|d| panic!("{src}\n{d:#?}"));
        let again = format_source(&out).unwrap_or_else(|d| panic!("{out}\n{d:#?}"));
        assert_eq!(out, again, "not idempotent");
        out
    }

    #[test]
    fn spacing_and_indentation() {
        let src = "fn add( a:Int,b :Int )->Int{\n      a+b\n}\nx=add(1,2)*-3\n";
        assert_eq!(fmt(src), "fn add(a: Int, b: Int) -> Int {\n  a + b\n}\nx = add(1, 2) * -3\n");
    }

    #[test]
    fn keyword_method_names() {
        let src = "class J {\n  n: Int\n  fn loop(self) -> Int { self.n }\n  fn match(self, k: Int) -> Bool { self.n == k }\n}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn trailing_blocks_and_closures() {
        assert_eq!(fmt("ys = xs.map{|x|x*2}\n"), "ys = xs.map { |x| x * 2 }\n");
        assert_eq!(fmt("t = xs.reduce(0){ |a,b| a+b }\n"), "t = xs.reduce(0) { |a, b| a + b }\n");
        assert_eq!(fmt("xs.each {puts(it)}\n"), "xs.each { puts(it) }\n");
        assert_eq!(fmt("f = fn(x:Int)->Int{x*x}\n"), "f = fn(x: Int) -> Int { x * x }\n");
        assert_eq!(fmt("fn f() {}\n"), "fn f() {}\n");
    }

    #[test]
    fn long_blocks_break() {
        let long = format!("ys = xs.map {{ |x| {} }}\n", vec!["x"; 50].join(" + "));
        let out = fmt(&long);
        assert!(out.starts_with("ys = xs.map { |x|\n  x + x"), "{out}");
        assert!(out.ends_with("\n}\n"), "{out}");
        let src = format!("fn f(n: Int) -> Int {{ a = n * 2; b = a + {}; b }}\n", vec!["n"; 20].join(" + "));
        let out = fmt(&src);
        assert_eq!(out.lines().count(), 5, "{out}");
        assert!(out.lines().all(|l| l.len() <= MAX_WIDTH), "{out}");
    }

    #[test]
    fn long_match_breaks_at_commas() {
        let src = format!(
            "fn f(s: Shape) -> Float {{\n  match s {{ .Circle(r) => 3.0 * r * r, .Rect(w, h) => w * h * {}, .Empty => 0.0 }}\n}}\n",
            vec!["1.0"; 15].join(" * ")
        );
        let out = fmt(&src);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "  match s {");
        assert_eq!(lines[2], "    .Circle(r) => 3.0 * r * r,");
        assert_eq!(lines[4], "    .Empty => 0.0");
        assert_eq!(lines[5], "  }");
    }

    #[test]
    fn comments_are_preserved() {
        let src = "#!/usr/bin/env catx\n# header\n\n\n\nfn f() {   # trailing\n\n  # inside\n  x = 1\n\n\n  # before close\n\n}\n# end\n";
        assert_eq!(
            fmt(src),
            "#!/usr/bin/env catx\n# header\n\nfn f() {   # trailing\n  # inside\n  x = 1\n\n  # before close\n}\n# end\n"
        );
        assert_eq!(fmt("x = 1 # note   \n"), "x = 1 # note\n");
        assert_eq!(fmt("class A {\n    # only a comment\n}\n"), "class A {\n  # only a comment\n}\n");
    }

    #[test]
    fn continuation_lines() {
        let src = "total = a +\nb +\n      c\nys = xs\n.map { |x| x }\n.select { |x|\nx > 1\n}\nzs = foo(\n1,\n2\n)\n";
        assert_eq!(
            fmt(src),
            "total = a +\n  b +\n  c\nys = xs\n  .map { |x| x }\n  .select { |x|\n    x > 1\n  }\nzs = foo(\n  1,\n  2\n)\n"
        );
    }

    #[test]
    fn match_and_patterns() {
        let src = "match v {\n.Circle(r)=>r\n1|2=>3\nx if x<0=>-x\nn:Int=>n\n4..9 => 0\n}\n";
        assert_eq!(
            fmt(src),
            "match v {\n  .Circle(r) => r\n  1 | 2 => 3\n  x if x < 0 => -x\n  n: Int => n\n  4..9 => 0\n}\n"
        );
    }

    #[test]
    fn postfix_and_types() {
        assert_eq!(fmt("x : Int? = nil\n"), "x: Int? = nil\n");
        assert_eq!(fmt("r = d[\"k\"] as? String ?? \"none\"\n"), "r = d[\"k\"] as? String ?? \"none\"\n");
        assert_eq!(fmt("v = a.b?.c!.d\n"), "v = a.b?.c!.d\n");
        assert_eq!(
            fmt("fn f(x: List[Int]) -> Result[Int, String] { Ok(x[0]?) }\n"),
            "fn f(x: List[Int]) -> Result[Int, String] { Ok(x[0]?) }\n"
        );
        assert_eq!(fmt("e = [:]\nd = [a:1, \"b\":2]\n"), "e = [:]\nd = [a: 1, \"b\": 2]\n");
        assert_eq!(fmt("if !done and not x { 1 }\n"), "if !done and not x { 1 }\n");
        assert_eq!(fmt("r = (1..10).to_list()\ns = xs[1...n - 1]\n"), "r = (1..10).to_list()\ns = xs[1...n - 1]\n");
        assert_eq!(
            fmt("fn g(f: fn(Int) -> Int, n: Int | String) { 1 }\n"),
            "fn g(f: fn(Int) -> Int, n: Int | String) { 1 }\n"
        );
    }

    #[test]
    fn operator_methods_and_imports() {
        let src = "import geometry.{ Point,area }\nclass V {\nx: Int\nfn +(self, o: V) -> V { V.new(self.x + o.x) }\nfn [](self, i: Int) -> Int { i }\nfn []=(self, i: Int, v: Int) { }\nfn <=>(self, o: V) -> Int { 0 }\n}\n";
        assert_eq!(
            fmt(src),
            "import geometry.{Point, area}\nclass V {\n  x: Int\n  fn +(self, o: V) -> V { V.new(self.x + o.x) }\n  fn [](self, i: Int) -> Int { i }\n  fn []=(self, i: Int, v: Int) {}\n  fn <=>(self, o: V) -> Int { 0 }\n}\n"
        );
    }

    #[test]
    fn else_joins_closing_brace() {
        assert_eq!(fmt("if a {\n1\n}\nelse {\n2\n}\n"), "if a {\n  1\n} else {\n  2\n}\n");
    }

    #[test]
    fn strings_are_verbatim() {
        let src = "s = \"a  #{ b+1 }\\n\"\nt = 'raw\\\\  '\nu = \"line one   \nline two\"\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn whitespace_and_final_newline() {
        assert_eq!(fmt("\n\n  x = 1   \n\n\n\ny = 2"), "x = 1\n\ny = 2\n");
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("x = 1\r\ny = 2\r\n"), "x = 1\ny = 2\n");
    }

    #[test]
    fn keeps_whitespace_the_parser_depends_on() {
        // `x? .y` is a member of the try result; gluing it would read as `x?.y`.
        assert_eq!(fmt("fn f(x: Int?) -> Int? { x? .abs }\n"), "fn f(x: Int?) -> Int? { x? .abs }\n");
        // A `-` that starts a statement is unary even after an operand on the previous line.
        assert_eq!(
            fmt("fn f() -> Int {\n  for x in xs { }\n  -1\n}\n"),
            "fn f() -> Int {\n  for x in xs {}\n  -1\n}\n"
        );
    }

    #[test]
    fn closures_without_params_and_extend_generics() {
        assert_eq!(fmt("c = { ||\ncount += 1\ncount\n}\n"), "c = { ||\n  count += 1\n  count\n}\n");
        assert_eq!(fmt("extend[T: Comparable] List[T] { }\n"), "extend[T: Comparable] List[T] {}\n");
    }

    #[test]
    fn reindents_interpolated_strings() {
        assert_eq!(fmt("fn f(n: Int) {\nputs(\"n = #{n}\")\n}\n"), "fn f(n: Int) {\n  puts(\"n = #{n}\")\n}\n");
    }

    #[test]
    fn long_if_else_breaks_every_branch() {
        let cond = ["x > 0"; 12].join(" and ");
        let out = fmt(&format!("if {cond} {{ a }} else if y {{ b }} else {{ c }}\n"));
        assert_eq!(out, format!("if {cond} {{\n  a\n}} else if y {{\n  b\n}} else {{\n  c\n}}\n"));
    }

    #[test]
    fn refuses_parse_errors() {
        let err = format_source("fn f( {\n").unwrap_err();
        assert!(err.iter().all(|d| d.is_error()) && !err.is_empty());
    }

    /// Every program in the repository formats idempotently, and runnable ones still type-check.
    #[test]
    fn repository_sources() {
        use crate::driver::{compile_source, Options};
        use std::path::{Path, PathBuf};

        fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    collect(&p, out);
                } else if p.extension().is_some_and(|x| x == "cx") {
                    out.push(p);
                }
            }
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in ["tests/conformance", "examples", "crates/catallaxy-compiler/src/prelude"] {
            collect(&root.join(dir), &mut files);
        }
        files.sort();
        assert!(files.len() > 20, "found only {} files", files.len());
        let mut formatted = 0;
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap();
            let is_reject = f.components().any(|c| c.as_os_str() == "reject");
            let out = match format_source(&src) {
                Ok(out) => out,
                Err(d) if is_reject => {
                    assert!(d.iter().all(|d| d.code != "E0801"), "{}: {d:#?}", f.display());
                    continue;
                }
                Err(d) => panic!("{}: {d:#?}", f.display()),
            };
            formatted += 1;
            let again =
                format_source(&out).unwrap_or_else(|d| panic!("{}: reformat failed {d:#?}\n{out}", f.display()));
            assert_eq!(out, again, "{}: not idempotent", f.display());
            assert!(out.lines().all(|l| l == l.trim_end()), "{}: trailing whitespace", f.display());
            let in_run = f.components().any(|c| c.as_os_str() == "run");
            let is_lib = f.components().any(|c| c.as_os_str() == "lib");
            if (in_run && !is_lib) || f.starts_with(root.join("examples")) {
                let before = compile_source(f, &src, &Options { check_only: true });
                let after = compile_source(f, &out, &Options { check_only: true });
                assert_eq!(before.has_errors(), after.has_errors(), "{}: check result changed\n{out}", f.display());
                assert!(!after.has_errors(), "{}: {:#?}", f.display(), after.diags);
            }
        }
        assert!(formatted > 20);
    }
}
