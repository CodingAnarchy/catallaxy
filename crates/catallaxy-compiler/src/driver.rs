//! Compilation driver: loads modules, runs the checker phases and generates a program.

use std::path::{Path, PathBuf};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::{self, Item};
use crate::check::Checker;
use crate::codegen::Codegen;
use crate::diag::{Diagnostic, SourceMap, Span};
use crate::parser;
use catallaxy_vm::Program;

pub const PRELUDE: &[(&str, &str)] = &[
    ("<prelude>/core.cx", include_str!("prelude/core.cx")),
    ("<prelude>/builtins.cx", include_str!("prelude/builtins.cx")),
];

/// Where module sources come from (the file system, or memory in tests and the LSP).
pub trait SourceLoader {
    fn load(&self, path: &Path) -> Option<String>;
}

pub struct FsLoader;

impl SourceLoader for FsLoader {
    fn load(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// In-memory overrides on top of the file system (open editor buffers).
pub struct OverlayLoader<'a> {
    pub files: &'a FxHashMap<PathBuf, String>,
}

impl SourceLoader for OverlayLoader<'_> {
    fn load(&self, path: &Path) -> Option<String> {
        self.files.get(path).cloned().or_else(|| std::fs::read_to_string(path).ok())
    }
}

pub struct Compilation {
    pub sm: SourceMap,
    pub ck: Checker,
    pub diags: Vec<Diagnostic>,
    pub program: Option<Program>,
    /// File id of each loaded user module, by module name.
    pub module_files: FxHashMap<String, u32>,
}

impl Compilation {
    pub fn has_errors(&self) -> bool {
        self.diags.iter().any(|d| d.is_error())
    }
}

struct Pending {
    name: String,
    file: u32,
    ast: ast::Module,
    deps: Vec<(String, Span)>,
    path: PathBuf,
}

/// Options for one compilation.
#[derive(Default, Clone)]
pub struct Options {
    /// Stop after type checking (no bytecode).
    pub check_only: bool,
}

/// Compiles an entry file and everything it imports.
pub fn compile_path(path: &Path, loader: &dyn SourceLoader, opts: &Options) -> Compilation {
    let src = loader.load(path);
    compile_entry(path, src, loader, opts, None)
}

/// As [`compile_path`], starting from a checked prelude instead of checking it again. See
/// [`Prelude`] for how the result compares.
pub fn compile_path_from(prelude: &Prelude, path: &Path, loader: &dyn SourceLoader, opts: &Options) -> Compilation {
    let src = loader.load(path);
    compile_entry(path, src, loader, opts, Some(prelude))
}

/// Compiles source text as the entry module (named after `path`).
pub fn compile_source(path: &Path, src: &str, opts: &Options) -> Compilation {
    compile_entry(path, Some(src.to_string()), &FsLoader, opts, None)
}

/// The prelude, parsed and checked on its own, for compilations to start from (see
/// [`compile_path_from`]) instead of each checking it again, which is most of the work of
/// checking a small program.
///
/// Checking a module never depends on the modules that import it: names, methods, extensions and
/// interfaces are looked up through the importing module's imports, and the prelude imports
/// nothing. A compilation that starts from a copy of this state therefore gets the prelude's
/// checked result it would have computed itself, and checks its own modules into that copy, so
/// everything that depends on the importer stays in the importer's compilation: implicit-generic
/// requirements checked at call sites, conversions to interfaces (`conform_log`), dynamic call
/// targets and diagnostics. Two pieces of global state are the exception:
/// - the loop pre-scan treats `x.name` as a call when any type has a method `name`, and builds
///   that set of names once from every module. The prelude was checked knowing only its own
///   names, so a program that declares a method named like a member the prelude's loops read
///   as a field checks the prelude itself instead (no such member exists today);
/// - `satisfies` caches whether a type conforms to an interface, but extensions visible to the
///   program can add methods, so the copy drops the negative entries.
///
/// The ids differ: the prelude's types, functions and classes come first, where a compilation
/// from scratch interleaves them with the program's. And from scratch, a prelude method's
/// mutating flag is final only after the program's bodies are checked, so a call of a mutating
/// prelude method on a frozen value is reported at the end, with a shorter note, where the
/// shared prelude reports it at the call. Otherwise the result is the same: the test
/// `shared_prelude.rs` compares the diagnostics, every expression's type, the symbol index and
/// the outlines over every conformance program and example. The language server's workspace
/// cache, which keeps no diagnostics, uses it; `catx` commands check the prelude themselves.
pub struct Prelude {
    sm: SourceMap,
    ck: Checker,
    diags: Vec<Diagnostic>,
    next_id: u32,
    /// Member names the prelude's loop pre-scans took for field reads because no type had such a
    /// method (see `Checker::method_name_misses`).
    misses: FxHashSet<String>,
}

impl Default for Prelude {
    fn default() -> Self {
        Prelude::new()
    }
}

impl Prelude {
    pub fn new() -> Prelude {
        let (sm, mut ck, mut diags, next_id) = load_prelude();
        ck.record_method_name_misses = true;
        run_phases(&mut ck, &FxHashMap::default(), 0);
        diags.append(&mut ck.diags);
        let misses = std::mem::take(&mut ck.method_name_misses);
        Prelude { sm, ck, diags, next_id, misses }
    }

    /// Whether `program` (the modules of a compilation) could change how the prelude checks, so
    /// that the compilation must check it again (see [`Prelude`]).
    fn affected_by(&self, program: &[&ast::Module]) -> bool {
        !self.misses.is_empty() && program.iter().any(|m| declares_method_named(m, &self.misses))
    }

    /// A copy of the checked prelude to check a program into.
    fn checker(&self) -> Checker {
        let mut ck = self.ck.clone();
        // Extensions visible to the program may make a type conform where the prelude's view
        // did not.
        ck.satisfies_cache.retain(|_, conforms| *conforms);
        // Rebuilt from every module on first use.
        ck.method_names.clear();
        ck
    }

    /// Member names the prelude's loop pre-scans read as fields: a program declaring a method of
    /// one of these names checks the prelude itself.
    pub fn loop_field_reads(&self) -> impl Iterator<Item = &String> {
        self.misses.iter()
    }

    /// The number of prelude modules (the program's modules come after them).
    pub fn modules(&self) -> u32 {
        self.ck.mods.len() as u32
    }
}

/// Whether `m` declares a method (of a class, interface, enum or extension) named in `names`.
fn declares_method_named(m: &ast::Module, names: &FxHashSet<String>) -> bool {
    m.items.iter().any(|it| {
        let methods = match it {
            Item::Class(c) => &c.methods,
            Item::Interface(i) => &i.methods,
            Item::Enum(e) => &e.methods,
            Item::Extend(x) => &x.methods,
            _ => return false,
        };
        methods.iter().any(|f| names.contains(&f.name.name))
    })
}

/// A checker with the prelude added (not yet checked), its source map, its parse diagnostics and
/// the next node id.
fn load_prelude() -> (SourceMap, Checker, Vec<Diagnostic>, u32) {
    let mut sm = SourceMap::default();
    let mut diags = Vec::new();
    let mut next_id = 0u32;
    let mut ck = Checker::new();
    let mut prelude_items = Vec::new();
    let mut prelude_file = 0;
    for (i, (name, text)) in PRELUDE.iter().enumerate() {
        let f = sm.add(name.to_string(), text.to_string());
        if i == 0 {
            prelude_file = f;
        }
        let (m, d) = parser::parse(text, f, &mut next_id);
        diags.extend(d);
        prelude_items.extend(m.items);
    }
    ck.add_module("prelude", prelude_file, ast::Module { items: prelude_items, file: prelude_file }, true);
    (sm, ck, diags, next_id)
}

fn compile_entry(
    path: &Path,
    src: Option<String>,
    loader: &dyn SourceLoader,
    opts: &Options,
    shared: Option<&Prelude>,
) -> Compilation {
    // The prelude's source files and node ids are the same whether it is checked here or shared.
    let (mut sm, mut ck, mut diags, mut next_id) = match shared {
        Some(p) => (p.sm.clone(), p.checker(), p.diags.clone(), p.next_id),
        None => load_prelude(),
    };

    let Some(src) = src else {
        diags.push(Diagnostic::error("E0001", format!("cannot read `{}`", path.display()), Span::new(u32::MAX, 0, 0)));
        return Compilation { sm, ck, diags, program: None, module_files: FxHashMap::default() };
    };
    let root = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));

    // Load the module graph breadth-first.
    let mut pending: Vec<Pending> = Vec::new();
    let mut by_name: FxHashMap<String, usize> = FxHashMap::default();
    let entry_name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "main".into());
    let f = sm.add(path.display().to_string(), src.clone());
    let (m, d) = parser::parse(&src, f, &mut next_id);
    diags.extend(d);
    let deps = imports_of(&m);
    pending.push(Pending { name: format!("<entry>{entry_name}"), file: f, ast: m, deps, path: path.to_path_buf() });
    let mut i = 0;
    while i < pending.len() {
        let deps = pending[i].deps.clone();
        let base_dir = pending[i].path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone());
        for (dep, span) in deps {
            if by_name.contains_key(&dep) {
                continue;
            }
            let rel: PathBuf = dep.split('.').collect::<PathBuf>().with_extension("cx");
            let candidates = [base_dir.join(&rel), root.join(&rel)];
            let found = candidates.iter().find_map(|p| loader.load(p).map(|s| (p.clone(), s)));
            match found {
                Some((p, text)) => {
                    let f = sm.add(p.display().to_string(), text.clone());
                    let (m, d) = parser::parse(&text, f, &mut next_id);
                    diags.extend(d);
                    let deps = imports_of(&m);
                    by_name.insert(dep.clone(), pending.len());
                    pending.push(Pending { name: dep.clone(), file: f, ast: m, deps, path: p });
                }
                None => {
                    let msg = format!("cannot find module `{dep}` (looked for `{}`)", rel.display());
                    diags.push(Diagnostic::error("E0204", msg, span));
                    by_name.insert(dep.clone(), usize::MAX);
                }
            }
        }
        i += 1;
    }

    // Topological order: dependencies first; report cycles.
    let mut order: Vec<usize> = Vec::new();
    let mut state = vec![0u8; pending.len()];
    fn visit(
        i: usize,
        pending: &[Pending],
        by_name: &FxHashMap<String, usize>,
        state: &mut [u8],
        order: &mut Vec<usize>,
        diags: &mut Vec<Diagnostic>,
    ) {
        if state[i] == 2 {
            return;
        }
        if state[i] == 1 {
            return;
        }
        state[i] = 1;
        for (dep, span) in &pending[i].deps {
            if let Some(&j) = by_name.get(dep) {
                if j == usize::MAX {
                    continue;
                }
                if state[j] == 1 {
                    diags.push(Diagnostic::error("E0205", format!("cyclic import of module `{dep}`"), *span));
                    continue;
                }
                visit(j, pending, by_name, state, order, diags);
            }
        }
        state[i] = 2;
        order.push(i);
    }
    for i in 0..pending.len() {
        visit(i, &pending, &by_name, &mut state, &mut order, &mut diags);
    }

    // The modules before `first` are checked already.
    let mut first = 0;
    if let Some(p) = shared {
        let asts: Vec<&ast::Module> = pending.iter().map(|p| &p.ast).collect();
        if p.affected_by(&asts) {
            ck = load_prelude().1;
        } else {
            first = p.modules();
        }
    }

    let mut module_ids: FxHashMap<String, u32> = FxHashMap::default();
    let mut module_files = FxHashMap::default();
    let mut pend: Vec<Option<Pending>> = pending.into_iter().map(Some).collect();
    for i in order {
        let p = pend[i].take().unwrap();
        let id = ck.add_module(&p.name, p.file, p.ast, false);
        module_files.insert(p.name.clone(), p.file);
        module_ids.insert(p.name, id);
    }

    run_phases(&mut ck, &module_ids, first);
    diags.extend(crate::lint::run(&ck, &sm));
    diags.append(&mut ck.diags);
    let mut program = None;
    if !opts.check_only && !diags.iter().any(|d| d.is_error()) {
        let (p, errs) = Codegen::new(&ck, &sm).generate();
        diags.extend(errs);
        if !diags.iter().any(|d| d.is_error()) {
            program = Some(p);
        }
    }
    crate::lint::apply_allows(&mut diags, &sm);
    dedup(&mut diags);
    Compilation { sm, ck, diags, program, module_files }
}

/// Runs the checker's phases on the modules from `first` on; the ones before are already checked
/// (a [`Prelude`]). The phases over every module skip what is done.
pub fn run_phases(ck: &mut Checker, module_ids: &FxHashMap<String, u32>, first: u32) {
    let n = ck.mods.len() as u32;
    // Run-time interface conformance depends on every module's classes and extensions.
    ck.rt_conf_cache.clear();
    for m in first..n {
        ck.declare_items(m);
    }
    for m in first..n {
        ck.resolve_imports(m, module_ids);
    }
    for m in first..n {
        ck.resolve_type_params(m);
    }
    ck.resolve_bounds();
    for m in first..n {
        ck.resolve_signatures(m);
    }
    ck.finish_classes();
    ck.check_all_bodies();
    ck.check_overrides();
    ck.finish_mutating();
    ck.finish_rt_ifaces();
}

fn dedup(diags: &mut Vec<Diagnostic>) {
    let mut seen = rustc_hash::FxHashSet::default();
    diags.retain(|d| seen.insert((d.code, d.span, d.message.clone())));
    diags.sort_by_key(|d| (d.span.file, d.span.start));
}

fn imports_of(m: &ast::Module) -> Vec<(String, Span)> {
    m.items
        .iter()
        .filter_map(|it| match it {
            Item::Import(imp) => {
                Some((imp.path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("."), imp.span))
            }
            _ => None,
        })
        .collect()
}
