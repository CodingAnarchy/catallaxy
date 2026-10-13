//! Native implementations of the Catallaxy standard library.
//!
//! Each native receives the calling thread's [`Vm`] and its arguments (the receiver first for
//! methods). A missing optional argument arrives as the empty marker; use [`opt`].

mod coll;
mod conc;
mod core;
pub mod fiber;
mod net;
mod script;
mod sys;

use std::io::Write;
use std::sync::OnceLock;

use catallaxy_runtime::class::class_of;
use catallaxy_runtime::object::{kind_of, str_of};
use catallaxy_runtime::ops;
use catallaxy_runtime::ownership;
use catallaxy_runtime::{panic, ObjKind, RtResult, Trap, Value};
use catallaxy_vm::{NativeFn, Vm};
use parking_lot::Mutex;

/// Command-line arguments passed to the program (after the script path).
pub static ARGS: OnceLock<Vec<String>> = OnceLock::new();

static STDOUT: Mutex<Option<std::io::BufWriter<std::io::Stdout>>> = Mutex::new(None);

/// Writes to the buffered standard output.
pub fn write_out(s: &str) {
    let mut g = STDOUT.lock();
    let w = g.get_or_insert_with(|| std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()));
    let _ = w.write_all(s.as_bytes());
}

/// Flushes buffered standard output (at exit, before reading input, before errors).
pub fn flush() {
    if let Some(w) = STDOUT.lock().as_mut() {
        let _ = w.flush();
    }
}

static EXTENSIONS: std::sync::LazyLock<parking_lot::RwLock<rustc_hash::FxHashMap<String, NativeFn>>> =
    std::sync::LazyLock::new(Default::default);

/// Registers a native function implemented by a Rust extension crate. Catallaxy code declares it
/// with `native fn name(...) -> T` (a top-level `native fn` uses its plain name; a `native fn` in
/// a class `C` uses `C.name`). Must be called before the program is loaded.
pub fn register_native(name: &str, f: NativeFn) {
    EXTENSIONS.write().insert(name.to_string(), f);
}

/// Looks up a native implementation by name: the standard library, then extensions.
pub fn lookup(name: &str) -> Option<NativeFn> {
    core::lookup(name)
        .or_else(|| coll::lookup(name))
        .or_else(|| conc::lookup(name))
        .or_else(|| sys::lookup(name))
        .or_else(|| script::lookup(name))
        .or_else(|| net::lookup(name))
        .or_else(|| EXTENSIONS.read().get(name).copied())
}

// ---- argument helpers ------------------------------------------------------------------------

pub(crate) fn opt(v: Value) -> Option<Value> {
    if v.is_empty_marker() {
        None
    } else {
        Some(v)
    }
}

pub(crate) fn arg(args: &[Value], i: usize) -> Option<Value> {
    args.get(i).copied().and_then(opt)
}

pub(crate) fn s<'a>(v: Value) -> RtResult<&'a str> {
    match str_of(v) {
        Some(s) => Ok(s),
        None => panic(format!("TypeError: expected String, got {}", class_of(v).name)),
    }
}

/// The string object behind `v`.
pub(crate) fn str_obj<'a>(v: Value) -> RtResult<&'a catallaxy_runtime::object::StrObj> {
    s(v)?;
    Ok(unsafe { catallaxy_runtime::object::as_str_obj(v) })
}

pub(crate) fn int(v: Value) -> RtResult<i64> {
    if v.is_small_int() {
        return Ok(v.as_small_int());
    }
    if ops::is_int(v) {
        return match ops::int_as_i64(v) {
            Some(i) => Ok(i),
            None => panic("RangeError: integer too large"),
        };
    }
    panic(format!("TypeError: expected Int, got {}", class_of(v).name))
}

pub(crate) fn float(v: Value) -> RtResult<f64> {
    if v.is_float() {
        Ok(v.as_f64())
    } else if ops::is_int(v) {
        Ok(ops::int_to_f64(v))
    } else {
        panic(format!("TypeError: expected Float, got {}", class_of(v).name))
    }
}

pub(crate) fn new_str(vm: &mut Vm, s: &str) -> Value {
    vm.m.alloc_str(s)
}

pub(crate) fn new_list(vm: &mut Vm, items: Vec<Value>) -> Value {
    vm.m.alloc_list(items)
}

/// A snapshot of a list's items (with ownership checks).
pub(crate) fn list_items(vm: &Vm, l: Value) -> RtResult<Vec<Value>> {
    if kind_of(l) != ObjKind::List {
        return panic(format!("TypeError: expected List, got {}", class_of(l).name));
    }
    Ok(ownership::read_list(&vm.m, l)?.to_vec())
}

pub(crate) fn to_s(vm: &mut Vm, v: Value) -> RtResult<String> {
    ops::to_s(vm, v)
}

pub(crate) fn ok(vm: &mut Vm, v: Value) -> Value {
    catallaxy_vm::slow::make_result(vm, true, v)
}

pub(crate) fn err(vm: &mut Vm, msg: &str) -> Value {
    let s = new_str(vm, msg);
    catallaxy_vm::slow::make_result(vm, false, s)
}

pub(crate) fn exit_trap(code: i32) -> Trap {
    Trap::Exit(code)
}

/// Runs a blocking operation with the thread in native state.
pub(crate) fn blocking<R>(vm: &Vm, f: impl FnOnce() -> R) -> R {
    flush();
    catallaxy_gc::blocking(&vm.m.rec, f)
}
