//! The C ABI for Catallaxy: embed the language in a C (or any FFI-capable) program and write
//! native functions in C. The interface is declared in `include/catallaxy.h`; read its header
//! comment for the rooting rules C code must follow.
//!
//! Values cross the boundary as their raw 64-bit words (`cx_value`), and a native receives the
//! calling VM as an opaque `cx_vm *`. Native functions registered from C are dispatched through a
//! fixed table of trampolines, because the VM's native-function type carries no user data.
//!
//! With the default `global-allocator` feature, the libraries' Rust code allocates through
//! [`catallaxy_gc::Allocator`]: a refused request releases the runtime's headroom and is retried
//! once, and a `MemoryError` follows at the next safepoint instead of an abort. The C host's own
//! `malloc` is not affected. A Rust program that depends on this crate (as an rlib) and has its
//! own `#[global_allocator]` must turn the feature off (`default-features = false`), since a
//! program has only one. `cx_allocator_active` reports whether it is in effect.

#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)] // the C header documents the contracts

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use catallaxy::{Engine, NativeFn, RtResult, Trap, Value, Vm};
use catallaxy_runtime::object::{kind_of, str_of};
use catallaxy_runtime::{ops, ownership, ObjKind};

#[cfg(feature = "global-allocator")]
#[global_allocator]
static ALLOC: catallaxy_gc::Allocator = catallaxy_gc::Allocator;

/// Opaque handle for the VM running a native call.
pub enum cx_vm {}
pub type cx_value = u64;

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum cx_status {
    CX_OK = 0,
    CX_PANIC = 1,
}

pub type cx_native = unsafe extern "C" fn(
    vm: *mut cx_vm,
    args: *const cx_value,
    nargs: usize,
    out: *mut cx_value,
    user: *mut c_void,
) -> cx_status;

// ---- native dispatch --------------------------------------------------------------------------

const MAX_NATIVES: usize = 256;

#[derive(Copy, Clone)]
struct Slot {
    f: cx_native,
    user: usize,
}

static SLOTS: [OnceLock<Slot>; MAX_NATIVES] = [const { OnceLock::new() }; MAX_NATIVES];
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The trap a native is propagating: set by `cx_panic` or a failed `cx_call`.
    static PENDING: RefCell<Option<Trap>> = const { RefCell::new(None) };
}

fn invoke(slot: usize, vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
    let s = *SLOTS[slot].get().expect("registered native");
    let mut out = Value::NIL.0;
    PENDING.with(|p| p.borrow_mut().take());
    // SAFETY: the callee follows the header's contract; args live in rooted VM registers.
    let status = unsafe {
        (s.f)(
            vm as *mut Vm as *mut cx_vm,
            args.as_ptr() as *const cx_value,
            args.len(),
            &mut out,
            s.user as *mut c_void,
        )
    };
    match status {
        cx_status::CX_OK => Ok(Value(out)),
        cx_status::CX_PANIC => {
            Err(PENDING.with(|p| p.borrow_mut().take()).unwrap_or_else(|| Trap::Panic("native function failed".into())))
        }
    }
}

fn trampoline<const I: usize>(vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
    invoke(I, vm, args)
}

macro_rules! trampolines {
    ($($i:literal)*) => { [$(trampoline::<$i> as NativeFn),*] };
}
static TRAMPOLINES: [NativeFn; MAX_NATIVES] = trampolines!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127 128 129 130 131 132 133 134 135 136 137 138 139 140 141 142 143 144 145 146 147 148 149 150 151 152 153 154 155 156 157 158 159 160 161 162 163 164 165 166 167 168 169 170 171 172 173 174 175 176 177 178 179 180 181 182 183 184 185 186 187 188 189 190 191 192 193 194 195 196 197 198 199 200 201 202 203 204 205 206 207 208 209 210 211 212 213 214 215 216 217 218 219 220 221 222 223 224 225 226 227 228 229 230 231 232 233 234 235 236 237 238 239 240 241 242 243 244 245 246 247 248 249 250 251 252 253 254 255);

unsafe fn vm<'a>(vm: *mut cx_vm) -> &'a mut Vm {
    &mut *(vm as *mut Vm)
}

unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

// ---- engine -----------------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn cx_register(name: *const c_char, f: Option<cx_native>, user: *mut c_void) -> c_int {
    let (Some(name), Some(f)) = (str_arg(name), f) else { return -1 };
    if name.is_empty() {
        return -1;
    }
    let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    if slot >= MAX_NATIVES {
        return -1;
    }
    let _ = SLOTS[slot].set(Slot { f, user: user as usize });
    Engine::new().register(name, TRAMPOLINES[slot]);
    0
}

/// 1 if the library's Rust code allocates through `catallaxy_gc::Allocator`, else 0.
#[no_mangle]
pub extern "C" fn cx_allocator_active() -> c_int {
    catallaxy_gc::Allocator::is_global() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn cx_set_args(argc: c_int, argv: *const *const c_char) {
    let mut args = Vec::new();
    for i in 0..argc.max(0) as usize {
        let p = *argv.add(i);
        if !p.is_null() {
            args.push(CStr::from_ptr(p).to_string_lossy().into_owned());
        }
    }
    Engine::new().set_args(args);
}

fn exit_code(r: Result<i32, String>) -> c_int {
    match r {
        Ok(code) => code,
        Err(diags) => {
            eprint!("{diags}");
            2
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cx_run_file(path: *const c_char) -> c_int {
    let Some(path) = str_arg(path) else { return 2 };
    exit_code(Engine::new().run_file(path.as_ref()))
}

#[no_mangle]
pub unsafe extern "C" fn cx_run_source(name: *const c_char, source: *const c_char) -> c_int {
    let (Some(name), Some(source)) = (str_arg(name), str_arg(source)) else { return 2 };
    exit_code(Engine::new().run_source(name, source))
}

// ---- values -----------------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn cx_nil() -> cx_value {
    Value::NIL.0
}
#[no_mangle]
pub extern "C" fn cx_bool(b: c_int) -> cx_value {
    Value::bool(b != 0).0
}
#[no_mangle]
pub extern "C" fn cx_float(f: f64) -> cx_value {
    Value::from_f64(f).0
}
#[no_mangle]
pub unsafe extern "C" fn cx_int(v: *mut cx_vm, i: i64) -> cx_value {
    vm(v).m.int(i).0
}
#[no_mangle]
pub unsafe extern "C" fn cx_string(v: *mut cx_vm, utf8: *const c_char, len: usize) -> cx_value {
    let bytes = if len == 0 { &[][..] } else { std::slice::from_raw_parts(utf8 as *const u8, len) };
    vm(v).m.alloc_str(&String::from_utf8_lossy(bytes)).0
}
#[no_mangle]
pub unsafe extern "C" fn cx_list(v: *mut cx_vm, items: *const cx_value, n: usize) -> cx_value {
    let items =
        if n == 0 { Vec::new() } else { std::slice::from_raw_parts(items, n).iter().map(|&w| Value(w)).collect() };
    vm(v).m.alloc_list(items).0
}

#[no_mangle]
pub extern "C" fn cx_is_nil(v: cx_value) -> c_int {
    Value(v).is_nil() as c_int
}
#[no_mangle]
pub extern "C" fn cx_is_bool(v: cx_value) -> c_int {
    Value(v).is_bool() as c_int
}
#[no_mangle]
pub extern "C" fn cx_is_int(v: cx_value) -> c_int {
    ops::is_int(Value(v)) as c_int
}
#[no_mangle]
pub extern "C" fn cx_is_float(v: cx_value) -> c_int {
    Value(v).is_float() as c_int
}
#[no_mangle]
pub extern "C" fn cx_is_string(v: cx_value) -> c_int {
    (kind_of(Value(v)) == ObjKind::Str) as c_int
}
#[no_mangle]
pub extern "C" fn cx_is_list(v: cx_value) -> c_int {
    (kind_of(Value(v)) == ObjKind::List) as c_int
}
#[no_mangle]
pub extern "C" fn cx_is_missing(v: cx_value) -> c_int {
    Value(v).is_empty_marker() as c_int
}
#[no_mangle]
pub extern "C" fn cx_truthy(v: cx_value) -> c_int {
    let v = Value(v);
    !(v.is_nil() || v == Value::FALSE) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn cx_to_int(v: cx_value, out: *mut i64) -> c_int {
    let v = Value(v);
    match ops::int_as_i64(v) {
        Some(i) if ops::is_int(v) => {
            *out = i;
            0
        }
        _ => -1,
    }
}
#[no_mangle]
pub unsafe extern "C" fn cx_to_float(v: cx_value, out: *mut f64) -> c_int {
    let v = Value(v);
    if v.is_float() {
        *out = v.as_f64();
    } else if ops::is_int(v) {
        *out = ops::int_to_f64(v);
    } else {
        return -1;
    }
    0
}
#[no_mangle]
pub unsafe extern "C" fn cx_string_data(v: cx_value, len: *mut usize) -> *const c_char {
    match str_of(Value(v)) {
        Some(s) => {
            if !len.is_null() {
                *len = s.len();
            }
            s.as_ptr() as *const c_char
        }
        None => std::ptr::null(),
    }
}
#[no_mangle]
pub unsafe extern "C" fn cx_list_len(v: *mut cx_vm, l: cx_value) -> i64 {
    let l = Value(l);
    if kind_of(l) != ObjKind::List {
        return -1;
    }
    match ownership::read_list(&vm(v).m, l) {
        Ok(list) => list.len() as i64,
        Err(_) => -1,
    }
}
#[no_mangle]
pub unsafe extern "C" fn cx_list_get(v: *mut cx_vm, l: cx_value, i: usize, out: *mut cx_value) -> c_int {
    let l = Value(l);
    if kind_of(l) != ObjKind::List {
        return -1;
    }
    match ownership::read_list(&vm(v).m, l).ok().and_then(|list| list.get(i)) {
        Some(x) => {
            *out = x.0;
            0
        }
        None => -1,
    }
}

// ---- calls, rooting, errors -------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn cx_call(
    v: *mut cx_vm,
    closure: cx_value,
    args: *const cx_value,
    nargs: usize,
    out: *mut cx_value,
) -> cx_status {
    let args: Vec<Value> = if nargs == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(args, nargs).iter().map(|&w| Value(w)).collect()
    };
    match vm(v).call_value(Value(closure), &args) {
        Ok(r) => {
            if !out.is_null() {
                *out = r.0;
            }
            cx_status::CX_OK
        }
        Err(t) => {
            PENDING.with(|p| *p.borrow_mut() = Some(t));
            cx_status::CX_PANIC
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cx_scope_open(v: *mut cx_vm) -> usize {
    vm(v).m.handle_mark()
}
#[no_mangle]
pub unsafe extern "C" fn cx_scope_close(v: *mut cx_vm, mark: usize) {
    let m = &mut vm(v).m;
    if mark <= m.handle_mark() {
        m.release_handles(mark);
    }
}
#[no_mangle]
pub unsafe extern "C" fn cx_root(v: *mut cx_vm, x: cx_value) -> usize {
    let m = &mut vm(v).m;
    m.root(Value(x));
    m.handle_mark() - 1
}
#[no_mangle]
pub unsafe extern "C" fn cx_rooted(v: *mut cx_vm, handle: usize) -> cx_value {
    vm(v).m.handle_values().get(handle).map_or(Value::NIL.0, |x| x.0)
}

#[no_mangle]
pub unsafe extern "C" fn cx_panic(_vm: *mut cx_vm, message: *const c_char) -> cx_status {
    let msg = if message.is_null() {
        "native function failed".to_string()
    } else {
        CStr::from_ptr(message).to_string_lossy().into_owned()
    };
    PENDING.with(|p| *p.borrow_mut() = Some(Trap::Panic(msg)));
    cx_status::CX_PANIC
}
