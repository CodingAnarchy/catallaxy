//! Compiling from a shared, checked prelude (`driver::Prelude`) gives the same result as checking
//! the prelude in each compilation: the same symbol index, interface conformance links and
//! outlines (what the language server's workspace cache keeps), the same type for every
//! expression, and the same diagnostics. Checked over every program in `tests/conformance` and
//! `examples`, and over programs aimed at the cases `Prelude` handles.
//!
//! Diagnostics are compared without their notes. A call of a mutating prelude method on a frozen
//! value is reported with one more note (`use .dup()`) from the shared prelude, whose mutating
//! flags are final, than from scratch, where they are inferred after the program's bodies are
//! checked and the call is reported then. This is why only the workspace cache, which keeps no
//! diagnostics, compiles from a shared prelude.

use std::path::{Path, PathBuf};

use catallaxy_compiler::driver::{compile_path, compile_path_from, Compilation, FsLoader, Options, Prelude};
use catallaxy_compiler::symbols::{decl_span, outline, symbol_name, Index, OutlineItem};

/// Everything compared between the two ways of compiling, as text.
fn summary(c: &Compilation) -> Vec<String> {
    let mut out = Vec::new();
    for d in &c.diags {
        let fixes: Vec<&str> = d.fixes.iter().map(|f| f.message.as_str()).collect();
        out.push(format!("diag {} {:?} {:?}: {} | {}", d.code, d.severity, d.span, d.message, fixes.join("; ")));
    }
    // Node ids are the same either way: the prelude is parsed first in both.
    let mut tys: Vec<_> = c.ck.expr_ty.iter().map(|(&id, &t)| (id, c.ck.show(t))).collect();
    tys.sort();
    out.extend(tys.into_iter().map(|(id, t)| format!("type {id}: {t}")));
    let index = Index::build(&c.ck, &c.sm);
    for o in &index.occs {
        let name = symbol_name(&c.ck, &c.sm, o.sym).unwrap_or_default();
        out.push(format!("occ {:?} {} {name} decl {:?}", o.span, o.is_decl, decl_span(&c.ck, o.sym)));
    }
    let mut links: Vec<String> = index
        .links
        .iter()
        .flat_map(|(a, bs)| bs.iter().map(move |b| (a, b)))
        .map(|(&a, &b)| format!("link {:?} {:?}", decl_span(&c.ck, a), decl_span(&c.ck, b)))
        .collect();
    links.sort();
    out.extend(links);
    for (m, md) in c.ck.mods.iter().enumerate() {
        if !md.is_prelude {
            let mut stack: Vec<OutlineItem> = outline(&c.ck, &c.sm, m as u32);
            while let Some(item) = stack.pop() {
                out.push(format!("outline {} {:?} {:?}", item.name, item.kind, item.name_span));
                stack.extend(item.children);
            }
        }
    }
    out
}

fn check_same(prelude: &Prelude, path: &Path) {
    let opts = Options { check_only: true };
    let fresh = compile_path(path, &FsLoader, &opts);
    let shared = compile_path_from(prelude, path, &FsLoader, &opts);
    let (a, b) = (summary(&fresh), summary(&shared));
    if a != b {
        let first = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        panic!(
            "{}: compiling from the shared prelude differs at line {first}:\n  fresh:  {:?}\n  shared: {:?}",
            path.display(),
            a.get(first),
            b.get(first)
        );
    }
}

fn cx_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            cx_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "cx") {
            out.push(p);
        }
    }
}

#[test]
fn shared_prelude_matches_fresh_compilations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    cx_files(&root.join("tests/conformance"), &mut files);
    cx_files(&root.join("examples"), &mut files);
    assert!(files.len() > 200, "{}", files.len());
    let prelude = Prelude::new();
    // Deep checker recursion: as `catx`, on a large stack.
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            for f in &files {
                check_same(&prelude, f);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn check_source_same(name: &str, src: &str) {
    let dir = std::env::temp_dir().join(format!("catx-shared-prelude-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    check_same(&Prelude::new(), &path);
}

/// A program's extension can make a builtin type conform to an interface: the prelude's
/// negative conformance results must not carry over.
#[test]
fn shared_prelude_extension_conformance() {
    check_source_same(
        "ext_conform.cx",
        "interface Area {\n  fn area(self) -> Float\n}\nextend Int {\n  fn area(self) -> Float { 1.0 }\n}\n\
         fn total(xs: List[Area]) -> Float {\n  s = 0.0\n  for x in xs { s = s + x.area() }\n  s\n}\n\
         puts(total([1, 2]))\nputs(\"#{[3, 1, 2].sort()}\")\n",
    );
}

/// Errors in the program and implicit generics checked at call sites of prelude functions.
#[test]
fn shared_prelude_errors_and_call_sites() {
    check_source_same(
        "errs.cx",
        "fn add(x, y) { x + y }\nputs(add(1, \"a\"))\nputs([1, 2].map { |x| x + \"s\" })\nn: Int = \"s\"\n\
         class C {\n  n: Int = 0\n  fn len(self) -> Int { self.n }\n}\nputs([C.new()].max())\n",
    );
}

/// A program that declares a method named like a member the prelude's loops read as a field
/// checks the prelude itself, with the same result. (No such member exists today; this covers
/// any the prelude gains.)
#[test]
fn shared_prelude_loop_field_reads() {
    let prelude = Prelude::new();
    for name in prelude.loop_field_reads() {
        check_source_same(
            &format!("loop_read_{name}.cx"),
            &format!("class C {{\n  n: Int = 0\n  fn {name}(self) -> Int {{ self.n }}\n}}\nputs(C.new().{name}())\n"),
        );
    }
}

/// The language server shares one prelude between its compile threads.
#[test]
fn shared_prelude_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Prelude>();
}
