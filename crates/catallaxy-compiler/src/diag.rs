//! Source spans, diagnostics with error codes and machine-applicable fixes, and rendering.

use std::fmt::Write as _;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub file: u32,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: u32, start: u32, end: u32) -> Self {
        Span { file, start, end }
    }
    pub fn to(self, other: Span) -> Span {
        Span { file: self.file, start: self.start.min(other.start), end: self.end.max(other.end) }
    }
    pub fn empty_at(self) -> Span {
        Span { file: self.file, start: self.start, end: self.start }
    }
    pub fn after(self) -> Span {
        Span { file: self.file, start: self.end, end: self.end }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub span: Span,
    pub replacement: String,
}

#[derive(Clone, Debug)]
pub struct Fix {
    pub message: String,
    pub edits: Vec<Edit>,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    pub notes: Vec<(String, Option<Span>)>,
    pub fixes: Vec<Fix>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            code,
            severity: Severity::Error,
            message: message.into(),
            span,
            notes: Vec::new(),
            fixes: Vec::new(),
        }
    }
    pub fn warning(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            code,
            severity: Severity::Warning,
            message: message.into(),
            span,
            notes: Vec::new(),
            fixes: Vec::new(),
        }
    }
    pub fn note(mut self, msg: impl Into<String>) -> Self {
        self.notes.push((msg.into(), None));
        self
    }
    pub fn note_at(mut self, msg: impl Into<String>, span: Span) -> Self {
        self.notes.push((msg.into(), Some(span)));
        self
    }
    pub fn fix(mut self, message: impl Into<String>, span: Span, replacement: impl Into<String>) -> Self {
        self.fixes.push(Fix { message: message.into(), edits: vec![Edit { span, replacement: replacement.into() }] });
        self
    }
    pub fn fix_edits(mut self, message: impl Into<String>, edits: Vec<Edit>) -> Self {
        self.fixes.push(Fix { message: message.into(), edits });
        self
    }
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// A loaded source file.
pub struct SourceFile {
    pub path: String,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(path: String, text: String) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile { path, text, line_starts }
    }

    /// 1-based line and column (in characters) of a byte offset.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let ls = self.line_starts[line] as usize;
        let end = (offset as usize).min(self.text.len());
        let col = self.text.get(ls..end).map(|s| s.chars().count()).unwrap_or(end - ls);
        (line as u32 + 1, col as u32 + 1)
    }

    pub fn line_text(&self, line: u32) -> &str {
        let i = (line - 1) as usize;
        let s = self.line_starts[i] as usize;
        let e = self.line_starts.get(i + 1).map(|&e| e as usize).unwrap_or(self.text.len());
        self.text[s..e].trim_end_matches(['\n', '\r'])
    }

    /// Byte offset of a 1-based line and 0-based UTF-16 character (for LSP positions).
    pub fn offset_of(&self, line0: u32, utf16_col: u32) -> u32 {
        let Some(&ls) = self.line_starts.get(line0 as usize) else { return self.text.len() as u32 };
        let mut off = ls as usize;
        let mut units = 0;
        for ch in self.text[ls as usize..].chars() {
            if units >= utf16_col || ch == '\n' {
                break;
            }
            units += ch.len_utf16() as u32;
            off += ch.len_utf8();
        }
        off as u32
    }

    /// 0-based line and UTF-16 column (for LSP).
    pub fn lsp_pos(&self, offset: u32) -> (u32, u32) {
        let (line, _) = self.line_col(offset);
        let ls = self.line_starts[(line - 1) as usize] as usize;
        let end = (offset as usize).min(self.text.len());
        let col: usize = self.text[ls..end].chars().map(|c| c.len_utf16()).sum();
        (line - 1, col as u32)
    }
}

#[derive(Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn add(&mut self, path: String, text: String) -> u32 {
        self.files.push(SourceFile::new(path, text));
        self.files.len() as u32 - 1
    }
    pub fn file(&self, id: u32) -> &SourceFile {
        &self.files[id as usize]
    }

    /// Human-readable rendering with a source excerpt.
    pub fn render(&self, d: &Diagnostic, color: bool) -> String {
        let (red, yellow, blue, bold, reset) =
            if color { ("\x1b[31m", "\x1b[33m", "\x1b[34m", "\x1b[1m", "\x1b[0m") } else { ("", "", "", "", "") };
        let mut out = String::new();
        let (sev, c) = match d.severity {
            Severity::Error => ("error", red),
            Severity::Warning => ("warning", yellow),
        };
        let _ = writeln!(out, "{bold}{c}{sev}[{}]{reset}{bold}: {}{reset}", d.code, d.message);
        self.excerpt(&mut out, d.span, blue, c, reset);
        for (n, sp) in &d.notes {
            match sp {
                Some(sp) if sp.file != u32::MAX && (sp.file as usize) < self.files.len() => {
                    let _ = writeln!(out, "  {blue}note{reset}: {n}");
                    self.excerpt(&mut out, *sp, blue, blue, reset);
                }
                _ => {
                    let _ = writeln!(out, "  {blue}={reset} note: {n}");
                }
            }
        }
        for f in &d.fixes {
            let _ = writeln!(out, "  {blue}={reset} help: {}", f.message);
        }
        out
    }

    fn excerpt(&self, out: &mut String, span: Span, blue: &str, c: &str, reset: &str) {
        if span.file as usize >= self.files.len() {
            return;
        }
        let f = self.file(span.file);
        let (line, col) = f.line_col(span.start);
        let (eline, ecol) = f.line_col(span.end);
        let _ = writeln!(out, "  {blue}-->{reset} {}:{}:{}", f.path, line, col);
        let text = f.line_text(line);
        let w = line.to_string().len();
        let _ = writeln!(out, "  {blue}{:w$} |{reset}", "");
        let _ = writeln!(out, "  {blue}{line} |{reset} {text}");
        let width = if eline == line {
            (ecol.saturating_sub(col)).max(1)
        } else {
            (text.chars().count() as u32 + 1).saturating_sub(col).max(1)
        };
        let _ = writeln!(
            out,
            "  {blue}{:w$} |{reset} {}{c}{}{reset}",
            "",
            " ".repeat(col as usize - 1),
            "^".repeat(width as usize)
        );
    }

    /// JSON for `check --json`.
    pub fn to_json(&self, diags: &[Diagnostic]) -> String {
        let mut out = String::from("{\"diagnostics\":[");
        for (i, d) in diags.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            let _ = write!(out, "\"code\":{},", json_str(d.code));
            let _ = write!(
                out,
                "\"severity\":{},",
                json_str(if d.severity == Severity::Error { "error" } else { "warning" })
            );
            let _ = write!(out, "\"message\":{},", json_str(&d.message));
            let file =
                if (d.span.file as usize) < self.files.len() { self.file(d.span.file).path.as_str() } else { "" };
            let _ = write!(out, "\"file\":{},", json_str(file));
            let _ = write!(out, "\"span\":{},", self.span_json(d.span));
            out.push_str("\"notes\":[");
            for (j, (n, sp)) in d.notes.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{{\"message\":{}", json_str(n));
                if let Some(sp) = sp {
                    if (sp.file as usize) < self.files.len() {
                        let _ = write!(
                            out,
                            ",\"file\":{},\"span\":{}",
                            json_str(&self.file(sp.file).path),
                            self.span_json(*sp)
                        );
                    }
                }
                out.push('}');
            }
            out.push_str("],\"fixes\":[");
            for (j, f) in d.fixes.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{{\"message\":{},\"edits\":[", json_str(&f.message));
                for (k, e) in f.edits.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    let _ = write!(
                        out,
                        "{{\"span\":{},\"replacement\":{}}}",
                        self.span_json(e.span),
                        json_str(&e.replacement)
                    );
                }
                out.push_str("]}");
            }
            out.push_str("]}");
        }
        out.push_str("]}");
        out
    }

    fn span_json(&self, s: Span) -> String {
        if s.file as usize >= self.files.len() {
            return "null".into();
        }
        let f = self.file(s.file);
        let (l1, c1) = f.line_col(s.start);
        let (l2, c2) = f.line_col(s.end);
        format!(
            "{{\"start\":{{\"line\":{l1},\"col\":{c1},\"offset\":{}}},\"end\":{{\"line\":{l2},\"col\":{c2},\"offset\":{}}}}}",
            s.start, s.end
        )
    }
}

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
