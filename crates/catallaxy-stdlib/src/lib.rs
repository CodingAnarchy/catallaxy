//! Native implementations of the Catallaxy standard library.
//!
//! Each native receives the calling thread's [`Vm`] and its arguments (the receiver first for
//! methods). A missing optional argument arrives as the empty marker; use [`opt`].

mod coll;
mod conc;
mod core;
mod sys;

use std::io::Write;
use std::sync::OnceLock;

use catallaxy_runtime::class::class_of;
use catallaxy_runtime::object::{as_list, kind_of, str_of};
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

/// Looks up a native implementation by name.
pub fn lookup(name: &str) -> Option<NativeFn> {
    core::lookup(name).or_else(|| coll::lookup(name)).or_else(|| conc::lookup(name)).or_else(|| sys::lookup(name))
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
    ownership::with_read(&vm.m, l, || unsafe { as_list(l).items() }.clone())
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
