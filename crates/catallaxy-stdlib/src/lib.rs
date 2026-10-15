//! Native implementations of the Catallaxy standard library.
//!
//! Each native receives the calling thread's [`Vm`] and its arguments (the receiver first for
//! methods). A missing optional argument arrives as the empty marker; use [`opt`].

mod coll;
mod conc;
pub use conc::report_unjoined_panics;
mod core;
mod data;
pub mod fiber;
mod net;
mod script;
mod sys;

use std::io::Write;
use std::sync::OnceLock;

use catallaxy_runtime::class::class_of;
use catallaxy_runtime::object::str_of;
use catallaxy_runtime::ops;
use catallaxy_runtime::ownership;
use catallaxy_runtime::{panic, try_push, try_vec, RtResult, Trap, Value};
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

/// The Catallaxy panic message for a Rust panic caught at a thread's or fiber's entry (an
/// internal error of the runtime), or `None` if the payload is not a panic message (such as the
/// forced unwind that drops a suspended fiber), which the catcher must resume.
pub fn internal_error_message(payload: &(dyn std::any::Any + Send)) -> Option<String> {
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        payload.downcast_ref::<String>()?.clone()
    };
    Some(format!("internal error: {msg}"))
}

/// Appends up to eight frames of `trace` to a panic message, as a thread's panic report does.
fn with_trace(mut msg: String, trace: &[String]) -> String {
    for line in trace.iter().take(8) {
        msg.push_str(&format!("\n    at {line}"));
    }
    msg
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
    // Natives are looked up while the program loads: start the monotonic clock there, so
    // `Time.monotonic` and `Instant` count from the start of the program.
    sys::init_clock();
    core::lookup(name)
        .or_else(|| coll::lookup(name))
        .or_else(|| conc::lookup(name))
        .or_else(|| sys::lookup(name))
        .or_else(|| script::lookup(name))
        .or_else(|| net::lookup(name))
        .or_else(|| data::lookup(name))
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
    catallaxy_runtime::object::str_obj_of(v)
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

/// `v` itself if it is an Int (small or big), else a `TypeError` panic.
pub(crate) fn int_value(v: Value) -> RtResult<Value> {
    if ops::is_int(v) {
        Ok(v)
    } else {
        catallaxy_runtime::object::type_error("Int", v)
    }
}

/// `v` itself if it is a Bool, else a `TypeError` panic.
pub(crate) fn bool_value(v: Value) -> RtResult<Value> {
    if v.is_bool() {
        Ok(v)
    } else {
        catallaxy_runtime::object::type_error("Bool", v)
    }
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

/// A list holding `items` (taking over the vector), or a `MemoryError` panic if its buffer
/// cannot be had. Nothing roots `items`, so there is no collection and retry.
pub(crate) fn new_list(vm: &mut Vm, items: Vec<Value>) -> RtResult<Value> {
    vm.m.try_alloc_list(items)
}

/// A list of new strings, one per part, or a `MemoryError` panic. The parts may point into a
/// rooted string: allocation never moves objects, and nothing here collects. A native that
/// holds no unrooted values should call `vm.m.collect_if_refused()` first.
pub(crate) fn new_str_list<'a>(vm: &mut Vm, parts: impl IntoIterator<Item = &'a str>) -> RtResult<Value> {
    let parts = parts.into_iter();
    let mut items = try_vec(parts.size_hint().0)?;
    for p in parts {
        let v = vm.m.try_alloc_str_nogc(p)?;
        try_push(&mut items, v)?;
    }
    new_list(vm, items)
}

/// A snapshot of a list's items (with ownership checks), or a `MemoryError` panic.
pub(crate) fn list_items(vm: &Vm, l: Value) -> RtResult<Vec<Value>> {
    ownership::read_list(&vm.m, l)?.try_to_vec()
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

/// `Err("path: error")` for a failed file operation, or a `MemoryError` panic if the data did
/// not fit in memory (the standard library reports a refused buffer as an I/O error).
pub(crate) fn io_err(vm: &mut Vm, path: &str, e: std::io::Error) -> RtResult<Value> {
    if e.kind() == std::io::ErrorKind::OutOfMemory {
        return catallaxy_runtime::refused(format!("cannot read {path} into memory"));
    }
    Ok(err(vm, &format!("{path}: {e}")))
}

pub(crate) fn exit_trap(code: i32) -> Trap {
    Trap::Exit(code)
}

/// Runs a blocking operation with the thread in native state.
pub(crate) fn blocking<R>(vm: &Vm, f: impl FnOnce() -> R) -> R {
    flush();
    catallaxy_gc::blocking(&vm.m.rec, f)
}
