//! Compilation driver: loads modules, runs the checker phases and generates a program.

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

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
    compile_entry(path, src, loader, opts)
}

/// Compiles source text as the entry module (named after `path`).
pub fn compile_source(path: &Path, src: &str, opts: &Options) -> Compilation {
    compile_entry(path, Some(src.to_string()), &FsLoader, opts)
}

fn compile_entry(path: &Path, src: Option<String>, loader: &dyn SourceLoader, opts: &Options) -> Compilation {
    let mut sm = SourceMap::default();
    let mut diags = Vec::new();
    let mut next_id = 0u32;
    let mut ck = Checker::new();

    // Prelude.
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
    let prelude = ck.add_module("prelude", prelude_file, ast::Module { items: prelude_items, file: prelude_file }, true);
    let _ = prelude;

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

    let mut module_ids: FxHashMap<String, u32> = FxHashMap::default();
    let mut module_files = FxHashMap::default();
    let mut pend: Vec<Option<Pending>> = pending.into_iter().map(Some).collect();
    for i in order {
        let p = pend[i].take().unwrap();
        let id = ck.add_module(&p.name, p.file, p.ast, false);
        module_files.insert(p.name.clone(), p.file);
        module_ids.insert(p.name, id);
    }

    run_phases(&mut ck, &module_ids);
    diags.append(&mut ck.diags);
    let mut program = None;
    if !opts.check_only && !diags.iter().any(|d| d.is_error()) {
        let (p, errs) = Codegen::new(&ck, &sm).generate();
        diags.extend(errs);
        if !diags.iter().any(|d| d.is_error()) {
            program = Some(p);
        }
    }
    dedup(&mut diags);
    Compilation { sm, ck, diags, program, module_files }
}

pub fn run_phases(ck: &mut Checker, module_ids: &FxHashMap<String, u32>) {
    let n = ck.mods.len() as u32;
    for m in 0..n {
        ck.declare_items(m);
    }
    for m in 0..n {
        ck.resolve_imports(m, module_ids);
    }
    for m in 0..n {
        ck.resolve_type_params(m);
    }
    ck.resolve_bounds();
    for m in 0..n {
        ck.resolve_signatures(m);
    }
    ck.finish_classes();
    ck.check_all_bodies();
    ck.finish_mutating();
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
