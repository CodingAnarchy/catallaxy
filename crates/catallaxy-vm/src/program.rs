//! Process-wide registries of loaded functions, natives and globals, and the program loader.
//!
//! Everything loaded here is frozen: function code and constants never change after loading, so
//! all threads read them without synchronization. Registries are append-only fixed-capacity arrays
//! of pointers, so lookups on the hot path are a single indexed load.

use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::LazyLock;

use catallaxy_gc::{RootSource, Tracer};
use catallaxy_runtime::class::Capability;
use catallaxy_runtime::class::{self, register_class, ClassSpec, ObjKind};
use catallaxy_runtime::object::mark_value;
use catallaxy_runtime::{intern, Mutator, RtResult, Sym, Value};
use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::bytecode::*;
use crate::interp::Vm;

/// A native function: receives the calling thread's VM and its arguments.
pub type NativeFn = fn(&mut Vm, &[Value]) -> RtResult<Value>;

/// Per-call-site inline cache: `(class id + 1) << 32 | function id` in one atomic word, so a reader
/// never sees a class from one write and a method from another.
pub struct Ic {
    pub name: Sym,
    pub cache: AtomicU64,
    pub fallback: u32,
}

pub const NO_FUNC: u32 = u32::MAX;

pub struct Func {
    pub id: u32,
    pub name: String,
    pub file: u32,
    pub nparams: usize,
    pub nregs: usize,
    pub code: Box<[u32]>,
    pub lines: Box<[(u32, u32)]>,
    pub consts: Box<[Value]>,
    pub ics: Box<[Ic]>,
    pub captures: Box<[CapSrc]>,
    pub native: Option<NativeFn>,
}

impl Func {
    pub fn line_at(&self, pc: usize) -> u32 {
        match self.lines.binary_search_by_key(&(pc as u32), |&(p, _)| p) {
            Ok(i) => self.lines[i].1,
            Err(0) => self.lines.first().map(|l| l.1).unwrap_or(0),
            Err(i) => self.lines[i - 1].1,
        }
    }
}

struct Table<T> {
    slots: Box<[AtomicPtr<T>]>,
    len: AtomicUsize,
}

impl<T> Table<T> {
    fn new(cap: usize) -> Self {
        Table { slots: (0..cap).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(), len: AtomicUsize::new(0) }
    }
    fn push_at(&self, idx: usize, v: T) -> &'static T {
        assert!(idx < self.slots.len(), "registry capacity exceeded");
        let p = Box::leak(Box::new(v));
        self.slots[idx].store(p, Ordering::Release);
        if idx >= self.len.load(Ordering::Relaxed) {
            self.len.store(idx + 1, Ordering::Release);
        }
        p
    }
    #[inline(always)]
    fn get(&self, idx: usize) -> &'static T {
        let p = self.slots[idx].load(Ordering::Acquire);
        debug_assert!(!p.is_null(), "registry slot {idx} empty");
        unsafe { &*p }
    }
    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }
}

static FUNCS: LazyLock<Table<Func>> = LazyLock::new(|| Table::new(1 << 18));
static NATIVES: LazyLock<Mutex<Vec<NativeFn>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static NATIVE_IDS: LazyLock<Table<NativeFn>> = LazyLock::new(|| Table::new(1 << 14));
pub const MAX_GLOBALS: usize = 1 << 16;
static GLOBALS: LazyLock<Box<[AtomicU64]>> =
    LazyLock::new(|| (0..MAX_GLOBALS).map(|_| AtomicU64::new(Value::NIL.0)).collect());
static GLOBAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static RT_TYPES: LazyLock<Table<RtType>> = LazyLock::new(|| Table::new(1 << 16));
static RT_PARTS: LazyLock<Table<Vec<u32>>> = LazyLock::new(|| Table::new(1 << 16));
static WRITE_CHECKS: LazyLock<Table<WriteCheck>> = LazyLock::new(|| Table::new(1 << 14));
static IFACES: LazyLock<Table<Vec<Sym>>> = LazyLock::new(|| Table::new(1 << 14));

#[inline]
pub fn rt_type(id: u32) -> &'static RtType {
    RT_TYPES.get(id as usize)
}

/// The ids of the direct parts of run-time type `id` (see `Program::rt_parts`).
#[inline]
pub fn rt_parts(id: u32) -> &'static [u32] {
    RT_PARTS.get(id as usize)
}

#[inline]
pub fn write_check(id: u32) -> &'static WriteCheck {
    WRITE_CHECKS.get(id as usize)
}

#[inline]
pub fn iface(id: u32) -> &'static [Sym] {
    IFACES.get(id as usize)
}

pub struct ProgramMeta {
    pub files: Vec<String>,
    pub globals: Vec<GlobalProto>,
    pub rt_types: Vec<RtType>,
    pub write_checks: usize,
    pub ifaces: Vec<Vec<Sym>>,
    pub loaded_funcs: usize,
    pub loaded_classes: usize,
    pub loaded_natives: usize,
    pub loaded_builtin_methods: usize,
    /// Well-known classes resolved by name after loading the prelude.
    pub ok_class: Option<u32>,
    pub err_class: Option<u32>,
}

pub static META: LazyLock<RwLock<ProgramMeta>> = LazyLock::new(|| {
    RwLock::new(ProgramMeta {
        files: Vec::new(),
        globals: Vec::new(),
        rt_types: Vec::new(),
        write_checks: 0,
        ifaces: Vec::new(),
        loaded_funcs: 0,
        loaded_classes: 0,
        loaded_natives: 0,
        loaded_builtin_methods: 0,
        ok_class: None,
        err_class: None,
    })
});

static RESULT_CLASSES: AtomicU64 = AtomicU64::new(u64::MAX);

#[inline(always)]
pub fn func(id: u32) -> &'static Func {
    FUNCS.get(id as usize)
}

pub fn func_count() -> usize {
    FUNCS.len()
}

#[inline(always)]
pub fn native(id: u32) -> NativeFn {
    *NATIVE_IDS.get(id as usize)
}

#[inline(always)]
pub fn global(idx: usize) -> &'static AtomicU64 {
    &GLOBALS[idx]
}

pub fn global_count() -> usize {
    GLOBAL_COUNT.load(Ordering::Acquire)
}

/// (Ok class id, Err class id) of the prelude's `Result`.
pub fn result_classes() -> (u32, u32) {
    let v = RESULT_CLASSES.load(Ordering::Relaxed);
    assert!(v != u64::MAX, "Result is not loaded");
    ((v >> 32) as u32, v as u32)
}

static NAMED_CLASSES: LazyLock<RwLock<FxHashMap<String, u32>>> = LazyLock::new(Default::default);

/// The id of the first loaded class with this name. The prelude loads first, so for a prelude
/// class name this is the prelude's class even if the program declares one with the same name;
/// natives use it to build instances of prelude classes (`Regex`, `Match`, ...).
pub fn class_named(name: &str) -> Option<u32> {
    NAMED_CLASSES.read().get(name).copied()
}

/// Lookup of native implementations by name, provided by the standard library.
pub type NativeLookup = dyn Fn(&str) -> Option<NativeFn>;

struct GlobalRoots;

impl RootSource for GlobalRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        for i in 0..global_count() {
            mark_value(t, Value(GLOBALS[i].load(Ordering::Relaxed)));
        }
        for i in 0..FUNCS.len() {
            for &c in FUNCS.get(i).consts.iter() {
                mark_value(t, c);
            }
        }
    }
}

static ROOTS_INSTALLED: std::sync::Once = std::sync::Once::new();

#[derive(Debug)]
pub struct LoadError(pub String);

/// Loads the parts of `p` not loaded yet. Must run on a registered thread (`m`) with no other
/// thread executing code that could observe half-loaded classes (startup, or the REPL between
/// inputs).
pub fn load(p: &Program, m: &mut Mutator, natives: &NativeLookup) -> Result<(), LoadError> {
    ROOTS_INSTALLED.call_once(|| catallaxy_gc::add_global_roots(Box::new(GlobalRoots)));
    assert_eq!(ObjKind::Tuple as u8, KIND_TUPLE, "IsKind tuple operand out of date");
    let _nogc = catallaxy_gc::NoGcGuard::new();
    // Loaded constants and globals are reached through global roots, which minor collections do
    // not scan: allocate them old.
    m.set_young_alloc(false);
    let r = load_inner(p, m, natives);
    m.set_young_alloc(true);
    r
}

fn load_inner(p: &Program, m: &mut Mutator, natives: &NativeLookup) -> Result<(), LoadError> {
    let mut meta = META.write();
    let _ = &*class::BUILTINS;

    // Natives referenced by the program.
    let mut natives_vec = NATIVES.lock();
    for i in meta.loaded_natives..p.natives.len() {
        let name = &p.natives[i];
        let f = natives(name).ok_or_else(|| LoadError(format!("unknown native function `{name}`")))?;
        natives_vec.push(f);
        NATIVE_IDS.push_at(i, f);
    }
    meta.loaded_natives = p.natives.len();
    drop(natives_vec);

    // Classes (ids must match registration order).
    for c in &p.classes[meta.loaded_classes..] {
        let mut spec = ClassSpec::new(ObjKind::Instance, &c.name);
        spec.capability = match c.capability {
            ClassCap::Owned => Capability::Owned,
            ClassCap::Frozen => Capability::Frozen,
            ClassCap::Shared => Capability::Shared,
        };
        spec.superclass = c.superclass.map(class::class_by_id);
        spec.field_names = c.fields.iter().map(|f| intern(f)).collect();
        spec.methods = c.methods.iter().map(|(n, f)| (intern(n), *f)).collect();
        spec.vtable = c.vtable.clone();
        if let Some((e, tag)) = c.variant {
            spec.enum_id = Some(e);
            spec.variant_tag = Some(tag);
        }
        let cls = register_class(spec);
        if cls.id != c.id {
            return Err(LoadError(format!("class id mismatch for {}: {} != {}", c.name, cls.id, c.id)));
        }
        NAMED_CLASSES.write().entry(c.name.clone()).or_insert(c.id);
        if c.name == "Ok" || c.name == "Err" {
            if c.name == "Ok" {
                meta.ok_class = Some(c.id);
            } else {
                meta.err_class = Some(c.id);
            }
        }
    }
    meta.loaded_classes = p.classes.len();
    if let (Some(o), Some(e)) = (meta.ok_class, meta.err_class) {
        RESULT_CLASSES.store(((o as u64) << 32) | e as u64, Ordering::Relaxed);
    }

    // Functions.
    for (i, f) in p.funcs.iter().enumerate().skip(meta.loaded_funcs) {
        let consts: Vec<Value> = f
            .consts
            .iter()
            .map(|c| match c {
                Const::Int(i) => m.int(*i),
                Const::BigInt(s) => m.int_from_big(s.parse().expect("bigint constant")),
                Const::Float(x) => Value::from_f64(*x),
                Const::Str(s) => m.alloc_str(s),
            })
            .collect();
        let native = match &f.native {
            Some(name) => Some(natives(name).ok_or_else(|| LoadError(format!("unknown native function `{name}`")))?),
            None => None,
        };
        let ics = f
            .ics
            .iter()
            .map(|ic| Ic { name: intern(&ic.name), cache: AtomicU64::new(0), fallback: ic.fallback.unwrap_or(NO_FUNC) })
            .collect();
        FUNCS.push_at(
            i,
            Func {
                id: i as u32,
                name: f.name.clone(),
                file: f.file,
                nparams: f.nparams as usize,
                nregs: f.nregs.max(f.nparams).max(1) as usize,
                code: f.code.clone().into_boxed_slice(),
                lines: f.lines.clone().into_boxed_slice(),
                consts: consts.into_boxed_slice(),
                ics,
                captures: f.captures.clone().into_boxed_slice(),
                native,
            },
        );
    }
    meta.loaded_funcs = p.funcs.len();

    // Methods on builtin classes (prelude and extensions), for dynamic dispatch.
    if p.builtin_methods.len() > meta.loaded_builtin_methods {
        let mut per_class: FxHashMap<u32, FxHashMap<Sym, u32>> = FxHashMap::default();
        for &(cid, ref name, fid) in &p.builtin_methods {
            per_class.entry(cid).or_default().insert(intern(name), fid);
        }
        for (cid, methods) in per_class {
            let cls = class::class_by_id(cid);
            let mut all = cls.methods().clone();
            all.extend(methods);
            class::set_methods(cls, all);
            if cid == class::ids::INT {
                let big = class::class_by_id(class::ids::BIGINT);
                class::set_methods(big, cls.methods().clone());
            }
        }
        meta.loaded_builtin_methods = p.builtin_methods.len();
    }

    meta.files = p.files.clone();
    meta.globals = p.globals.clone();
    GLOBAL_COUNT.store(p.globals.len(), Ordering::Release);
    for i in meta.rt_types.len()..p.rt_types.len() {
        RT_TYPES.push_at(i, p.rt_types[i].clone());
        RT_PARTS.push_at(i, p.rt_parts.get(i).cloned().unwrap_or_default());
    }
    meta.rt_types = p.rt_types.clone();
    for i in meta.write_checks..p.write_checks.len() {
        WRITE_CHECKS.push_at(i, p.write_checks[i].clone());
    }
    meta.write_checks = p.write_checks.len();
    for i in meta.ifaces.len()..p.ifaces.len() {
        IFACES.push_at(i, p.ifaces[i].iter().map(|n| intern(n)).collect());
    }
    meta.ifaces = p.ifaces.iter().map(|names| names.iter().map(|n| intern(n)).collect()).collect();
    Ok(())
}

pub fn file_name(id: u32) -> String {
    META.read().files.get(id as usize).cloned().unwrap_or_else(|| "<unknown>".into())
}

pub fn global_name(idx: usize) -> String {
    META.read().globals.get(idx).map(|g| g.name.clone()).unwrap_or_default()
}
