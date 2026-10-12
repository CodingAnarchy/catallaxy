//! Object model tests: allocation, containers, ownership states and structural operations,
//! with collections running in between. Run under Miri as part of the safety gates.

use std::cell::UnsafeCell;
use std::sync::Mutex as StdMutex;

use catallaxy_gc::{enter_native, install, leave_native, register_thread, unregister_thread, RootSource, Tracer};
use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops::{self, Interop};
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::{Mutator, RtResult, Sym, Value};

/// A minimal host: one mutator whose roots are its handle stack.
struct Host {
    /// Shared with the collector's root scan, so owned through a raw pointer.
    raw: *mut Mutator,
    m: &'static mut Mutator,
}

struct HandleRoots(*const Vec<Value>);
impl RootSource for HandleRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        for &v in unsafe { &*self.0 } {
            if v.is_ptr() {
                t.mark(v.as_ptr());
            }
        }
    }
}

impl Interop for Host {
    fn mutator(&mut self) -> &mut Mutator {
        self.m
    }
    fn call_method(&mut self, _: Value, _: Sym, _: &[Value]) -> RtResult<Option<Value>> {
        Ok(None)
    }
}

impl Host {
    fn new() -> Self {
        let rec = register_thread();
        let raw = Box::into_raw(Box::new(Mutator::new(rec)));
        let roots: Box<dyn RootSource> = Box::new(HandleRoots(unsafe { (*raw).handles_ptr() }));
        let m = unsafe { &mut *raw };
        unsafe { install(&m.rec, Box::into_raw(roots), m.heap_ptr()) };
        leave_native(&m.rec);
        Host { raw, m }
    }
    fn gc(&mut self) {
        catallaxy_gc::collect_now(&self.m.rec);
    }
    fn finish(self) {
        enter_native(&self.m.rec);
        unregister_thread(&self.m.rec);
        self.m.release_pages();
        // The mutator is intentionally leaked: the collector may still hold its root pointer.
        let _ = self.raw;
    }
}

static SERIAL: StdMutex<()> = StdMutex::new(());

fn n(i: usize) -> usize {
    if cfg!(miri) {
        (i / 50).max(3)
    } else {
        i
    }
}

#[test]
fn containers_survive_collection() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let list = h.m.alloc_list(Vec::new());
    let hl = h.m.root(list);
    for i in 0..n(500) {
        let s = h.m.alloc_str(&format!("item-{i}"));
        unsafe { as_list(list).items_mut().push(s) };
        // Garbage.
        let _ = h.m.alloc_str("garbage that should be reclaimed");
        let _ = h.m.alloc_tuple(&[Value::small_int(i as i64)]);
    }
    h.gc();
    h.gc();
    let list = h.m.get(hl);
    let items = unsafe { as_list(list).items() }.clone();
    assert_eq!(items.len(), n(500));
    for (i, v) in items.iter().enumerate() {
        assert_eq!(str_of(*v), Some(format!("item-{i}").as_str()));
    }
    // Dicts: insert, look up, delete.
    let d = h.m.alloc_dict(DictTable::new());
    let hd = h.m.root(d);
    for i in 0..n(200) {
        let k = h.m.alloc_str(&format!("k{i}"));
        let hash = ops::hash_value(&mut h, k).unwrap();
        unsafe { as_dict(d).table_mut() }.insert_new(hash, k, Value::small_int(i as i64));
    }
    h.gc();
    let d = h.m.get(hd);
    for i in 0..n(200) {
        let k = h.m.alloc_str(&format!("k{i}"));
        let idx = ops::dict_lookup(&mut h, d, k).unwrap().expect("key present");
        assert_eq!(unsafe { as_dict(d).table() }.entry(idx).value, Value::small_int(i as i64));
    }
    h.finish();
}

#[test]
fn big_integers_and_equality() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let big = h.m.int(1 << 50);
    assert!(big.is_ptr());
    let sum = ops::int_add(&mut h.m, big, Value::small_int(1));
    assert_eq!(ops::format_int(sum), ((1i64 << 50) + 1).to_string());
    let back = ops::int_sub(&mut h.m, sum, big);
    assert!(back.is_small_int() && back.as_small_int() == 1);
    let a = h.m.alloc_list(vec![Value::small_int(1), big]);
    let big2 = h.m.int(1 << 50);
    let b = h.m.alloc_list(vec![Value::small_int(1), big2]);
    h.m.root(a);
    h.m.root(b);
    h.gc();
    assert!(ops::values_eq(&mut h, a, b).unwrap());
    assert_eq!(ops::hash_value(&mut h, a).unwrap(), ops::hash_value(&mut h, b).unwrap());
    assert!(ops::values_eq(&mut h, Value::small_int(2), Value::from_f64(2.0)).unwrap());
    let s = ops::inspect(&mut h, a).unwrap();
    assert_eq!(s, "[1, 1125899906842624]");
    h.finish();
}

#[test]
fn ownership_states() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let inner = h.m.alloc_list(vec![Value::small_int(1)]);
    let outer = h.m.alloc_list(vec![inner]);
    h.m.root(outer);
    let hdr = |v: Value| unsafe { &*v.as_ptr() };
    assert_eq!(ownership::check_write(&h.m, hdr(outer)).unwrap(), Access::Local);
    ownership::freeze_deep(&h.m, outer).unwrap();
    assert!(ownership::check_write(&h.m, hdr(outer)).is_err());
    assert!(ownership::check_write(&h.m, hdr(inner)).is_err(), "freezing is deep");
    assert_eq!(ownership::check_read(&h.m, hdr(inner)).unwrap(), Access::Local);

    let child = h.m.alloc_list(Vec::new());
    let shared = h.m.alloc_list(vec![child]);
    h.m.root(shared);
    ownership::share_deep(&h.m, shared, false).unwrap();
    assert_eq!(ownership::check_write(&h.m, hdr(shared)).unwrap(), Access::Shared);
    ownership::with_write(&h.m, shared, |_| unsafe { as_list(shared).items_mut() }.push(Value::NIL)).unwrap();

    // Transfer to another (fake) owner: now foreign to us.
    let mine = h.m.alloc_list(Vec::new());
    h.m.root(mine);
    ownership::transfer_deep(mine, h.m.tid, 999);
    assert!(ownership::check_read(&h.m, hdr(mine)).is_err());
    ownership::transfer_deep(mine, 999, h.m.tid);
    assert!(ownership::check_read(&h.m, hdr(mine)).is_ok());
    h.gc();
    h.finish();
}

#[test]
fn shared_list_is_safe_under_concurrent_pushes() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let list = h.m.alloc_list(Vec::new());
    h.m.root(list);
    ownership::share_deep(&h.m, list, false).unwrap();
    let addr = list.0;
    let per = n(300);
    let threads: Vec<_> = (0..3)
        .map(|_| {
            std::thread::spawn(move || {
                let mut w = Host::new();
                let l = Value(addr);
                for i in 0..per {
                    ownership::with_write(&w.m, l, |_| unsafe { as_list(l).items_mut() }.push(Value::small_int(i as i64)))
                        .unwrap();
                    if i % 50 == 0 {
                        catallaxy_gc::safepoint(&w.m.rec);
                    }
                }
                w.gc();
                w.finish();
            })
        })
        .collect();
    // Keep participating in collections while the workers run.
    catallaxy_gc::blocking(&h.m.rec, || {
        for t in threads {
            t.join().unwrap();
        }
    });
    let len = ownership::with_read(&h.m, list, || unsafe { as_list(list).items() }.len()).unwrap();
    assert_eq!(len, 3 * per);
    h.finish();
}

#[allow(dead_code)]
fn unused(_: UnsafeCell<()>) {}
