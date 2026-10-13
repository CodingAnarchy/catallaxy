//! `catx lsp`: a language server over stdio.
//!
//! Every open buffer is kept in memory and overlays the file system, so imports of unsaved
//! buffers see their current text. Each change re-checks the open documents (type checking only,
//! no bytecode) and publishes their diagnostics. Hover, go to definition and completion compile
//! the requested document on demand and query the checker's side tables (see
//! `catallaxy_compiler::query`).

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use catallaxy_compiler::diag::{Severity, SourceFile};
use catallaxy_compiler::driver::{compile_path, Compilation, Options, OverlayLoader, PRELUDE};
use catallaxy_compiler::query::{self, CompletionKind as QK};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types as lt;
use rustc_hash::FxHashMap;
use serde_json::Value as Json;

pub fn cmd_lsp(_args: &[String]) -> i32 {
    let (conn, io) = Connection::stdio();
    let code = match serve(&conn) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("catx lsp: {e}");
            1
        }
    };
    drop(conn);
    let _ = io.join();
    code
}

fn capabilities() -> lt::ServerCapabilities {
    lt::ServerCapabilities {
        text_document_sync: Some(lt::TextDocumentSyncCapability::Options(lt::TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(lt::TextDocumentSyncKind::FULL),
            save: Some(lt::TextDocumentSyncSaveOptions::Supported(true)),
            ..Default::default()
        })),
        hover_provider: Some(lt::HoverProviderCapability::Simple(true)),
        definition_provider: Some(lt::OneOf::Left(true)),
        completion_provider: Some(lt::CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        document_formatting_provider: if formatting_available() { Some(lt::OneOf::Left(true)) } else { None },
        ..Default::default()
    }
}

struct Server<'a> {
    conn: &'a Connection,
    /// Open documents by file path: (uri, text).
    docs: FxHashMap<PathBuf, (lt::Uri, String)>,
}

fn serve(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let caps = serde_json::to_value(capabilities())?;
    let init = serde_json::json!({
        "capabilities": caps,
        "serverInfo": { "name": "catx", "version": env!("CARGO_PKG_VERSION") },
    });
    let (id, _params) = conn.initialize_start()?;
    conn.initialize_finish(id, init)?;
    let mut s = Server { conn, docs: FxHashMap::default() };
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }
                let id = req.id.clone();
                let resp = match std::panic::catch_unwind(AssertUnwindSafe(|| s.request(req))) {
                    Ok(r) => r,
                    Err(_) => Response::new_err(id, ErrorCode::InternalError as i32, "internal error".into()),
                };
                conn.sender.send(Message::Response(resp))?;
            }
            Message::Notification(n) => {
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| s.notification(n)));
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

// ---- URIs ----------------------------------------------------------------------------------

fn uri_to_path(uri: &lt::Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    // Skip an authority (`file://host/path`); `file:///path` has an empty one.
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let mut p = String::from_utf8(out).ok()?;
    // Windows: `/C:/x` → `C:/x`.
    if cfg!(windows) && p.len() > 2 && p.as_bytes()[2] == b':' {
        p.remove(0);
    }
    Some(PathBuf::from(p))
}

fn path_to_uri(p: &Path) -> Option<lt::Uri> {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/');
    }
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    lt::Uri::from_str(&out).ok()
}

// ---- compilation ---------------------------------------------------------------------------

/// A checked document: the compilation and the file id of the document's own source.
struct Checked {
    comp: Compilation,
    file: u32,
}

impl Server<'_> {
    fn compile(&self, path: &Path, text_override: Option<String>) -> Checked {
        let mut files: FxHashMap<PathBuf, String> =
            self.docs.iter().map(|(p, (_, t))| (p.clone(), t.clone())).collect();
        if let Some(t) = text_override {
            files.insert(path.to_path_buf(), t);
        }
        let comp = compile_path(path, &OverlayLoader { files: &files }, &Options { check_only: true });
        // The entry file is added right after the prelude.
        Checked { comp, file: PRELUDE.len() as u32 }
    }

    fn range(sf: &SourceFile, start: u32, end: u32) -> lt::Range {
        let (l1, c1) = sf.lsp_pos(start);
        let (l2, c2) = sf.lsp_pos(end);
        lt::Range::new(lt::Position::new(l1, c1), lt::Position::new(l2, c2))
    }

    fn location(c: &Compilation, span: catallaxy_compiler::diag::Span) -> Option<lt::Location> {
        if span.file as usize >= c.sm.files.len() {
            return None;
        }
        let sf = c.sm.file(span.file);
        let uri = path_to_uri(Path::new(&sf.path))?;
        Some(lt::Location::new(uri, Self::range(sf, span.start, span.end)))
    }

    fn publish(&self, path: &Path) {
        let Some((uri, _)) = self.docs.get(path) else { return };
        let ck = self.compile(path, None);
        let c = &ck.comp;
        let mut out = Vec::new();
        for d in &c.diags {
            let range = if d.span.file == ck.file && (ck.file as usize) < c.sm.files.len() {
                Self::range(c.sm.file(ck.file), d.span.start, d.span.end)
            } else if d.span.file == u32::MAX {
                lt::Range::default()
            } else {
                // Diagnostics in other files (imported modules) are reported there.
                continue;
            };
            let mut message = d.message.clone();
            let mut related = Vec::new();
            for (note, sp) in &d.notes {
                match sp.and_then(|sp| Self::location(c, sp)) {
                    Some(loc) => {
                        related.push(lt::DiagnosticRelatedInformation { location: loc, message: note.clone() })
                    }
                    None => {
                        message.push_str("\nnote: ");
                        message.push_str(note);
                    }
                }
            }
            for f in &d.fixes {
                message.push_str("\nhelp: ");
                message.push_str(&f.message);
            }
            out.push(lt::Diagnostic {
                range,
                severity: Some(match d.severity {
                    Severity::Error => lt::DiagnosticSeverity::ERROR,
                    Severity::Warning => lt::DiagnosticSeverity::WARNING,
                }),
                code: Some(lt::NumberOrString::String(d.code.to_string())),
                source: Some("catx".into()),
                message,
                related_information: if related.is_empty() { None } else { Some(related) },
                ..Default::default()
            });
        }
        let params = lt::PublishDiagnosticsParams { uri: uri.clone(), diagnostics: out, version: None };
        self.notify("textDocument/publishDiagnostics", params);
    }

    fn publish_all(&self) {
        let paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for p in paths {
            self.publish(&p);
        }
    }

    fn notify(&self, method: &str, params: impl serde::Serialize) {
        let n = Notification::new(method.to_string(), params);
        let _ = self.conn.sender.send(Message::Notification(n));
    }

    // ---- notifications -----------------------------------------------------------------------

    fn notification(&mut self, n: Notification) {
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let Ok(p) = serde_json::from_value::<lt::DidOpenTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                self.docs.insert(path, (p.text_document.uri, p.text_document.text));
                self.publish_all();
            }
            "textDocument/didChange" => {
                let Ok(p) = serde_json::from_value::<lt::DidChangeTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                // Full sync: the last change holds the whole text.
                if let Some(c) = p.content_changes.into_iter().last() {
                    self.docs.insert(path, (p.text_document.uri, c.text));
                }
                self.publish_all();
            }
            "textDocument/didSave" => self.publish_all(),
            "textDocument/didClose" => {
                let Ok(p) = serde_json::from_value::<lt::DidCloseTextDocumentParams>(n.params) else { return };
                if let Some(path) = uri_to_path(&p.text_document.uri) {
                    self.docs.remove(&path);
                }
                let params =
                    lt::PublishDiagnosticsParams { uri: p.text_document.uri, diagnostics: Vec::new(), version: None };
                self.notify("textDocument/publishDiagnostics", params);
                self.publish_all();
            }
            _ => {}
        }
    }

    // ---- requests ----------------------------------------------------------------------------

    fn request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        let result: Result<Json, String> = match req.method.as_str() {
            "textDocument/hover" => parse(req.params).map(|p| to_json(self.hover(p))),
            "textDocument/definition" => parse(req.params).map(|p| to_json(self.definition(p))),
            "textDocument/completion" => parse(req.params).map(|p| to_json(self.completion(p))),
            "textDocument/formatting" => parse(req.params).map(|p| to_json(self.formatting(p))),
            other => {
                return Response::new_err(
                    id,
                    ErrorCode::MethodNotFound as i32,
                    format!("unsupported request `{other}`"),
                )
            }
        };
        match result {
            Ok(v) => Response::new_ok(id, v),
            Err(e) => Response::new_err(id, ErrorCode::InvalidParams as i32, e),
        }
    }

    /// Path, text and byte offset of a document position.
    fn locate(&self, pos: &lt::TextDocumentPositionParams) -> Option<(PathBuf, String, u32)> {
        let path = uri_to_path(&pos.text_document.uri)?;
        let text = match self.docs.get(&path) {
            Some((_, t)) => t.clone(),
            None => std::fs::read_to_string(&path).ok()?,
        };
        let sf = SourceFile::new(String::new(), text.clone());
        let off = sf.offset_of(pos.position.line, pos.position.character);
        Some((path, text, off))
    }

    fn hover(&self, p: lt::HoverParams) -> Option<lt::Hover> {
        let (path, _, off) = self.locate(&p.text_document_position_params)?;
        let c = self.compile(&path, None);
        let m = query::module_of_file(&c.comp.ck, c.file)?;
        let (text, span) = query::hover(&c.comp.ck, m, off)?;
        Some(lt::Hover {
            contents: lt::HoverContents::Markup(lt::MarkupContent { kind: lt::MarkupKind::Markdown, value: text }),
            range: Some(Self::range(c.comp.sm.file(c.file), span.start, span.end)),
        })
    }

    fn definition(&self, p: lt::GotoDefinitionParams) -> Option<lt::GotoDefinitionResponse> {
        let (path, _, off) = self.locate(&p.text_document_position_params)?;
        let c = self.compile(&path, None);
        let m = query::module_of_file(&c.comp.ck, c.file)?;
        let span = query::definition(&c.comp.ck, m, off)?;
        Self::location(&c.comp, span).map(lt::GotoDefinitionResponse::Scalar)
    }

    fn completion(&self, p: lt::CompletionParams) -> Option<lt::CompletionResponse> {
        let (path, text, off) = self.locate(&p.text_document_position)?;
        let bytes = text.as_bytes();
        let mut start = off as usize;
        while start > 0
            && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_' || bytes[start - 1] >= 0x80)
        {
            start -= 1;
        }
        let items = if start > 0 && bytes[start - 1] == b'.' && !(start > 1 && bytes[start - 2] == b'.') {
            // Member completion: blank out `.partial` so the receiver parses and checks cleanly.
            let mut dot = start - 1;
            if dot > 0 && bytes[dot - 1] == b'?' {
                dot -= 1;
            }
            let mut patched = text[..dot].to_string();
            patched.push_str(&" ".repeat(off as usize - dot));
            patched.push_str(&text[off as usize..]);
            let mut c = self.compile(&path, Some(patched));
            let m = query::module_of_file(&c.comp.ck, c.file)?;
            let ck = &mut c.comp.ck;
            let recv = query::expr_ending_at(&ck.mods[m as usize].ast, dot as u32)?.clone();
            match query::path_res(ck, m, &recv) {
                Some(
                    res @ (catallaxy_compiler::check::NameRes::Class(_)
                    | catallaxy_compiler::check::NameRes::Enum(_)
                    | catallaxy_compiler::check::NameRes::Module(_)),
                ) => query::path_completions(ck, res),
                _ => {
                    let t = *ck.expr_ty.get(&recv.id)?;
                    query::member_completions(ck, m, t)
                }
            }
        } else {
            let c = self.compile(&path, None);
            let m = query::module_of_file(&c.comp.ck, c.file)?;
            query::scope_completions(&c.comp.ck, m, off)
        };
        let items = items
            .into_iter()
            .map(|c| lt::CompletionItem {
                label: c.label,
                kind: Some(match c.kind {
                    QK::Method => lt::CompletionItemKind::METHOD,
                    QK::Field => lt::CompletionItemKind::FIELD,
                    QK::Function => lt::CompletionItemKind::FUNCTION,
                    QK::Variable => lt::CompletionItemKind::VARIABLE,
                    QK::Class => lt::CompletionItemKind::CLASS,
                    QK::Enum => lt::CompletionItemKind::ENUM,
                    QK::Interface => lt::CompletionItemKind::INTERFACE,
                    QK::Module => lt::CompletionItemKind::MODULE,
                    QK::Keyword => lt::CompletionItemKind::KEYWORD,
                    QK::Constant => lt::CompletionItemKind::CONSTANT,
                    QK::Variant => lt::CompletionItemKind::ENUM_MEMBER,
                }),
                detail: c.detail,
                ..Default::default()
            })
            .collect();
        Some(lt::CompletionResponse::Array(items))
    }

    fn formatting(&self, p: lt::DocumentFormattingParams) -> Option<Vec<lt::TextEdit>> {
        let path = uri_to_path(&p.text_document.uri)?;
        let text = self.docs.get(&path).map(|(_, t)| t.clone())?;
        let formatted = format_document(&text)?;
        if formatted == text {
            return Some(Vec::new());
        }
        let sf = SourceFile::new(String::new(), text.clone());
        let end = Self::range(&sf, 0, text.len() as u32).end;
        Some(vec![lt::TextEdit { range: lt::Range::new(lt::Position::new(0, 0), end), new_text: formatted }])
    }
}

// ---- formatting hook -----------------------------------------------------------------------
//
// FORMATTER HOOK: document formatting delegates to `catallaxy_compiler::fmt::format_source`,
// which is developed separately. Build with `--features fmt` once that module exists (and adjust
// the call below if its signature differs). Without the feature the server does not advertise
// formatting.

#[cfg(feature = "fmt")]
fn formatting_available() -> bool {
    true
}

#[cfg(feature = "fmt")]
fn format_document(text: &str) -> Option<String> {
    catallaxy_compiler::fmt::format_source(text).ok()
}

#[cfg(not(feature = "fmt"))]
fn formatting_available() -> bool {
    false
}

#[cfg(not(feature = "fmt"))]
fn format_document(_text: &str) -> Option<String> {
    None
}

// ---- helpers -------------------------------------------------------------------------------

fn parse<T: serde::de::DeserializeOwned>(v: Json) -> Result<T, String> {
    serde_json::from_value(v).map_err(|e| e.to_string())
}

fn to_json<T: serde::Serialize>(v: T) -> Json {
    serde_json::to_value(v).unwrap_or(Json::Null)
}
