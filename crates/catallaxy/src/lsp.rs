//! `catx lsp`: a language server over stdio.
//!
//! Every open buffer is kept in memory and overlays the file system, so imports of unsaved
//! buffers see their current text. Each change re-checks the open documents (type checking only,
//! no bytecode) and publishes their diagnostics. Hover, go to definition and completion compile
//! the requested document on demand and query the checker's side tables (see
//! `catallaxy_compiler::query`). References, rename, signature help, inlay hints and symbol lists
//! use the symbol index and outlines of `catallaxy_compiler::symbols`. References, rename and
//! `workspace/symbol` cover the workspace (see [`workspace`]): every `.cx` file under the
//! workspace folders and every open document, each compiled with the modules it imports and
//! cached; the cache is warmed in the background at startup and when folders change. They
//! match symbols across those compilations by their declarations, and take along the methods that
//! implement an interface member in the types used as that interface. Code actions offer the fixes
//! attached to the document's diagnostics as quick fixes.

mod workspace;

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use catallaxy_compiler::diag::{Severity, SourceFile};
use catallaxy_compiler::driver::{compile_path, Compilation, Options, OverlayLoader, PRELUDE};
use catallaxy_compiler::query::{self, CompletionKind as QK};
use catallaxy_compiler::symbols::{self, OutlineItem, OutlineKind, Symbol};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types as lt;
use rustc_hash::FxHashMap;
use serde_json::Value as Json;
use workspace::{Notes, Overlay, Unit, Workspace, ENTRY_FILE};

pub fn cmd_lsp(args: &[String]) -> i32 {
    // Clients that launch servers over stdio often say so with `--stdio`.
    if let Some(a) = args.iter().find(|a| *a != "--stdio") {
        return crate::unexpected("lsp", a);
    }
    let (conn, io) = Connection::stdio();
    // Requests walk syntax trees as deep as the parser allows: serve them on the compiler's stack.
    let code = match catallaxy_compiler::util::on_compiler_stack(|| serve(&conn).map_err(|e| e.to_string())) {
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
        workspace: Some(lt::WorkspaceServerCapabilities {
            workspace_folders: Some(lt::WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(lt::OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        ..Default::default()
    }
}

struct Server<'a> {
    conn: &'a Connection,
    /// Open documents by file path: (uri, text).
    docs: FxHashMap<PathBuf, (lt::Uri, String)>,
    /// The workspace folders, their files and the cached compilations of references and rename.
    ws: Arc<Workspace>,
    /// Whether the client accepts `window/workDoneProgress` (for the warm-up).
    progress: bool,
}

/// The id of the request registering the file watcher.
const WATCH_REQUEST: &str = "catx/watch-files";

/// The workspace folders named by `initialize` (`workspaceFolders`, else `rootUri`, else
/// `rootPath`).
fn initial_roots(params: &Json) -> Vec<PathBuf> {
    if let Some(folders) = params["workspaceFolders"].as_array() {
        return folders.iter().filter_map(|f| folder_path(&f["uri"])).collect();
    }
    if let Some(p) = folder_path(&params["rootUri"]) {
        return vec![p];
    }
    params["rootPath"].as_str().map(PathBuf::from).into_iter().collect()
}

fn folder_path(uri: &Json) -> Option<PathBuf> {
    uri_to_path(&lt::Uri::from_str(uri.as_str()?).ok()?)
}

fn serve(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let caps = serde_json::to_value(capabilities())?;
    let init = serde_json::json!({
        "capabilities": caps,
        "serverInfo": { "name": "catx", "version": env!("CARGO_PKG_VERSION") },
    });
    let (id, params) = conn.initialize_start()?;
    conn.initialize_finish(id, init)?;
    // Ask to hear of created and deleted files, so the list of workspace files can be kept
    // between requests (otherwise it is rebuilt for each).
    let watching = params["capabilities"]["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"] == true;
    if watching {
        let reg = lt::RegistrationParams {
            registrations: vec![lt::Registration {
                id: "catx-watch-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(serde_json::json!({
                    "watchers": [{ "globPattern": "**/*", "kind": 5 }] // create | delete
                })),
            }],
        };
        let req = Request::new(WATCH_REQUEST.to_string().into(), "client/registerCapability".into(), reg);
        conn.sender.send(Message::Request(req))?;
    }
    let ws = Arc::new(Workspace::new(initial_roots(&params), watching));
    let progress = params["capabilities"]["window"]["workDoneProgress"] == true;
    let mut s = Server { conn, docs: FxHashMap::default(), ws, progress };
    s.warm();
    let r = s.serve();
    // Wait for the warm-up's threads, which hold senders of the connection.
    s.ws.stop();
    r
}

impl Server<'_> {
    fn serve(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.conn;
        for msg in &conn.receiver {
            match msg {
                Message::Request(req) => {
                    if req.method == "shutdown" {
                        self.ws.stop();
                    }
                    if conn.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    let id = req.id.clone();
                    let resp = match std::panic::catch_unwind(AssertUnwindSafe(|| self.request(req))) {
                        Ok(r) => r,
                        Err(_) => Response::new_err(id, ErrorCode::InternalError as i32, "internal error".into()),
                    };
                    conn.sender.send(Message::Response(resp))?;
                }
                Message::Notification(n) => {
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.notification(n)));
                }
                Message::Response(r) => {
                    let id = serde_json::to_value(&r.id).unwrap_or_default();
                    let id = id.as_str().unwrap_or_default();
                    if id == WATCH_REQUEST && r.error.is_some() {
                        self.ws.stop_watching();
                    } else if id.starts_with(workspace::PROGRESS_REQUEST) {
                        self.ws.progress_answer(id.to_string(), r.error.is_none());
                    }
                }
            }
        }
        Ok(())
    }

    /// Warms the workspace cache in the background (see `Workspace::warm`).
    fn warm(&self) {
        let tx = self.conn.sender.clone();
        self.ws.warm(Arc::new(move |m| drop(tx.send(m))), self.progress);
    }
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

// ---- the prelude on disk -------------------------------------------------------------------
//
// The prelude is compiled into `catx`; its source files are named `<prelude>/core.cx` and so on,
// which no editor can open. Locations in it (go to definition of `puts`, a diagnostic's note)
// point to a copy written once to a cache directory named after the version and the prelude's
// content, read-only.

/// The user's cache directory: `XDG_CACHE_HOME` if set, else the platform's.
fn cache_home() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(d) = var("XDG_CACHE_HOME") {
        return Some(d);
    }
    if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Caches"))
    } else {
        var("HOME").map(|h| h.join(".cache"))
    }
}

/// The directory holding the prelude's sources, written on first use (`None` if it cannot be).
fn prelude_dir() -> Option<&'static Path> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        PRELUDE.hash(&mut h);
        let dir =
            cache_home()?.join("catallaxy").join(format!("prelude-{}-{:016x}", env!("CARGO_PKG_VERSION"), h.finish()));
        std::fs::create_dir_all(&dir).ok()?;
        for (name, text) in PRELUDE {
            let p = dir.join(name.rsplit('/').next()?);
            if std::fs::read_to_string(&p).is_ok_and(|t| t == *text) {
                continue;
            }
            // Written next to it and renamed, so a reader never sees half a file.
            let tmp = dir.join(format!(".{}.{}", std::process::id(), name.rsplit('/').next()?));
            std::fs::write(&tmp, text).ok()?;
            let mut perm = std::fs::metadata(&tmp).ok()?.permissions();
            perm.set_readonly(true);
            let _ = std::fs::set_permissions(&tmp, perm);
            if std::fs::rename(&tmp, &p).is_err() {
                let _ = std::fs::remove_file(&tmp);
                // Another server may have written it meanwhile.
                if !std::fs::read_to_string(&p).is_ok_and(|t| t == *text) {
                    return None;
                }
            }
        }
        Some(dir)
    })
    .as_deref()
}

/// The on-disk copy of prelude file `file` (an index into `PRELUDE`).
fn prelude_path(file: u32) -> Option<PathBuf> {
    let (name, _) = PRELUDE.get(file as usize)?;
    Some(prelude_dir()?.join(name.rsplit('/').next()?))
}

/// Whether `p` is a file of the prelude's on-disk copy (opened from a location in it): it is not
/// a program, so it gets no diagnostics and takes no part in the workspace.
fn is_prelude_copy(p: &Path) -> bool {
    let Some(parent) = p.parent() else { return false };
    let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) else { return false };
    dir_name.starts_with("prelude-")
        && parent.parent().and_then(|g| g.file_name()) == Some(std::ffi::OsStr::new("catallaxy"))
        && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| PRELUDE.iter().any(|(q, _)| q.ends_with(n)))
}

/// Whether a request or notification is about a document of the prelude's on-disk copy.
fn on_prelude_copy(params: &Json) -> bool {
    params["textDocument"]["uri"]
        .as_str()
        .and_then(|u| lt::Uri::from_str(u).ok())
        .and_then(|u| uri_to_path(&u))
        .is_some_and(|p| is_prelude_copy(&p))
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
        let uri = if (span.file as usize) < PRELUDE.len() {
            path_to_uri(&prelude_path(span.file)?)?
        } else {
            path_to_uri(Path::new(&sf.path))?
        };
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
        // The warm-up pauses meanwhile.
        let _busy = self.ws.busy();
        let paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for p in paths {
            self.publish(&p);
        }
    }

    fn notify(&self, method: &str, params: impl serde::Serialize) {
        let n = Notification::new(method.to_string(), params);
        let _ = self.conn.sender.send(Message::Notification(n));
    }

    /// Sends the workspace's log lines to the client's log.
    fn log(&self, notes: Notes) {
        for w in notes.warnings {
            let params = lt::LogMessageParams { typ: lt::MessageType::WARNING, message: w };
            self.notify("window/logMessage", params);
        }
        for d in notes.debug {
            let params = lt::LogMessageParams { typ: lt::MessageType::LOG, message: d };
            self.notify("window/logMessage", params);
        }
    }

    /// The open documents changed: they overlay the disk for the workspace.
    fn docs_changed(&self) {
        self.ws.set_overlay(Overlay::new(self.docs.iter().map(|(p, (_, t))| (p, t))));
    }

    // ---- notifications -----------------------------------------------------------------------

    fn notification(&mut self, n: Notification) {
        // The prelude's copy, opened from a location in it, is not a program of the workspace.
        if on_prelude_copy(&n.params) {
            return;
        }
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let Ok(p) = serde_json::from_value::<lt::DidOpenTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                self.docs.insert(path, (p.text_document.uri, p.text_document.text));
                self.docs_changed();
                self.publish_all();
            }
            "textDocument/didChange" => {
                let Ok(p) = serde_json::from_value::<lt::DidChangeTextDocumentParams>(n.params) else { return };
                let Some(path) = uri_to_path(&p.text_document.uri) else { return };
                // Full sync: the last change holds the whole text.
                if let Some(c) = p.content_changes.into_iter().last() {
                    self.docs.insert(path, (p.text_document.uri, c.text));
                }
                self.docs_changed();
                self.publish_all();
            }
            "textDocument/didSave" => self.publish_all(),
            "textDocument/didClose" => {
                let Ok(p) = serde_json::from_value::<lt::DidCloseTextDocumentParams>(n.params) else { return };
                if let Some(path) = uri_to_path(&p.text_document.uri) {
                    self.docs.remove(&path);
                }
                self.docs_changed();
                let params =
                    lt::PublishDiagnosticsParams { uri: p.text_document.uri, diagnostics: Vec::new(), version: None };
                self.notify("textDocument/publishDiagnostics", params);
                self.publish_all();
            }
            "workspace/didChangeWorkspaceFolders" => {
                let Ok(p) = serde_json::from_value::<lt::DidChangeWorkspaceFoldersParams>(n.params) else { return };
                self.ws.set_roots(|roots| {
                    for f in p.event.removed {
                        if let Some(path) = uri_to_path(&f.uri) {
                            roots.retain(|r| *r != path);
                        }
                    }
                    for f in p.event.added {
                        if let Some(path) = uri_to_path(&f.uri) {
                            if !roots.contains(&path) {
                                roots.push(path);
                            }
                        }
                    }
                });
                self.warm();
            }
            // Contents are checked by each request; only the list of files can change here.
            "workspace/didChangeWatchedFiles" => self.ws.rescan(),
            _ => {}
        }
    }

    // ---- requests ----------------------------------------------------------------------------

    fn request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        if on_prelude_copy(&req.params) {
            return Response::new_ok(id, Json::Null);
        }
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
        let (text, span) = query::hover(&c.comp.ck, &c.comp.sm, m, off)?;
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
                documentation: c.doc.map(|d| {
                    lt::Documentation::MarkupContent(lt::MarkupContent { kind: lt::MarkupKind::Markdown, value: d })
                }),
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

/// The symbol under a cursor, with the unit of the cursor's document.
struct AtSymbol {
    path: PathBuf,
    unit: Arc<Unit>,
    sym: Symbol,
    /// The occurrence under the cursor.
    span: catallaxy_compiler::diag::Span,
}

/// Everything a rename of the symbol under a cursor rewrites, across compilations.
struct Gathered {
    /// Each occurrence once: its location, whether it declares its symbol, and its position as a
    /// key (canonical path and byte range).
    occs: Vec<(lt::Location, bool, Key)>,
    /// Why the rename must be refused, if it must.
    refusal: Option<String>,
    /// The units searched, by entry path; the cursor's document's first.
    units: Vec<(PathBuf, Arc<Unit>)>,
}

type Key = (PathBuf, u32, u32);

/// A file a rename edits: its path as loaded, its text, and the byte ranges replaced.
type EditedFile = (PathBuf, String, Vec<(u32, u32)>);

impl Server<'_> {
    /// The units of `first` and then, with `all`, of the open documents and the workspace files.
    fn units(&self, first: Option<&Path>, all: bool) -> Vec<(PathBuf, Arc<Unit>)> {
        let mut entries: Vec<PathBuf> = first.map(Path::to_path_buf).into_iter().collect();
        if all {
            let mut open: Vec<PathBuf> = self.docs.keys().cloned().collect();
            open.sort();
            entries.extend(open);
        }
        let mut notes = Notes::default();
        let units = self.ws.units(entries, all, &mut notes);
        self.log(notes);
        units
    }

    fn unit_location(u: &Unit, span: catallaxy_compiler::diag::Span) -> Option<lt::Location> {
        let f = u.file(span.file)?;
        let uri = path_to_uri(Path::new(&f.sf.path))?;
        Some(lt::Location::new(uri, Self::range(&f.sf, span.start, span.end)))
    }

    fn symbol_at(&self, pos: &lt::TextDocumentPositionParams) -> Option<AtSymbol> {
        let (path, _, off) = self.locate(pos)?;
        let (_, unit) = self.units(Some(&path), false).into_iter().next()?;
        let occ = *unit.index.at(ENTRY_FILE, off)?;
        Some(AtSymbol { path, unit, sym: occ.sym, span: occ.span })
    }

    /// The occurrences of `at`'s symbol and of the symbols renamed with it (the methods that
    /// implement an interface member, see `Index::family`), in every compilation of the
    /// workspace: those of the open documents and of the files under the workspace folders.
    /// Symbols are matched across compilations by their declarations.
    fn gather(&self, at: AtSymbol) -> Gathered {
        let AtSymbol { path, sym, .. } = at;
        let entries = self.units(Some(&path), true);
        let units: Vec<Arc<Unit>> = entries.iter().map(|(_, u)| u.clone()).collect();
        if units.is_empty() {
            return Gathered { occs: Vec::new(), refusal: None, units: entries };
        }
        // The symbols of the rename in each unit.
        let mut syms: Vec<Vec<Symbol>> = vec![Vec::new(); units.len()];
        syms[0] = units[0].index.family(sym);
        let cursor_key = units[0].decl_key(sym);
        // Each unit's declarations by key.
        let decl_at: Vec<FxHashMap<Key, Vec<Symbol>>> = units
            .iter()
            .map(|u| {
                let mut m: FxHashMap<Key, Vec<Symbol>> = FxHashMap::default();
                for o in u.index.occs.iter().filter(|o| o.is_decl) {
                    if let Some(k) = u.key(o.span) {
                        m.entry(k).or_default().push(o.sym);
                    }
                }
                m
            })
            .collect();
        let mut keys: rustc_hash::FxHashSet<Key> = Default::default();
        let mut grew = true;
        while grew {
            grew = false;
            for (i, u) in units.iter().enumerate() {
                let found: Vec<Symbol> = keys
                    .iter()
                    .filter_map(|k| decl_at[i].get(k))
                    .flatten()
                    .copied()
                    .filter(|s| !syms[i].contains(s))
                    .collect();
                for s in found {
                    for f in u.index.family(s) {
                        if !syms[i].contains(&f) {
                            syms[i].push(f);
                        }
                    }
                }
                for s in &syms[i] {
                    if let Some(k) = u.decl_key(*s) {
                        grew |= keys.insert(k);
                    }
                }
            }
        }
        let mut refusal = None;
        let mut seen = rustc_hash::FxHashSet::default();
        let mut occs = Vec::new();
        for (i, u) in units.iter().enumerate() {
            for &s in &syms[i] {
                if refusal.is_some() {
                    break;
                }
                if let Some((name, sig)) = u.fixed.get(&s) {
                    let is_cursor = if i == 0 { s == sym } else { cursor_key.is_some() && u.decl_key(s) == cursor_key };
                    refusal = Some(match sig {
                        Some(sig) if !is_cursor => format!(
                            "renaming `{name}` would also rename `{sig}`, which is not declared in this program (it is built in or from the prelude)"
                        ),
                        _ => format!("`{name}` is not declared in this program (it is built in or from the prelude)"),
                    });
                }
            }
            for o in u.index.occs.iter().filter(|o| syms[i].contains(&o.sym)) {
                let (Some(k), Some(loc)) = (u.key(o.span), Self::unit_location(u, o.span)) else { continue };
                if seen.insert(k.clone()) {
                    occs.push((loc, o.is_decl, k));
                }
            }
        }
        Gathered { occs, refusal, units: entries }
    }

    fn references(&self, p: lt::ReferenceParams) -> Option<Vec<lt::Location>> {
        let at = self.symbol_at(&p.text_document_position)?;
        let with_decl = p.context.include_declaration;
        Some(self.gather(at).occs.into_iter().filter(|(_, d, _)| with_decl || !d).map(|(l, ..)| l).collect())
    }

    fn prepare_rename(&self, p: lt::TextDocumentPositionParams) -> Result<Option<lt::PrepareRenameResponse>, String> {
        let Some(at) = self.symbol_at(&p) else { return Ok(None) };
        let Some(f) = at.unit.file(ENTRY_FILE) else { return Ok(None) };
        let range = Self::range(&f.sf, at.span.start, at.span.end);
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
        // The new name must not collide with a name visible where the symbol is declared or used.
        let (path, offset) = (at.path.clone(), at.span.start);
        if let Some(why) = self.scope_conflict(&path, offset, new_name) {
            return Err(why);
        }
        let g = self.gather(at);
        if let Some(why) = g.refusal {
            return Err(why);
        }
        // And, to catch what that misses, the renamed program must check as the original did.
        if let Some(why) = self.recheck(&g, new_name) {
            return Err(why);
        }
        let mut changes: std::collections::HashMap<lt::Uri, Vec<lt::TextEdit>> = Default::default();
        for (loc, ..) in g.occs {
            changes.entry(loc.uri).or_default().push(lt::TextEdit { range: loc.range, new_text: new_name.into() });
        }
        Ok(Some(lt::WorkspaceEdit { changes: Some(changes), ..Default::default() }))
    }

    /// Why renaming the symbol at `offset` of the document at `path` to `new` would clash with an
    /// existing name (see `symbols::rename_conflict`), checked in that document's compilation.
    fn scope_conflict(&self, path: &Path, offset: u32, new: &str) -> Option<String> {
        let c = self.compile(path, None);
        let (ck, sm) = (&c.comp.ck, &c.comp.sm);
        let index = symbols::Index::build(ck, sm);
        let occ = index.at(c.file, offset)?;
        index.family(occ.sym).into_iter().find_map(|s| symbols::rename_conflict(ck, sm, &index, s, new))
    }

    /// Checks the program with the rename applied: every compilation that includes an edited file
    /// must report no diagnostic the original did not, and the renamed occurrences must be
    /// exactly the occurrences of their symbols (no other name now means the renamed thing, and
    /// no renamed occurrence now means something else). Why not, if it fails.
    fn recheck(&self, g: &Gathered, new: &str) -> Option<String> {
        if g.occs.is_empty() {
            return None;
        }
        // Each edited file's text (as the compilations saw it) and its edits, by canonical path.
        let mut by_file: FxHashMap<PathBuf, EditedFile> = FxHashMap::default();
        for (_, _, (canon, start, end)) in &g.occs {
            if !by_file.contains_key(canon) {
                let Some(f) = g.units.iter().find_map(|(_, u)| u.files.iter().flatten().find(|f| &f.canon == canon))
                else {
                    continue;
                };
                by_file.insert(canon.clone(), (PathBuf::from(&f.sf.path), f.sf.text.clone(), Vec::new()));
            }
            if let Some(e) = by_file.get_mut(canon) {
                e.2.push((*start, *end));
            }
        }
        let mut texts: FxHashMap<PathBuf, String> = FxHashMap::default();
        let mut renamed: rustc_hash::FxHashSet<Key> = Default::default();
        let mut old_name = String::new();
        for (canon, (path, text, edits)) in &mut by_file {
            edits.sort_unstable();
            edits.dedup();
            let mut out = String::with_capacity(text.len());
            let mut last = 0usize;
            for &(a, b) in edits.iter() {
                let (a, b) = (a as usize, b as usize);
                if a < last || b > text.len() {
                    return Some("the rename's edits overlap".into());
                }
                if old_name.is_empty() {
                    old_name = text[a..b].to_string();
                }
                out.push_str(&text[last..a]);
                let start = out.len() as u32;
                out.push_str(new);
                renamed.insert((canon.clone(), start, out.len() as u32));
                last = b;
            }
            out.push_str(&text[last..]);
            texts.insert(path.clone(), out);
        }
        // The compilations that include an edited file.
        let entries: Vec<PathBuf> = g
            .units
            .iter()
            .filter(|(_, u)| u.files.iter().flatten().any(|f| by_file.contains_key(&f.canon)))
            .map(|(p, _)| p.clone())
            .collect();
        let before: FxHashMap<&PathBuf, &Arc<Unit>> = g.units.iter().map(|(p, u)| (p, u)).collect();
        let place = |u: &Unit, file: u32, offset: u32| match u.file(file) {
            Some(f) => {
                let (line, _) = f.sf.line_col(offset);
                let name = Path::new(&f.sf.path).file_name().map(|n| n.to_string_lossy().to_string());
                format!("{}:{line}", name.unwrap_or_default())
            }
            None => "?".into(),
        };
        let mut compiled = 0;
        for (entry, after) in self.ws.trial(&texts, &entries) {
            compiled += 1;
            let Some(after) = after else {
                return Some(format!("renaming to `{new}` could not be checked: compiling {} failed", entry.display()));
            };
            let Some(old) = before.get(&entry) else { continue };
            // New diagnostics: a code reported more often than before.
            let mut counts: FxHashMap<&str, i64> = FxHashMap::default();
            for (code, ..) in &old.diags {
                *counts.entry(code).or_default() += 1;
            }
            for (code, msg, sp) in &after.diags {
                let n = counts.entry(code).or_default();
                *n -= 1;
                if *n < 0 {
                    let kind = if code.starts_with('W') { "warning" } else { "error" };
                    let at = place(&after, sp.file, sp.start);
                    return Some(format!("renaming `{old_name}` to `{new}` would cause {kind}[{code}] at {at}: {msg}"));
                }
            }
            // The renamed occurrences, and every occurrence of their symbols.
            let files: rustc_hash::FxHashSet<&PathBuf> = after.files.iter().flatten().map(|f| &f.canon).collect();
            let mut syms: Vec<Symbol> = Vec::new();
            let mut found: rustc_hash::FxHashSet<Key> = Default::default();
            for o in &after.index.occs {
                if let Some(k) = after.key(o.span).filter(|k| renamed.contains(k)) {
                    if !syms.contains(&o.sym) {
                        syms.push(o.sym);
                    }
                    found.insert(k);
                }
            }
            if let Some(k) = renamed.iter().find(|k| files.contains(&k.0) && !found.contains(*k)) {
                let file = after.files.iter().position(|f| f.as_ref().is_some_and(|f| f.canon == k.0)).unwrap_or(0);
                let at = place(&after, file as u32, k.1);
                return Some(format!(
                    "renaming `{old_name}` to `{new}` would leave the `{new}` at {at} naming nothing"
                ));
            }
            // Several symbols may be renamed together (an interface member and its
            // implementations, which one compilation may not link): each must keep to the edits.
            for o in after.index.occs.iter().filter(|o| syms.contains(&o.sym)) {
                if after.key(o.span).is_some_and(|k| !renamed.contains(&k)) {
                    let at = place(&after, o.span.file, o.span.start);
                    return Some(format!(
                        "renaming `{old_name}` to `{new}` would change what the `{new}` at {at} refers to"
                    ));
                }
            }
        }
        if self.ws.debug() {
            let line = format!("catx lsp: checking the rename compiled {compiled} files");
            self.log(Notes { debug: vec![line], ..Default::default() });
        }
        None
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

    /// Declarations matching the query in the workspace: the files under the workspace folders,
    /// the open documents, and the modules they import.
    fn workspace_symbols(&self, p: lt::WorkspaceSymbolParams) -> Option<lt::WorkspaceSymbolResponse> {
        let q = p.query.to_lowercase();
        let mut seen = rustc_hash::FxHashSet::default();
        let mut out = Vec::new();
        for (_, u) in self.units(None, true) {
            for item in &u.outline {
                if !item.name.to_lowercase().contains(&q) {
                    continue;
                }
                let Some(k) = u.key(item.name_span) else { continue };
                if !seen.insert(k) {
                    continue;
                }
                let Some(loc) = Self::unit_location(&u, item.name_span) else { continue };
                out.push(lt::WorkspaceSymbol {
                    name: item.name.clone(),
                    kind: symbol_kind(item.kind),
                    tags: None,
                    container_name: item.container.clone(),
                    location: lt::OneOf::Left(loc),
                    data: None,
                });
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
