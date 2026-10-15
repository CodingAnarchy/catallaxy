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
//!
//! Every compilation starts from the same checked prelude (`driver::Prelude`) instead of checking
//! it again, which was most of the work of compiling a typical file.
//!
//! After `initialized`, and when workspace folders change, the cache is warmed in the background
//! on half the cores (see [`Workspace::warm`]), so the first request seldom compiles anything.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use catallaxy_compiler::diag::{SourceFile, Span};
use catallaxy_compiler::driver::{compile_path_from, Compilation, Options, Prelude, SourceLoader, PRELUDE};
use catallaxy_compiler::query;
use catallaxy_compiler::symbols::{self, Index, OutlineKind, Symbol};
use lsp_server::{Message, Notification, Request};
use lsp_types as lt;
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
#[derive(Default)]
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

    /// The documents' paths.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.texts.keys()
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
    log: Mutex<Vec<Load>>,
}

impl SourceLoader for Recording<'_> {
    fn load(&self, path: &Path) -> Option<String> {
        if let Some((t, h)) = self.overlay.get(path) {
            self.log.lock().unwrap().push((path.to_path_buf(), Some(*h), None));
            return Some(t.clone());
        }
        let st = stamp(path);
        let text = st.and_then(|_| std::fs::read_to_string(path).ok());
        self.log.lock().unwrap().push((path.to_path_buf(), text.as_deref().map(hash_text), st));
        text
    }
}

/// A cached unit, or a compilation of it in progress (a request or the warm-up builds it, and
/// whoever else needs it waits for that file).
enum Slot {
    Ready(Arc<Unit>),
    Building,
}

/// The workspace folders, the files found under them, and the cached units.
#[derive(Default)]
struct State {
    roots: Vec<PathBuf>,
    /// Whether the client reports created and deleted files, so the file list can be kept.
    watching: bool,
    /// The `.cx` files under the roots, and whether the list was capped (`None`: walk again).
    walked: Option<(Vec<PathBuf>, bool)>,
    /// Bumped whenever the file list is forgotten, so a walk that started before is not kept.
    walk_gen: u64,
    /// The cap last reported in the log, so it is reported once.
    reported_cap: Option<usize>,
    units: FxHashMap<PathBuf, Slot>,
    /// Disk files' stamps and content hashes.
    hashes: FxHashMap<PathBuf, (Stamp, u64)>,
}

/// What a request of the workspace produced besides units: log lines for the client.
#[derive(Default)]
pub struct Notes {
    pub warnings: Vec<String>,
    /// Debug lines (`CATX_LSP_DEBUG=1`), logged at the lowest level.
    pub debug: Vec<String>,
}

fn max_files() -> usize {
    std::env::var("CATX_LSP_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_FILES)
}

fn cap_warning(cap: usize) -> String {
    format!(
        "catx lsp: the workspace has more than {cap} .cx files; references, rename and workspace symbols \
         cover the first {cap} (set CATX_LSP_MAX_FILES to change the limit)"
    )
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

/// `list` without duplicates, however a file is reached.
fn dedup(list: &mut Vec<PathBuf>) {
    let mut seen = FxHashSet::default();
    list.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
}

impl State {
    /// Records a walk of the roots; returns the cap warning to log if it is new.
    fn store_walk(&mut self, files: Vec<PathBuf>, capped: bool, cap: usize) -> Option<String> {
        let warn = (capped && self.reported_cap != Some(cap)).then(|| cap_warning(cap));
        self.reported_cap = capped.then_some(cap);
        self.walked = Some((files, capped));
        warn
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
        notes.warnings.extend(self.store_walk(files.clone(), capped, cap));
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

    /// Whether every file `u`'s compilation loaded still has the content it had then.
    fn valid(&mut self, overlay: &Overlay, memo: &mut FxHashMap<PathBuf, Option<u64>>, u: &Unit) -> bool {
        u.deps.iter().all(|(d, h)| self.current(overlay, memo, d) == *h)
    }

    /// Stores a finished compilation of `p` (`None`: it failed).
    fn finish(&mut self, p: PathBuf, built: Built) -> bool {
        match built {
            Some((u, stamps)) => {
                self.hashes.extend(stamps);
                self.units.insert(p, Slot::Ready(Arc::new(u)));
                true
            }
            None => {
                self.units.remove(&p);
                false
            }
        }
    }
}

/// Sends a message to the client (from any thread).
pub type Sender = Arc<dyn Fn(Message) + Send + Sync>;

/// The workspace: its folders and files, the cached units, the open documents' text, and the
/// background warm-up of the cache.
///
/// The cache is shared between the message loop and the warm-up threads. A unit being compiled
/// is marked [`Slot::Building`]: a request that needs it waits for that file only, and the
/// warm-up skips it, so no file is compiled twice at once. Units are checked against the current
/// content of the files they loaded whenever they are used, so a unit the warm-up built from text
/// that has changed since is recompiled, not used.
#[derive(Default)]
pub struct Workspace {
    state: Mutex<State>,
    /// Signalled whenever a compilation finishes.
    built: Condvar,
    /// The open documents' text, replaced on every change.
    overlay: Mutex<Arc<Overlay>>,
    /// Requests and diagnostics running; the warm-up waits between files while there are any.
    busy: AtomicUsize,
    /// The current warm-up; an older one stops at its next file.
    generation: AtomicU64,
    stopped: AtomicBool,
    warmers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Answers to `window/workDoneProgress/create`, by request id: whether the client accepted.
    progress: Mutex<FxHashMap<String, bool>>,
    progress_answered: Condvar,
    /// `CATX_LSP_DEBUG=1`: log how many files each request compiled.
    debug: bool,
}

/// While alive, the warm-up pauses between files.
pub struct Busy<'a>(&'a AtomicUsize);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Request id prefix of `window/workDoneProgress/create`.
pub const PROGRESS_REQUEST: &str = "catx/progress-";

impl Workspace {
    pub fn new(roots: Vec<PathBuf>, watching: bool) -> Workspace {
        let ws = Workspace { debug: std::env::var("CATX_LSP_DEBUG").is_ok_and(|v| v == "1"), ..Default::default() };
        {
            let mut st = ws.lock();
            st.roots = roots;
            st.watching = watching;
        }
        ws
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn overlay(&self) -> Arc<Overlay> {
        self.overlay.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_overlay(&self, o: Overlay) {
        *self.overlay.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(o);
    }

    /// Marks a request or a diagnostics run in progress until the guard is dropped.
    pub fn busy(&self) -> Busy<'_> {
        self.busy.fetch_add(1, Ordering::SeqCst);
        Busy(&self.busy)
    }

    /// Changes the workspace folders.
    pub fn set_roots(&self, f: impl FnOnce(&mut Vec<PathBuf>)) {
        let mut st = self.lock();
        f(&mut st.roots);
        st.walked = None;
        st.walk_gen += 1;
    }

    /// The client will not report created and deleted files after all.
    pub fn stop_watching(&self) {
        self.lock().watching = false;
        self.rescan();
    }

    /// Forget the file list (files were created or deleted).
    pub fn rescan(&self) {
        let mut st = self.lock();
        st.walked = None;
        st.walk_gen += 1;
    }

    /// Records the client's answer to a `window/workDoneProgress/create` request.
    pub fn progress_answer(&self, id: String, accepted: bool) {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).insert(id, accepted);
        self.progress_answered.notify_all();
    }

    /// Stops the warm-up and waits for its threads (each finishes the file it is compiling).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let handles = std::mem::take(&mut *self.warmers.lock().unwrap_or_else(|e| e.into_inner()));
        for h in handles {
            let _ = h.join();
        }
    }

    /// Units for `entries` (in that order, duplicates dropped), compiling the ones that are
    /// missing or out of date and waiting for those being compiled. With `all`, the workspace
    /// files follow the given entries and cached units of other entries are dropped.
    pub fn units(&self, entries: Vec<PathBuf>, all: bool, notes: &mut Notes) -> Vec<(PathBuf, Arc<Unit>)> {
        let _busy = self.busy();
        let overlay = self.overlay();
        let mut st = self.lock();
        let mut list = entries;
        if all {
            list.extend(st.files(notes));
        }
        dedup(&mut list);
        let mut failed = FxHashSet::default();
        let mut compiled = 0;
        loop {
            let mut memo = FxHashMap::default();
            let mut stale = Vec::new();
            let mut waiting = false;
            for p in &list {
                if failed.contains(p) {
                    continue;
                }
                let ok = match st.units.get(p) {
                    Some(Slot::Building) => {
                        waiting = true;
                        continue;
                    }
                    Some(Slot::Ready(u)) => {
                        let u = u.clone();
                        st.valid(&overlay, &mut memo, &u)
                    }
                    None => false,
                };
                if !ok {
                    stale.push(p.clone());
                }
            }
            if !stale.is_empty() {
                for p in &stale {
                    st.units.insert(p.clone(), Slot::Building);
                }
                drop(st);
                let built = compile_all(&overlay, &stale, cores());
                compiled += stale.len();
                st = self.lock();
                let mut done = FxHashSet::default();
                for (p, b) in built {
                    done.insert(p.clone());
                    if !st.finish(p.clone(), b) {
                        notes.warnings.push(format!("catx lsp: compiling {} failed", p.display()));
                        failed.insert(p);
                    }
                }
                // A worker that died without an answer leaves no file marked as building.
                for p in stale {
                    if !done.contains(&p) {
                        st.units.remove(&p);
                        failed.insert(p);
                    }
                }
                self.built.notify_all();
                continue;
            }
            if !waiting {
                break;
            }
            st = self.built.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        if all {
            let keep: FxHashSet<&PathBuf> = list.iter().collect();
            st.units.retain(|p, s| keep.contains(p) || matches!(s, Slot::Building));
        }
        if self.debug && compiled > 0 {
            notes.debug.push(format!("catx lsp: a request compiled {compiled} files"));
        }
        list.into_iter()
            .filter_map(|p| match st.units.get(&p) {
                Some(Slot::Ready(u)) => Some((p, u.clone())),
                _ => None,
            })
            .collect()
    }

    /// Starts compiling every workspace file and open document in the background, replacing any
    /// warm-up in progress. Requests don't wait for it: one that needs a file the warm-up is
    /// compiling waits for that file, and compiles the files not yet reached itself.
    pub fn warm(self: &Arc<Self>, send: Sender, progress: bool) {
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let ws = self.clone();
        let spawned =
            // Indexing walks syntax trees as deep as the parser allows (see `parser::MAX_DEPTH`).
            std::thread::Builder::new()
                .name("catx-warm".into())
                .stack_size(64 << 20)
                .spawn(move || ws.run_warm(gen, &send, progress));
        let mut warmers = self.warmers.lock().unwrap_or_else(|e| e.into_inner());
        warmers.retain(|h| !h.is_finished());
        warmers.extend(spawned.ok());
    }

    fn cancelled(&self, gen: u64) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.generation.load(Ordering::SeqCst) != gen
    }

    /// Sleeps `ms` milliseconds, or less if the warm-up is cancelled.
    fn nap(&self, gen: u64, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while !self.cancelled(gen) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(2).min(end.saturating_duration_since(Instant::now())));
        }
    }

    fn run_warm(&self, gen: u64, send: &Sender, progress: bool) {
        let start = Instant::now();
        let (roots, kept, walk_gen) = {
            let st = self.lock();
            let kept = st.walked.clone().filter(|_| st.watching);
            (st.roots.clone(), kept, st.walk_gen)
        };
        if roots.is_empty() {
            return;
        }
        let cap = max_files();
        let (files, capped) = kept.unwrap_or_else(|| walk(&roots, cap));
        {
            let mut st = self.lock();
            if st.walk_gen == walk_gen && st.roots == roots {
                // Logged under the lock, so a request that finds the list stored answers after it.
                if let Some(w) = st.store_walk(files.clone(), capped, cap) {
                    send(log_message(lt::MessageType::WARNING, w));
                }
            }
        }
        let mut list: Vec<PathBuf> = self.overlay().paths().cloned().collect();
        list.sort();
        list.extend(files);
        dedup(&mut list);
        if list.is_empty() || self.cancelled(gen) {
            return;
        }
        let progress = progress.then(|| Progress::start(self, gen, send)).flatten();
        // For tests: hold each file as being compiled this long first, so requests and edits
        // meet the warm-up in progress.
        let delay = std::env::var("CATX_LSP_WARM_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let compiled = AtomicUsize::new(0);
        let threads = warm_threads().min(list.len());
        std::thread::scope(|s| {
            for _ in 0..threads {
                let work = || loop {
                    if self.cancelled(gen) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(p) = list.get(i) else { break };
                    // Requests and diagnostics go first.
                    while self.busy.load(Ordering::SeqCst) > 0 && !self.cancelled(gen) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    if self.cancelled(gen) {
                        break;
                    }
                    if self.warm_one(p, gen, delay, send) {
                        compiled.fetch_add(1, Ordering::SeqCst);
                    }
                    let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                    if let Some(pr) = &progress {
                        pr.report(send, d, list.len());
                    }
                };
                // Compiling needs deep stacks, as in `compile_all`.
                let _ = std::thread::Builder::new().stack_size(64 << 20).spawn_scoped(s, work);
            }
        });
        let cancelled = self.cancelled(gen);
        if let Some(pr) = progress {
            pr.end(send, cancelled);
        }
        if !cancelled {
            let msg = format!(
                "catx lsp: indexed the workspace ({} files, {} compiled) in {} ms",
                list.len(),
                compiled.load(Ordering::SeqCst),
                start.elapsed().as_millis()
            );
            send(log_message(lt::MessageType::LOG, msg));
        }
    }

    /// Compiles `p` unless it is up to date or being compiled; whether it compiled it.
    fn warm_one(&self, p: &Path, gen: u64, delay: u64, send: &Sender) -> bool {
        let overlay = self.overlay();
        {
            let mut st = self.lock();
            match st.units.get(p) {
                Some(Slot::Building) => return false,
                Some(Slot::Ready(u)) => {
                    let u = u.clone();
                    if st.valid(&overlay, &mut FxHashMap::default(), &u) {
                        return false;
                    }
                }
                None => {}
            }
            st.units.insert(p.to_path_buf(), Slot::Building);
        }
        self.nap(gen, delay);
        if self.cancelled(gen) {
            self.lock().units.remove(p);
            self.built.notify_all();
            return false;
        }
        let built = compile_one(&overlay, p);
        if !self.lock().finish(p.to_path_buf(), built) {
            send(log_message(lt::MessageType::WARNING, format!("catx lsp: compiling {} failed", p.display())));
        }
        self.built.notify_all();
        true
    }
}

pub fn log_message(typ: lt::MessageType, message: String) -> Message {
    Message::Notification(Notification::new("window/logMessage".into(), lt::LogMessageParams { typ, message }))
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// The warm-up's threads: half the cores, so requests and typing keep the rest.
fn warm_threads() -> usize {
    (cores() / 2).max(1)
}

/// `window/workDoneProgress` reports of a warm-up.
struct Progress {
    token: String,
    /// The last percentage reported.
    last: Mutex<u32>,
}

impl Progress {
    /// Asks the client for a progress token and waits briefly for its answer; `None` if it
    /// refused or did not answer in time.
    fn start(ws: &Workspace, gen: u64, send: &Sender) -> Option<Progress> {
        let id = format!("{PROGRESS_REQUEST}{gen}");
        let token = format!("catx-warm-{gen}");
        let params = lt::WorkDoneProgressCreateParams { token: lt::NumberOrString::String(token.clone()) };
        send(Message::Request(Request::new(id.clone().into(), "window/workDoneProgress/create".into(), params)));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut answers = ws.progress.lock().unwrap_or_else(|e| e.into_inner());
        let accepted = loop {
            if let Some(a) = answers.remove(&id) {
                break a;
            }
            let now = Instant::now();
            if now >= deadline || ws.cancelled(gen) {
                return None;
            }
            answers = ws.progress_answered.wait_timeout(answers, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        };
        drop(answers);
        if !accepted {
            return None;
        }
        let pr = Progress { token, last: Mutex::new(0) };
        pr.send(
            send,
            lt::WorkDoneProgress::Begin(lt::WorkDoneProgressBegin {
                title: "Indexing the workspace".into(),
                cancellable: Some(false),
                message: None,
                percentage: Some(0),
            }),
        );
        Some(pr)
    }

    fn send(&self, send: &Sender, value: lt::WorkDoneProgress) {
        let params = lt::ProgressParams {
            token: lt::NumberOrString::String(self.token.clone()),
            value: lt::ProgressParamsValue::WorkDone(value),
        };
        send(Message::Notification(Notification::new("$/progress".into(), params)));
    }

    fn report(&self, send: &Sender, done: usize, total: usize) {
        let pct = (done * 100 / total.max(1)) as u32;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if pct < *last + 5 {
            return;
        }
        *last = pct;
        self.send(
            send,
            lt::WorkDoneProgress::Report(lt::WorkDoneProgressReport {
                cancellable: None,
                message: Some(format!("{done}/{total} files")),
                percentage: Some(pct),
            }),
        );
    }

    fn end(self, send: &Sender, cancelled: bool) {
        let message = Some(if cancelled { "stopped" } else { "done" }.to_string());
        self.send(send, lt::WorkDoneProgress::End(lt::WorkDoneProgressEnd { message }));
    }
}

type Built = Option<(Unit, Vec<(PathBuf, (Stamp, u64))>)>;

/// The prelude, checked once for every compilation of the workspace (see `driver::Prelude`).
fn prelude() -> &'static Prelude {
    static PRELUDE_CHECKED: std::sync::OnceLock<Prelude> = std::sync::OnceLock::new();
    PRELUDE_CHECKED.get_or_init(Prelude::new)
}

fn compile_one(overlay: &Overlay, path: &Path) -> Built {
    let run = || {
        let loader = Recording { overlay, log: Mutex::new(Vec::new()) };
        let comp = compile_path_from(prelude(), path, &loader, &Options { check_only: true });
        let log = loader.log.into_inner().unwrap();
        let stamps = log.iter().filter_map(|(p, h, st)| Some((p.clone(), ((*st)?, (*h)?)))).collect();
        let deps = log.into_iter().map(|(p, h, _)| (p, h)).collect();
        (Unit::new(comp, deps), stamps)
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).ok()
}

/// Compiles each path as an entry, on up to `threads` threads.
fn compile_all(overlay: &Overlay, paths: &[PathBuf], threads: usize) -> Vec<(PathBuf, Built)> {
    let threads = threads.min(paths.len()).max(1);
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
