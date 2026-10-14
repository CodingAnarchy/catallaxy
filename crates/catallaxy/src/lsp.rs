//! `catx lsp`: a language server over stdio.
//!
//! Every open buffer is kept in memory and overlays the file system, so imports of unsaved
//! buffers see their current text. Each change re-checks the open documents (type checking only,
//! no bytecode) and publishes their diagnostics. Hover, go to definition and completion compile
//! the requested document on demand and query the checker's side tables (see
//! `catallaxy_compiler::query`). References, rename, signature help, inlay hints and symbol lists
//! use the symbol index and outlines of `catallaxy_compiler::symbols`; references and rename cover
//! the compilations of every open document (each document and the modules it imports), matching
//! symbols across them by their declarations, and take along the methods that implement an
//! interface member in the types used as that interface. Code actions
//! offer the fixes attached to the document's diagnostics as quick fixes.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use catallaxy_compiler::diag::{Severity, SourceFile};
use catallaxy_compiler::driver::{compile_path, Compilation, Options, OverlayLoader, PRELUDE};
use catallaxy_compiler::query::{self, CompletionKind as QK};
use catallaxy_compiler::symbols::{self, Index, OutlineItem, OutlineKind, Symbol};
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
        references_provider: Some(lt::OneOf::Left(true)),
        rename_provider: Some(lt::OneOf::Right(lt::RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        signature_help_provider: Some(lt::SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into()]),
            retrigger_characters: None,
            work_done_progress_options: Default::default(),
        }),
        inlay_hint_provider: Some(lt::OneOf::Left(true)),
        document_symbol_provider: Some(lt::OneOf::Left(true)),
        workspace_symbol_provider: Some(lt::OneOf::Left(true)),
        code_action_provider: Some(lt::CodeActionProviderCapability::Options(lt::CodeActionOptions {
            code_action_kinds: Some(vec![lt::CodeActionKind::QUICKFIX]),
            resolve_provider: None,
            work_done_progress_options: Default::default(),
        })),
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

    /// The LSP form of a diagnostic of the checked document, or `None` if it belongs to another
    /// file (diagnostics in imported modules are reported there).
    fn lsp_diagnostic(ck: &Checked, d: &catallaxy_compiler::diag::Diagnostic) -> Option<lt::Diagnostic> {
        let c = &ck.comp;
        let range = if d.span.file == ck.file && (ck.file as usize) < c.sm.files.len() {
            Self::range(c.sm.file(ck.file), d.span.start, d.span.end)
        } else if d.span.file == u32::MAX {
            lt::Range::default()
        } else {
            return None;
        };
        let mut message = d.message.clone();
        let mut related = Vec::new();
        for (note, sp) in &d.notes {
            match sp.and_then(|sp| Self::location(c, sp)) {
                Some(loc) => related.push(lt::DiagnosticRelatedInformation { location: loc, message: note.clone() }),
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
        Some(lt::Diagnostic {
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
        })
    }

    fn publish(&self, path: &Path) {
        let Some((uri, _)) = self.docs.get(path) else { return };
        let ck = self.compile(path, None);
        let out: Vec<lt::Diagnostic> = ck.comp.diags.iter().filter_map(|d| Self::lsp_diagnostic(&ck, d)).collect();
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
        let result: Result<Json, (ErrorCode, String)> = match req.method.as_str() {
            "textDocument/hover" => parse(req.params).map(|p| to_json(self.hover(p))),
            "textDocument/definition" => parse(req.params).map(|p| to_json(self.definition(p))),
            "textDocument/completion" => parse(req.params).map(|p| to_json(self.completion(p))),
            "textDocument/formatting" => parse(req.params).map(|p| to_json(self.formatting(p))),
            "textDocument/references" => parse(req.params).map(|p| to_json(self.references(p))),
            "textDocument/prepareRename" => parse(req.params).and_then(|p| failed(self.prepare_rename(p))),
            "textDocument/rename" => parse(req.params).and_then(|p| failed(self.rename(p))),
            "textDocument/signatureHelp" => parse(req.params).map(|p| to_json(self.signature_help(p))),
            "textDocument/inlayHint" => parse(req.params).map(|p| to_json(self.inlay_hints(p))),
            "textDocument/documentSymbol" => parse(req.params).map(|p| to_json(self.document_symbols(p))),
            "workspace/symbol" => parse(req.params).map(|p| to_json(self.workspace_symbols(p))),
            "textDocument/codeAction" => parse(req.params).map(|p| to_json(self.code_actions(p))),
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
            Err((code, e)) => Response::new_err(id, code as i32, e),
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

    /// Quick fixes for the diagnostics that touch the requested range: one action per fix, the
    /// first mechanically applicable fix of each diagnostic marked preferred.
    #[allow(clippy::mutable_key_type)]
    fn code_actions(&self, p: lt::CodeActionParams) -> Option<Vec<lt::CodeActionOrCommand>> {
        if p.context.only.as_ref().is_some_and(|only| !only.iter().any(|k| k.as_str().starts_with("quickfix"))) {
            return Some(Vec::new());
        }
        let path = uri_to_path(&p.text_document.uri)?;
        let ck = self.compile(&path, None);
        let c = &ck.comp;
        let mut out = Vec::new();
        for d in &c.diags {
            if d.fixes.is_empty() {
                continue;
            }
            let Some(diag) = Self::lsp_diagnostic(&ck, d) else { continue };
            let (r, want) = (diag.range, p.range);
            if (r.end.line, r.end.character) < (want.start.line, want.start.character)
                || (want.end.line, want.end.character) < (r.start.line, r.start.character)
            {
                continue;
            }
            let preferred = d.fixes.iter().position(|f| f.applicable);
            for (i, f) in d.fixes.iter().enumerate() {
                let mut changes: std::collections::HashMap<lt::Uri, Vec<lt::TextEdit>> = Default::default();
                let mut ok = true;
                for e in &f.edits {
                    match Self::location(c, e.span) {
                        Some(loc) => changes
                            .entry(loc.uri)
                            .or_default()
                            .push(lt::TextEdit { range: loc.range, new_text: e.replacement.clone() }),
                        None => ok = false,
                    }
                }
                if !ok || changes.is_empty() {
                    continue;
                }
                // Locations name the document by its path; answer with the client's own URI.
                if let Some(own) = Self::location(c, catallaxy_compiler::diag::Span::new(ck.file, 0, 0)) {
                    if let Some(edits) = changes.remove(&own.uri) {
                        changes.insert(p.text_document.uri.clone(), edits);
                    }
                }
                out.push(lt::CodeActionOrCommand::CodeAction(lt::CodeAction {
                    title: f.message.clone(),
                    kind: Some(lt::CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(lt::WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    is_preferred: if preferred == Some(i) { Some(true) } else { None },
                    ..Default::default()
                }));
            }
        }
        Some(out)
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

// ---- navigation: references, rename, signature help, inlay hints, symbols ------------------

/// The symbol under a cursor, with the checked compilation and its index.
struct AtSymbol {
    path: PathBuf,
    ck: Checked,
    index: Index,
    sym: Symbol,
    /// The occurrence under the cursor.
    span: catallaxy_compiler::diag::Span,
}

/// Everything a rename of the symbol under a cursor rewrites, across compilations.
struct Gathered {
    /// Each occurrence once: its location and whether it declares its symbol.
    occs: Vec<(lt::Location, bool)>,
    /// Why the rename must be refused, if it must.
    refusal: Option<String>,
}

/// A compilation with its symbol index, the canonical path of each of its files, and the
/// symbols of the rename found in it.
struct Unit {
    ck: Checked,
    index: Index,
    files: Vec<PathBuf>,
    syms: Vec<Symbol>,
}

impl Unit {
    fn new(ck: Checked, index: Index) -> Unit {
        let files = ck
            .comp
            .sm
            .files
            .iter()
            .map(|f| {
                let p = Path::new(&f.path);
                std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
            })
            .collect();
        Unit { ck, index, files, syms: Vec::new() }
    }

    /// A span as a position independent of the compilation's file numbering.
    fn key(&self, sp: catallaxy_compiler::diag::Span) -> Option<(PathBuf, u32, u32)> {
        Some((self.files.get(sp.file as usize)?.clone(), sp.start, sp.end))
    }
}

impl Server<'_> {
    fn symbol_at(&self, pos: &lt::TextDocumentPositionParams) -> Option<AtSymbol> {
        let (path, _, off) = self.locate(pos)?;
        let ck = self.compile(&path, None);
        let index = Index::build(&ck.comp.ck, &ck.comp.sm);
        let occ = *index.at(ck.file, off)?;
        Some(AtSymbol { path, ck, index, sym: occ.sym, span: occ.span })
    }

    /// The occurrences of `at`'s symbol and of the symbols renamed with it (the methods that
    /// implement an interface member, see [`Index::family`]), in the compilation of `at`'s
    /// document and in those of the other open documents, which may import it. Symbols are
    /// matched across compilations by their declarations.
    fn gather(&self, at: AtSymbol) -> Gathered {
        let AtSymbol { path, ck, index, sym, .. } = at;
        let mut first = Unit::new(ck, index);
        first.syms = first.index.family(sym);
        let mut units = vec![first];
        let mut others: Vec<&PathBuf> = self.docs.keys().filter(|p| **p != path).collect();
        others.sort();
        for p in others {
            let ck = self.compile(p, None);
            let index = Index::build(&ck.comp.ck, &ck.comp.sm);
            units.push(Unit::new(ck, index));
        }
        let mut keys: rustc_hash::FxHashSet<(PathBuf, u32, u32)> = Default::default();
        let mut grew = true;
        while grew {
            grew = false;
            for u in units.iter_mut() {
                let found: Vec<Symbol> = u
                    .index
                    .occs
                    .iter()
                    .filter(|o| o.is_decl && !u.syms.contains(&o.sym) && u.key(o.span).is_some_and(|k| keys.contains(&k)))
                    .map(|o| o.sym)
                    .collect();
                for s in found {
                    for f in u.index.family(s) {
                        if !u.syms.contains(&f) {
                            u.syms.push(f);
                        }
                    }
                }
                for s in &u.syms {
                    if let Some(k) = symbols::decl_span(&u.ck.comp.ck, *s).and_then(|sp| u.key(sp)) {
                        grew |= keys.insert(k);
                    }
                }
            }
        }
        let mut refusal = None;
        let mut seen = rustc_hash::FxHashSet::default();
        let mut occs = Vec::new();
        for u in &units {
            let c = &u.ck.comp;
            for &s in &u.syms {
                if refusal.is_none() && !u.index.renamable(&c.ck, s) {
                    let name = symbols::symbol_name(&c.ck, &c.sm, s).unwrap_or_default();
                    refusal = Some(match s {
                        Symbol::Fn(f) if s != sym => format!(
                            "renaming `{name}` would also rename `{}`, which is not declared in this program (it is built in or from the prelude)",
                            query::fn_signature(&c.ck, f)
                        ),
                        _ => format!("`{name}` is not declared in this program (it is built in or from the prelude)"),
                    });
                }
            }
            for o in u.index.occs.iter().filter(|o| u.syms.contains(&o.sym)) {
                let Some(loc) = Self::location(c, o.span) else { continue };
                if seen.insert((loc.uri.as_str().to_string(), loc.range.start, loc.range.end)) {
                    occs.push((loc, o.is_decl));
                }
            }
        }
        Gathered { occs, refusal }
    }

    fn references(&self, p: lt::ReferenceParams) -> Option<Vec<lt::Location>> {
        let at = self.symbol_at(&p.text_document_position)?;
        let with_decl = p.context.include_declaration;
        Some(self.gather(at).occs.into_iter().filter(|(_, d)| with_decl || !d).map(|(l, _)| l).collect())
    }

    fn prepare_rename(&self, p: lt::TextDocumentPositionParams) -> Result<Option<lt::PrepareRenameResponse>, String> {
        let Some(at) = self.symbol_at(&p) else { return Ok(None) };
        let range = Self::range(at.ck.comp.sm.file(at.ck.file), at.span.start, at.span.end);
        if let Some(why) = self.gather(at).refusal {
            return Err(why);
        }
        Ok(Some(lt::PrepareRenameResponse::Range(range)))
    }

    // `WorkspaceEdit::changes` is keyed by `Uri`, whose lazily computed parts are cells.
    #[allow(clippy::mutable_key_type)]
    fn rename(&self, p: lt::RenameParams) -> Result<Option<lt::WorkspaceEdit>, String> {
        let new_name = p.new_name.trim();
        if !symbols::is_identifier(new_name) {
            return Err(format!("`{new_name}` is not a valid name"));
        }
        let Some(at) = self.symbol_at(&p.text_document_position) else { return Ok(None) };
        let g = self.gather(at);
        if let Some(why) = g.refusal {
            return Err(why);
        }
        let mut changes: std::collections::HashMap<lt::Uri, Vec<lt::TextEdit>> = Default::default();
        for (loc, _) in g.occs {
            changes.entry(loc.uri).or_default().push(lt::TextEdit { range: loc.range, new_text: new_name.into() });
        }
        Ok(Some(lt::WorkspaceEdit { changes: Some(changes), ..Default::default() }))
    }

    fn signature_help(&self, p: lt::SignatureHelpParams) -> Option<lt::SignatureHelp> {
        let (path, text, off) = self.locate(&p.text_document_position_params)?;
        let call = open_call(&text, off as usize)?;
        // The call as written, else with the argument list (and then every open bracket) closed:
        // the user is still typing it.
        let mut found = None;
        let all = &call.closers;
        for patch in ["", ")", "nil)", all, &format!("nil{all}")] {
            let src = (!patch.is_empty()).then(|| format!("{}{patch}{}", &text[..off as usize], &text[off as usize..]));
            let c = self.compile(&path, src);
            let Some(m) = query::module_of_file(&c.comp.ck, c.file) else { continue };
            if let Some(f) = symbols::call_at_paren(&c.comp.ck, m, call.paren as u32) {
                found = Some(symbols::signature_info(&c.comp.ck, &c.comp.sm, f));
                break;
            }
        }
        let sig = found?;
        let active = match &call.named {
            Some(n) => sig.params.iter().position(|q| &q.name == n),
            None => Some(call.commas as usize).filter(|&i| i < sig.params.len()),
        };
        let utf16 = |b: u32| sig.label[..b as usize].encode_utf16().count() as u32;
        let parameters = sig
            .params
            .iter()
            .map(|q| lt::ParameterInformation {
                label: lt::ParameterLabel::LabelOffsets([utf16(q.range.0), utf16(q.range.1)]),
                documentation: q
                    .is_block
                    .then(|| lt::Documentation::String("block parameter: pass a trailing `{ |…| … }`".into()))
                    .or_else(|| q.has_default.then(|| lt::Documentation::String("optional".into()))),
            })
            .collect();
        Some(lt::SignatureHelp {
            signatures: vec![lt::SignatureInformation {
                label: sig.label.clone(),
                documentation: sig.doc.clone().map(|d| {
                    lt::Documentation::MarkupContent(lt::MarkupContent { kind: lt::MarkupKind::Markdown, value: d })
                }),
                parameters: Some(parameters),
                active_parameter: active.map(|a| a as u32),
            }],
            active_signature: Some(0),
            active_parameter: active.map(|a| a as u32),
        })
    }

    fn inlay_hints(&self, p: lt::InlayHintParams) -> Option<Vec<lt::InlayHint>> {
        let path = uri_to_path(&p.text_document.uri)?;
        let c = self.compile(&path, None);
        let m = query::module_of_file(&c.comp.ck, c.file)?;
        let sf = c.comp.sm.file(c.file);
        let lo = sf.offset_of(p.range.start.line, p.range.start.character);
        let hi = sf.offset_of(p.range.end.line, p.range.end.character);
        Some(
            symbols::inlay_hints(&c.comp.ck, m)
                .into_iter()
                .filter(|h| lo <= h.offset && h.offset <= hi)
                .map(|h| {
                    let (line, col) = sf.lsp_pos(h.offset);
                    lt::InlayHint {
                        position: lt::Position::new(line, col),
                        label: lt::InlayHintLabel::String(h.label),
                        kind: Some(lt::InlayHintKind::TYPE),
                        text_edits: None,
                        tooltip: None,
                        padding_left: None,
                        padding_right: None,
                        data: None,
                    }
                })
                .collect(),
        )
    }

    fn document_symbols(&self, p: lt::DocumentSymbolParams) -> Option<lt::DocumentSymbolResponse> {
        let path = uri_to_path(&p.text_document.uri)?;
        let c = self.compile(&path, None);
        let m = query::module_of_file(&c.comp.ck, c.file)?;
        let sf = c.comp.sm.file(c.file);
        let items = symbols::outline(&c.comp.ck, &c.comp.sm, m);
        Some(lt::DocumentSymbolResponse::Nested(items.into_iter().map(|i| document_symbol(sf, i)).collect()))
    }

    /// Declarations matching the query in the open documents and the modules they import.
    fn workspace_symbols(&self, p: lt::WorkspaceSymbolParams) -> Option<lt::WorkspaceSymbolResponse> {
        let q = p.query.to_lowercase();
        let mut seen = rustc_hash::FxHashSet::default();
        let mut out = Vec::new();
        let paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for path in paths {
            let c = self.compile(&path, None);
            for (m, md) in c.comp.ck.mods.iter().enumerate() {
                if md.is_prelude {
                    continue;
                }
                let mut stack: Vec<(OutlineItem, Option<String>)> =
                    symbols::outline(&c.comp.ck, &c.comp.sm, m as u32).into_iter().map(|i| (i, None)).collect();
                while let Some((item, container)) = stack.pop() {
                    stack.extend(item.children.iter().cloned().map(|ch| (ch, Some(item.name.clone()))));
                    if !item.name.to_lowercase().contains(&q) {
                        continue;
                    }
                    let Some(loc) = Self::location(&c.comp, item.name_span) else { continue };
                    if !seen.insert((loc.uri.as_str().to_string(), loc.range.start)) {
                        continue;
                    }
                    out.push(lt::WorkspaceSymbol {
                        name: item.name,
                        kind: symbol_kind(item.kind),
                        tags: None,
                        container_name: container,
                        location: lt::OneOf::Left(loc),
                        data: None,
                    });
                }
            }
        }
        Some(lt::WorkspaceSymbolResponse::Nested(out))
    }
}

/// An argument list being typed: the position of its `(`, the commas before the cursor, and the
/// name of a named argument under the cursor (`name: ...`).
struct OpenCall {
    paren: usize,
    commas: u32,
    named: Option<String>,
    /// Closing brackets for every bracket open at the cursor, innermost first.
    closers: String,
}

/// Finds the innermost unclosed `(` before `off`, skipping strings and comments.
fn open_call(text: &str, off: usize) -> Option<OpenCall> {
    let b = text.as_bytes();
    let off = off.min(b.len());
    // (bracket, position, commas, start of the current argument)
    let mut stack: Vec<(u8, usize, u32, usize)> = Vec::new();
    let mut i = 0;
    while i < off {
        match b[i] {
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < off && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'#' => {
                while i < off && b[i] != b'\n' {
                    i += 1;
                }
            }
            c @ (b'(' | b'[' | b'{') => stack.push((c, i, 0, i + 1)),
            b')' | b']' | b'}' => {
                stack.pop();
            }
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.2 += 1;
                    top.3 = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let &(c, paren, commas, arg_start) = stack.last()?;
    if c != b'(' {
        return None;
    }
    let arg = text.get(arg_start..off)?.trim_start();
    let name_len = arg.find(|ch: char| !(ch.is_alphanumeric() || ch == '_')).unwrap_or(arg.len());
    let rest = arg[name_len..].trim_start();
    let named = (name_len > 0 && rest.starts_with(':') && !rest.starts_with("::")).then(|| arg[..name_len].to_string());
    let closers = stack
        .iter()
        .rev()
        .map(|&(c, ..)| match c {
            b'(' => ')',
            b'[' => ']',
            _ => '}',
        })
        .collect();
    Some(OpenCall { paren, commas, named, closers })
}

fn symbol_kind(k: OutlineKind) -> lt::SymbolKind {
    match k {
        OutlineKind::Function => lt::SymbolKind::FUNCTION,
        OutlineKind::Method => lt::SymbolKind::METHOD,
        OutlineKind::Class => lt::SymbolKind::CLASS,
        OutlineKind::Interface => lt::SymbolKind::INTERFACE,
        OutlineKind::Enum => lt::SymbolKind::ENUM,
        OutlineKind::Variant => lt::SymbolKind::ENUM_MEMBER,
        OutlineKind::Field => lt::SymbolKind::FIELD,
        OutlineKind::Constant => lt::SymbolKind::CONSTANT,
        OutlineKind::Variable => lt::SymbolKind::VARIABLE,
        OutlineKind::Extension => lt::SymbolKind::NAMESPACE,
    }
}

#[allow(deprecated)] // `DocumentSymbol::deprecated` must still be written.
fn document_symbol(sf: &SourceFile, i: OutlineItem) -> lt::DocumentSymbol {
    lt::DocumentSymbol {
        name: i.name,
        detail: i.detail,
        kind: symbol_kind(i.kind),
        tags: None,
        deprecated: None,
        range: Server::range(sf, i.span.start, i.span.end),
        selection_range: Server::range(sf, i.name_span.start, i.name_span.end),
        children: if i.children.is_empty() {
            None
        } else {
            Some(i.children.into_iter().map(|c| document_symbol(sf, c)).collect())
        },
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

fn parse<T: serde::de::DeserializeOwned>(v: Json) -> Result<T, (ErrorCode, String)> {
    serde_json::from_value(v).map_err(|e| (ErrorCode::InvalidParams, e.to_string()))
}

/// A handler's refusal (e.g. a symbol that cannot be renamed) as a failed request.
fn failed<T: serde::Serialize>(r: Result<T, String>) -> Result<Json, (ErrorCode, String)> {
    r.map(to_json).map_err(|e| (ErrorCode::RequestFailed, e))
}

fn to_json<T: serde::Serialize>(v: T) -> Json {
    serde_json::to_value(v).unwrap_or(Json::Null)
}
