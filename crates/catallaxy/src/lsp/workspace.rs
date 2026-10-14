//! The workspace as the language server sees it for references, rename and `workspace/symbol`:
//! every `.cx` file under the workspace folders plus the open documents, each compiled as an
//! entry (with the modules it imports).
//!
//! A compilation is kept only as a [`Unit`]: its symbol index, the declaration of each symbol,
//! the rename refusals, the outline and the source files, not the checker itself. Units are
//! cached by entry path and stay valid while every file their compilation tried to load (a found
//! module, or a candidate path that did not exist) still has the same content hash; a request
//! recompiles only the units that fail that check, in parallel. Disk files are re-read only when
//! their modification time or size changes. Open documents' text takes precedence over the disk.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use catallaxy_compiler::diag::{SourceFile, Span};
use catallaxy_compiler::driver::{compile_path, Compilation, Options, SourceLoader, PRELUDE};
use catallaxy_compiler::query;
use catallaxy_compiler::symbols::{self, Index, OutlineKind, Symbol};
use rustc_hash::{FxHashMap, FxHashSet};

/// The most `.cx` files taken from the workspace folders (`CATX_LSP_MAX_FILES` overrides it).
const DEFAULT_MAX_FILES: usize = 2000;

/// Directories never searched for sources (besides hidden ones).
const SKIPPED_DIRS: &[&str] = &["target", "node_modules"];

/// A source file of a unit's compilation.
pub struct FileData {
    pub sf: SourceFile,
    /// The canonical path, which identifies the file across compilations.
    pub canon: PathBuf,
}

/// A declaration for `workspace/symbol`.
pub struct WsItem {
    pub name: String,
    pub kind: OutlineKind,
    pub container: Option<String>,
    pub name_span: Span,
}

/// What references, rename and workspace symbols need of one compilation.
pub struct Unit {
    /// Every path the compilation tried to load and the content hash it found (`None`: absent).
    deps: Vec<(PathBuf, Option<u64>)>,
    /// Source files by file id (`None` for the prelude).
    pub files: Vec<Option<FileData>>,
    pub index: Index,
    /// Declaration of each indexed symbol that has one.
    pub decls: FxHashMap<Symbol, Span>,
    /// Symbols that cannot be renamed (not declared in the program): their name, and a
    /// function's signature.
    pub fixed: FxHashMap<Symbol, (String, Option<String>)>,
    /// Declarations of the user modules, with their containers.
    pub outline: Vec<WsItem>,
}

/// The file id of a compilation's entry document (added right after the prelude).
pub const ENTRY_FILE: u32 = PRELUDE.len() as u32;

impl Unit {
    fn new(comp: Compilation, deps: Vec<(PathBuf, Option<u64>)>) -> Unit {
        let (ck, sm) = (&comp.ck, &comp.sm);
        let index = Index::build(ck, sm);
        let mut syms: FxHashSet<Symbol> = index.occs.iter().map(|o| o.sym).collect();
        for (k, v) in &index.links {
            syms.insert(*k);
            syms.extend(v.iter().copied());
        }
        let declared: FxHashSet<Symbol> = index.occs.iter().filter(|o| o.is_decl).map(|o| o.sym).collect();
        let mut decls = FxHashMap::default();
        let mut fixed = FxHashMap::default();
        for &s in &syms {
            let d = symbols::decl_span(ck, s);
            if let Some(sp) = d {
                decls.insert(s, sp);
            }
            // As `Index::renamable`.
            if !(d.is_some_and(|sp| index.user_files.contains(&sp.file)) && declared.contains(&s)) {
                let name = symbols::symbol_name(ck, sm, s).unwrap_or_default();
                let sig = match s {
                    Symbol::Fn(f) => Some(query::fn_signature(ck, f)),
                    _ => None,
                };
                fixed.insert(s, (name, sig));
            }
        }
        let mut outline = Vec::new();
        for (m, md) in ck.mods.iter().enumerate() {
            if md.is_prelude {
                continue;
            }
            let mut stack: Vec<_> = symbols::outline(ck, sm, m as u32).into_iter().map(|i| (i, None)).collect();
            while let Some((item, container)) = stack.pop() {
                let name = item.name;
                stack.extend(item.children.into_iter().map(|ch| (ch, Some(name.clone()))));
                outline.push(WsItem { name, kind: item.kind, container, name_span: item.name_span });
            }
        }
        let files = comp
            .sm
            .files
            .into_iter()
            .enumerate()
            .map(|(i, sf)| {
                (i >= PRELUDE.len()).then(|| {
                    let p = Path::new(&sf.path);
                    let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                    FileData { sf, canon }
                })
            })
            .collect();
        Unit { deps, files, index, decls, fixed, outline }
    }

    pub fn file(&self, f: u32) -> Option<&FileData> {
        self.files.get(f as usize)?.as_ref()
    }

    /// A span as a position independent of the compilation's file numbering.
    pub fn key(&self, sp: Span) -> Option<(PathBuf, u32, u32)> {
        Some((self.file(sp.file)?.canon.clone(), sp.start, sp.end))
    }

    /// The declaration of `s` as a key.
    pub fn decl_key(&self, s: Symbol) -> Option<(PathBuf, u32, u32)> {
        self.key(*self.decls.get(&s)?)
    }
}

fn hash_text(t: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

/// Open documents' text by path, with their hashes.
pub struct Overlay {
    texts: FxHashMap<PathBuf, (String, u64)>,
    /// Canonical path of each document whose file exists, for files reached by another path.
    canon: FxHashMap<PathBuf, PathBuf>,
    /// File names of the documents, so only a path with one of them needs canonicalizing.
    names: FxHashSet<std::ffi::OsString>,
}

impl Overlay {
    pub fn new<'a>(docs: impl Iterator<Item = (&'a PathBuf, &'a String)>) -> Overlay {
        let mut o = Overlay { texts: Default::default(), canon: Default::default(), names: Default::default() };
        for (p, t) in docs {
            o.texts.insert(p.clone(), (t.clone(), hash_text(t)));
            if let Ok(c) = std::fs::canonicalize(p) {
                if &c != p {
                    o.canon.insert(c, p.clone());
                }
            }
            if let Some(n) = p.file_name() {
                o.names.insert(n.to_owned());
            }
        }
        o
    }

    fn get(&self, p: &Path) -> Option<&(String, u64)> {
        if let Some(t) = self.texts.get(p) {
            return Some(t);
        }
        if !p.file_name().is_some_and(|n| self.names.contains(n)) {
            return None;
        }
        let c = std::fs::canonicalize(p).ok()?;
        self.texts.get(&c).or_else(|| self.texts.get(self.canon.get(&c)?))
    }
}

type Stamp = (SystemTime, u64);

fn stamp(p: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(p).ok()?;
    m.is_file().then(|| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

/// A path a compilation asked for: the content hash found (`None`: absent) and, for a disk
/// file, its stamp.
type Load = (PathBuf, Option<u64>, Option<Stamp>);

/// A loader that reads open documents from the overlay and records every path asked for.
struct Recording<'a> {
    overlay: &'a Overlay,
    log: RefCell<Vec<Load>>,
}

impl SourceLoader for Recording<'_> {
    fn load(&self, path: &Path) -> Option<String> {
        if let Some((t, h)) = self.overlay.get(path) {
            self.log.borrow_mut().push((path.to_path_buf(), Some(*h), None));
            return Some(t.clone());
        }
        let st = stamp(path);
        let text = st.and_then(|_| std::fs::read_to_string(path).ok());
        self.log.borrow_mut().push((path.to_path_buf(), text.as_deref().map(hash_text), st));
        text
    }
}

/// The workspace folders, the files found under them, and the cached units.
#[derive(Default)]
pub struct Workspace {
    pub roots: Vec<PathBuf>,
    /// Whether the client reports created and deleted files, so the file list can be kept.
    pub watching: bool,
    /// The `.cx` files under the roots, and whether the list was capped (`None`: walk again).
    walked: Option<(Vec<PathBuf>, bool)>,
    /// The cap last reported in the log, so it is reported once.
    reported_cap: Option<usize>,
    units: FxHashMap<PathBuf, Arc<Unit>>,
    /// Disk files' stamps and content hashes.
    hashes: FxHashMap<PathBuf, (Stamp, u64)>,
}

/// What a request of the workspace produced besides units: log lines for the client.
#[derive(Default)]
pub struct Notes {
    pub warnings: Vec<String>,
}

fn max_files() -> usize {
    std::env::var("CATX_LSP_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_FILES)
}

/// The `.cx` files under `roots` in path order, at most `cap` (and whether there were more).
fn walk(roots: &[PathBuf], cap: usize) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut seen = FxHashSet::default();
    let mut stack: Vec<PathBuf> = roots.iter().rev().cloned().collect();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for e in entries {
            let name = e.file_name();
            let n = name.to_string_lossy();
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                if !n.starts_with('.') && !SKIPPED_DIRS.contains(&n.as_ref()) {
                    subdirs.push(p);
                }
            } else if n.ends_with(".cx") && !n.starts_with('.') && (ft.is_file() || p.is_file()) {
                if out.len() == cap {
                    return (out, true);
                }
                if seen.insert(p.clone()) {
                    out.push(p);
                }
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }
    (out, false)
}

impl Workspace {
    /// Forget the file list (folders changed, or files were created or deleted).
    pub fn rescan(&mut self) {
        self.walked = None;
    }

    fn files(&mut self, notes: &mut Notes) -> Vec<PathBuf> {
        if self.roots.is_empty() {
            return Vec::new();
        }
        let cap = max_files();
        let (files, capped) = match self.walked.take() {
            Some(w) if self.watching => w,
            _ => walk(&self.roots, cap),
        };
        if capped && self.reported_cap != Some(cap) {
            notes.warnings.push(format!(
                "catx lsp: the workspace has more than {cap} .cx files; references, rename and workspace symbols \
                 cover the first {cap} (set CATX_LSP_MAX_FILES to change the limit)"
            ));
        }
        self.reported_cap = capped.then_some(cap);
        self.walked = Some((files.clone(), capped));
        files
    }

    /// The current content hash of `p` (the overlay's text, else the disk's).
    fn current(&mut self, overlay: &Overlay, memo: &mut FxHashMap<PathBuf, Option<u64>>, p: &Path) -> Option<u64> {
        if let Some(h) = memo.get(p) {
            return *h;
        }
        let h = match overlay.get(p) {
            Some((_, h)) => Some(*h),
            None => match stamp(p) {
                None => None,
                Some(st) => match self.hashes.get(p) {
                    Some(&(s, h)) if s == st => Some(h),
                    _ => {
                        let h = std::fs::read_to_string(p).ok().map(|t| hash_text(&t));
                        if let Some(h) = h {
                            self.hashes.insert(p.to_path_buf(), (st, h));
                        }
                        h
                    }
                },
            },
        };
        memo.insert(p.to_path_buf(), h);
        h
    }

    /// Units for `entries` (in that order, duplicates dropped), compiling the ones that are
    /// missing or out of date. With `all`, the workspace files follow the given entries and
    /// cached units of other entries are dropped.
    pub fn units(
        &mut self,
        overlay: &Overlay,
        entries: Vec<PathBuf>,
        all: bool,
        notes: &mut Notes,
    ) -> Vec<(PathBuf, Arc<Unit>)> {
        let mut list = entries;
        if all {
            list.extend(self.files(notes));
        }
        // One unit per file, however it is reached.
        let mut seen = FxHashSet::default();
        list.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
        let mut memo = FxHashMap::default();
        let mut stale = Vec::new();
        for p in &list {
            let ok = match self.units.get(p).cloned() {
                Some(u) => u.deps.iter().all(|(d, h)| self.current(overlay, &mut memo, d) == *h),
                None => false,
            };
            if !ok {
                stale.push(p.clone());
            }
        }
        if !stale.is_empty() {
            for (p, built) in compile_all(overlay, &stale) {
                match built {
                    Some((u, stamps)) => {
                        self.hashes.extend(stamps);
                        self.units.insert(p, Arc::new(u));
                    }
                    None => {
                        notes.warnings.push(format!("catx lsp: compiling {} failed", p.display()));
                        self.units.remove(&p);
                    }
                }
            }
        }
        if all {
            let keep: FxHashSet<&PathBuf> = list.iter().collect();
            self.units.retain(|p, _| keep.contains(p));
        }
        list.into_iter().filter_map(|p| self.units.get(&p).cloned().map(|u| (p, u))).collect()
    }
}

type Built = Option<(Unit, Vec<(PathBuf, (Stamp, u64))>)>;

fn compile_one(overlay: &Overlay, path: &Path) -> Built {
    let run = || {
        let loader = Recording { overlay, log: RefCell::new(Vec::new()) };
        let comp = compile_path(path, &loader, &Options { check_only: true });
        let log = loader.log.into_inner();
        let stamps = log.iter().filter_map(|(p, h, st)| Some((p.clone(), ((*st)?, (*h)?)))).collect();
        let deps = log.into_iter().map(|(p, h, _)| (p, h)).collect();
        (Unit::new(comp, deps), stamps)
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).ok()
}

/// Compiles each path as an entry, on as many threads as there are cores.
fn compile_all(overlay: &Overlay, paths: &[PathBuf]) -> Vec<(PathBuf, Built)> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(paths.len()).max(1);
    if threads == 1 {
        return paths.iter().map(|p| (p.clone(), compile_one(overlay, p))).collect();
    }
    let next = AtomicUsize::new(0);
    let mut out: Vec<(PathBuf, Built)> = Vec::with_capacity(paths.len());
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(64 << 20)
                    .spawn_scoped(s, || {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(p) = paths.get(i) else { break };
                            done.push((p.clone(), compile_one(overlay, p)));
                        }
                        done
                    })
                    .expect("spawn a compile thread")
            })
            .collect();
        for w in workers {
            if let Ok(done) = w.join() {
                out.extend(done);
            }
        }
    });
    out
}
