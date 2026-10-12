//! Interned names (methods, fields). Symbols are process-wide and never freed.

use std::sync::LazyLock;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Sym(pub u32);

struct Interner {
    map: FxHashMap<&'static str, Sym>,
    names: Vec<&'static str>,
}

static INTERNER: LazyLock<RwLock<Interner>> =
    LazyLock::new(|| RwLock::new(Interner { map: FxHashMap::default(), names: Vec::new() }));

pub fn intern(s: &str) -> Sym {
    if let Some(&sym) = INTERNER.read().map.get(s) {
        return sym;
    }
    let mut w = INTERNER.write();
    if let Some(&sym) = w.map.get(s) {
        return sym;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    let sym = Sym(w.names.len() as u32);
    w.names.push(leaked);
    w.map.insert(leaked, sym);
    sym
}

pub fn name(sym: Sym) -> &'static str {
    INTERNER.read().names[sym.0 as usize]
}

impl std::fmt::Display for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(name(*self))
    }
}
