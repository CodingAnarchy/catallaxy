//! Drives the C ABI through its `extern "C"` entry points, as a C host would. One test: the VM's
//! program state and the native table are process-wide.

use std::ffi::{c_void, CString};
use std::sync::Mutex;

use catallaxy_capi::*;

static SEEN: Mutex<Vec<i64>> = Mutex::new(Vec::new());

/// native fn record(x: Int) -> Nil
unsafe extern "C" fn record(
    _vm: *mut cx_vm,
    args: *const cx_value,
    nargs: usize,
    out: *mut cx_value,
    _: *mut c_void,
) -> cx_status {
    assert_eq!(nargs, 1);
    let mut x = 0;
    assert_eq!(cx_to_int(*args, &mut x), 0);
    SEEN.lock().unwrap().push(x);
    *out = cx_nil();
    cx_status::CX_OK
}

/// native fn scale(x: Int) -> Int, multiplying by the registered user value.
unsafe extern "C" fn scale(
    vm: *mut cx_vm,
    args: *const cx_value,
    _: usize,
    out: *mut cx_value,
    user: *mut c_void,
) -> cx_status {
    let mut x = 0;
    if cx_to_int(*args, &mut x) != 0 {
        return cx_panic(vm, c"scale: not an Int".as_ptr());
    }
    *out = cx_int(vm, x * (user as i64));
    cx_status::CX_OK
}

/// native fn map_strings(f: fn(String) -> String, xs: List[String]) -> List[String]: calls back
/// into the VM for each item, rooting results between calls.
unsafe extern "C" fn map_strings(
    vm: *mut cx_vm,
    args: *const cx_value,
    _: usize,
    out: *mut cx_value,
    _: *mut c_void,
) -> cx_status {
    let mark = cx_scope_open(vm);
    let f = cx_root(vm, *args);
    let xs = cx_root(vm, *args.add(1));
    let n = cx_list_len(vm, cx_rooted(vm, xs));
    let mut handles = Vec::new();
    for i in 0..n as usize {
        let mut item = 0;
        assert_eq!(cx_list_get(vm, cx_rooted(vm, xs), i, &mut item), 0);
        let mut r = 0;
        if cx_call(vm, cx_rooted(vm, f), &item, 1, &mut r) != cx_status::CX_OK {
            return cx_status::CX_PANIC;
        }
        let mut len = 0;
        assert!(!cx_string_data(r, &mut len).is_null());
        handles.push(cx_root(vm, r));
    }
    let items: Vec<cx_value> = handles.iter().map(|&h| cx_rooted(vm, h)).collect();
    *out = cx_list(vm, items.as_ptr(), items.len());
    cx_scope_close(vm, mark);
    cx_status::CX_OK
}

#[test]
fn c_abi_round_trip() {
    unsafe {
        assert_eq!(cx_register(c"record".as_ptr(), Some(record), std::ptr::null_mut()), 0);
        assert_eq!(cx_register(c"scale".as_ptr(), Some(scale), 7 as *mut c_void), 0);
        assert_eq!(cx_register(c"map_strings".as_ptr(), Some(map_strings), std::ptr::null_mut()), 0);
        assert_eq!(cx_register(std::ptr::null(), Some(record), std::ptr::null_mut()), -1);
        assert_eq!(cx_register(c"".as_ptr(), Some(record), std::ptr::null_mut()), -1);

        let src = CString::new(
            r##"native fn record(x: Int) -> Nil
native fn scale(x: Int) -> Int
native fn map_strings(f: fn(String) -> String, xs: List[String]) -> List[String]
record(scale(6))
ys = map_strings({ |s| s.upcase() + (0...300).to_list().map { |i| "#{i}" }.join("").length.to_s() }, ["a", "bc"])
record(ys.length)
record(ys[1].length)
bad = spawn { scale(1) + map_strings({ |s| panic("inner #{s}") }, ["x"]).length }
match bad.join() {
  Ok(_) => record(-1)
  Err(e) => record(if e.contains("inner x") { 1 } else { 0 })
}
"##,
        )
        .unwrap();
        let code = cx_run_source(c"api.cx".as_ptr(), src.as_ptr());
        assert_eq!(code, 0);
    }
    // 6 * 7; two mapped items; "BC" + "790" (the digits of 0..299 are 790 characters); the
    // panic inside the callback propagates out of the native as a thread panic.
    assert_eq!(*SEEN.lock().unwrap(), vec![42, 2, 5, 1]);
    // The test binary links the rlib, so the crate's `global-allocator` feature decides.
    assert_eq!(cx_allocator_active(), cfg!(feature = "global-allocator") as std::ffi::c_int);
    assert_eq!(cx_is_nil(cx_nil()), 1);
    assert_eq!(cx_truthy(cx_bool(0)), 0);
    let mut f = 0.0;
    assert_eq!(unsafe { cx_to_float(cx_float(2.5), &mut f) }, 0);
    assert_eq!(f, 2.5);
}
