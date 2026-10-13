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
        unsafe { as_list(list).push(s, Access::Local) };
        // Garbage.
        let _ = h.m.alloc_str("garbage that should be reclaimed");
        let _ = h.m.alloc_tuple(&[Value::small_int(i as i64)]);
    }
    h.gc();
    h.gc();
    let list = h.m.get(hl);
    let items = unsafe { as_list(list) }.to_vec();
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
        unsafe { as_dict(d).table().insert_new(hash, k, Value::small_int(i as i64), Access::Local) };
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
    let sum = ops::int_add(h.m, big, Value::small_int(1));
    assert_eq!(ops::format_int(sum), ((1i64 << 50) + 1).to_string());
    let back = ops::int_sub(h.m, sum, big);
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
    assert_eq!(ownership::check_write(h.m, hdr(outer)).unwrap(), Access::Local);
    ownership::freeze_deep(h.m, outer).unwrap();
    assert!(ownership::check_write(h.m, hdr(outer)).is_err());
    assert!(ownership::check_write(h.m, hdr(inner)).is_err(), "freezing is deep");
    assert_eq!(ownership::check_read(h.m, hdr(inner)).unwrap(), Access::Local);

    let child = h.m.alloc_list(Vec::new());
    let shared = h.m.alloc_list(vec![child]);
    h.m.root(shared);
    ownership::share_deep(h.m, shared, false).unwrap();
    assert_eq!(ownership::check_write(h.m, hdr(shared)).unwrap(), Access::Shared);
    ownership::write_list(h.m, shared, |l, access| unsafe { l.push(Value::NIL, access) }).unwrap();

    // Transfer to another (fake) owner: now foreign to us.
    let mine = h.m.alloc_list(Vec::new());
    h.m.root(mine);
    ownership::transfer_deep(mine, h.m.tid, 999);
    assert!(ownership::check_read(h.m, hdr(mine)).is_err());
    ownership::transfer_deep(mine, 999, h.m.tid);
    assert!(ownership::check_read(h.m, hdr(mine)).is_ok());
    h.gc();
    h.finish();
}

#[test]
fn shared_list_is_safe_under_concurrent_pushes() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = Host::new();
    let list = h.m.alloc_list(Vec::new());
    h.m.root(list);
    ownership::share_deep(h.m, list, false).unwrap();
    let addr = list.0;
    let per = n(300);
    let threads: Vec<_> = (0..3)
        .map(|_| {
            std::thread::spawn(move || {
                let mut w = Host::new();
                let l = Value(addr);
                for i in 0..per {
                    ownership::write_list(w.m, l, |l, access| unsafe { l.push(Value::small_int(i as i64), access) })
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
    let len = ownership::read_list(h.m, list).unwrap().len();
    assert_eq!(len, 3 * per);
    h.finish();
}

#[test]
fn shared_list_reads_do_not_lock_and_stay_safe() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let items: Vec<Value> = (0..8).map(|i| h.m.alloc_str(&format!("s{i}"))).collect();
    let list = h.m.alloc_list(items);
    h.m.root(list);
    ownership::share_deep(h.m, list, false).unwrap();
    let addr = list.0;
    // A reader finishes while another thread holds the list's lock.
    {
        let _g = ownership::lock_obj(h.m, unsafe { &*list.as_ptr() });
        let reader = std::thread::spawn(move || {
            let w = Host::new();
            let n = ownership::read_list(w.m, Value(addr)).unwrap().to_vec().len();
            w.finish();
            n
        });
        assert_eq!(catallaxy_gc::blocking(&h.m.rec, || reader.join().unwrap()), 8);
    }
    let rounds = n(200);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let w = Host::new();
                let l = Value(addr);
                let mut seen = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let list = ownership::read_list(w.m, l).unwrap();
                    for i in 0..list.len() + 2 {
                        if let Some(v) = list.get(i) {
                            assert!(v.is_nil() || str_of(v).is_some_and(|s| s.starts_with('s')));
                            seen += 1;
                        }
                    }
                    catallaxy_gc::safepoint(&w.m.rec);
                }
                assert!(seen > 0);
                w.finish();
            })
        })
        .collect();
    for r in 0..rounds {
        let s = h.m.alloc_str(&format!("s{r}"));
        ownership::publish_into_shared(h.m, s).unwrap();
        ownership::write_list(h.m, list, |l, access| unsafe {
            for _ in 0..(r % 7) {
                l.push(s, access);
            }
            if r % 3 == 0 {
                l.pop();
            }
            if r % 11 == 0 {
                let keep: Vec<Value> = l.to_vec().into_iter().take(3).collect();
                l.replace(&keep, access);
            }
            if r % 13 == 0 {
                let mut big = l.to_vec();
                big.extend(std::iter::repeat_n(s, 40));
                l.replace(&big, access);
            }
        })
        .unwrap();
        if r % 20 == 0 {
            h.gc();
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    catallaxy_gc::blocking(&h.m.rec, || {
        for t in readers {
            t.join().unwrap();
        }
    });
    h.finish();
}

#[allow(dead_code)]
fn unused(_: UnsafeCell<()>) {}

#[test]
fn shared_dict_seqlock_reads_stay_consistent() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let d = h.m.alloc_dict(DictTable::new());
    h.m.root(d);
    ownership::share_deep(h.m, d, false).unwrap();
    // Keys are small ints, so a lookup never needs to dereference a key.
    let keys = n(64) as i64;
    let addr = d.0;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut w = Host::new();
                let d = Value(addr);
                let mut hits = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for k in 0..keys {
                        let key = Value::small_int(k);
                        if let Some(idx) = ops::dict_lookup(&mut w, d, key).unwrap() {
                            // A value, once seen for key k, is always k * 10 or k * 10 + 1.
                            let e = ownership::read_dict(w.m, d, |t| t.entry(idx)).unwrap();
                            if e.key == key {
                                let v = e.value.as_small_int();
                                assert!(v == k * 10 || v == k * 10 + 1, "torn read: {k} -> {v}");
                                hits += 1;
                            }
                        }
                    }
                    let n = ownership::read_dict(w.m, d, |t| t.iter().count()).unwrap();
                    assert!(n <= keys as usize);
                    catallaxy_gc::safepoint(&w.m.rec);
                }
                assert!(hits > 0);
                w.finish();
            })
        })
        .collect();
    for round in 0..n(300) as i64 {
        for k in 0..keys {
            let key = Value::small_int(k);
            let hash = ops::hash_value(&mut h, key).unwrap();
            let found = ops::dict_lookup(&mut h, d, key).unwrap();
            ownership::write_dict(h.m, d, |t, access| unsafe {
                match found {
                    Some(idx) if (round + k) % 5 == 0 => {
                        t.remove_at(idx, access);
                    }
                    Some(idx) => t.set_value_at(idx, Value::small_int(k * 10 + (round & 1)), access),
                    None => t.insert_new(hash, key, Value::small_int(k * 10), access),
                }
            })
            .unwrap();
        }
        if round % 25 == 0 {
            h.gc();
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    catallaxy_gc::blocking(&h.m.rec, || {
        for t in readers {
            t.join().unwrap();
        }
    });
    h.finish();
}

/// Throughput of lookups on a shared dict from 1..8 threads: the seqlock path against taking the
/// object's lock on every read (the 0.1 behavior). Run with
/// `cargo test --release -p catallaxy-runtime --test objects -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_shared_dict_reads() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let d = h.m.alloc_dict(DictTable::new());
    h.m.root(d);
    for k in 0..1000 {
        let key = Value::small_int(k);
        let hash = ops::hash_value(&mut h, key).unwrap();
        unsafe { as_dict(d).table().insert_new(hash, key, key, Access::Local) };
    }
    ownership::share_deep(h.m, d, false).unwrap();
    let addr = d.0;
    for locked in [true, false] {
        for threads in [1, 2, 4, 8] {
            let per = 4_000_000 / threads;
            let start = std::time::Instant::now();
            let ts: Vec<_> = (0..threads)
                .map(|_| {
                    std::thread::spawn(move || {
                        let w = Host::new();
                        let d = Value(addr);
                        let hdr = unsafe { &*d.as_ptr() };
                        let mut sum = 0i64;
                        for i in 0..per {
                            let key = Value::small_int((i % 1000) as i64);
                            let f = |t: &DictTable| match t.probe(ops::hash_int(key.as_small_int()), key) {
                                catallaxy_runtime::dict::Probe::Found(ix) => t.entry(ix).value.as_small_int(),
                                _ => 0,
                            };
                            sum += if locked {
                                let _g = ownership::lock_obj(w.m, hdr);
                                f(unsafe { as_dict(d) }.table())
                            } else {
                                ownership::read_dict(w.m, d, f).unwrap()
                            };
                        }
                        assert!(sum > 0);
                        w.finish();
                    })
                })
                .collect();
            catallaxy_gc::blocking(&h.m.rec, || ts.into_iter().for_each(|t| t.join().unwrap()));
            let secs = start.elapsed().as_secs_f64();
            println!(
                "{:>7} {} threads: {:6.1} M lookups/s",
                if locked { "lock" } else { "seqlock" },
                threads,
                4.0 / secs
            );
        }
    }
    h.finish();
}
