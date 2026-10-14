//! Object model tests: allocation, containers, ownership states and structural operations,
//! with collections running in between. Run under Miri as part of the safety gates.

use std::cell::UnsafeCell;

use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops;
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::Value;

mod common;
use common::*;

#[test]
fn lists_built_from_vecs_and_slices() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let strs: Vec<Value> = (0..12).map(|i| h.m.alloc_str(&format!("v{i}"))).collect();
    // A vector with spare capacity: its allocation becomes the list's buffer. (Vectors of up to
    // `ListObj::INLINE_MAX` items are copied into the list object instead; see `inline_lists.rs`.)
    let mut items = Vec::with_capacity(17);
    items.extend_from_slice(&strs);
    let a = h.m.alloc_list(items);
    let ha = h.m.root(a);
    // A vector with no spare capacity, an empty one with capacity, and one with none.
    let b = h.m.alloc_list(strs.clone());
    let hb = h.m.root(b);
    let c = h.m.alloc_list(Vec::with_capacity(3));
    let hc = h.m.root(c);
    let d = h.m.alloc_list(Vec::new());
    let hd = h.m.root(d);
    // Copies of a slice, with and without room to spare.
    let e = h.m.alloc_list_from_slice(&strs[1..4], 9);
    let he = h.m.root(e);
    let f = h.m.alloc_list_from_slice(&[], 0);
    let hf = h.m.root(f);
    h.gc();
    h.gc();
    let names = |v: Value| -> Vec<String> {
        unsafe { as_list(v) }.to_vec().iter().map(|&s| str_of(s).unwrap().to_string()).collect()
    };
    let (a, b, c, d, e, f) = (h.m.get(ha), h.m.get(hb), h.m.get(hc), h.m.get(hd), h.m.get(he), h.m.get(hf));
    let all: Vec<String> = (0..12).map(|i| format!("v{i}")).collect();
    assert_eq!(names(a), all);
    assert_eq!(unsafe { as_list(a) }.capacity(), 17);
    assert_eq!(names(b), all);
    assert_eq!(unsafe { as_list(c) }.capacity(), 3);
    assert!(unsafe { as_list(c) }.is_empty() && unsafe { as_list(d) }.is_empty());
    assert_eq!(names(e), ["v1", "v2", "v3"]);
    assert_eq!(unsafe { as_list(e) }.capacity(), 9);
    assert!(unsafe { as_list(f) }.is_empty());
    // Spare slots are initialized: pushing into them and growing past them works.
    for l in [a, b, c, d, e, f] {
        for i in 0..12 {
            let s = h.m.alloc_str(&format!("p{i}"));
            unsafe { as_list(l).push(s, Access::Local) };
        }
    }
    h.gc();
    let a = h.m.get(ha);
    assert_eq!(names(a).len(), 24);
    assert_eq!(names(a)[23], "p11");
    assert_eq!(names(h.m.get(hc)).len(), 12);
    h.finish();
}

/// The `try_` builders that natives use (a refusal itself is covered by `out_of_memory.rs`,
/// under a real address-space limit) build the same containers as the infallible ones.
#[test]
fn fallible_builders() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let mut strs: Vec<Value> = Vec::new();
    for i in 0..12 {
        strs.push(h.m.try_alloc_str_nogc(&format!("v{i}")).unwrap());
    }
    let mut items = catallaxy_runtime::try_vec(20).unwrap();
    items.extend_from_slice(&strs);
    catallaxy_runtime::try_push(&mut items, strs[0]).unwrap();
    let a = h.m.try_alloc_list(items).unwrap();
    let ha = h.m.root(a);
    let b = h.m.try_alloc_list(strs[..2].to_vec()).unwrap();
    let hb = h.m.root(b);
    let c = h.m.try_alloc_list_from_slice(&strs[3..11], 10).unwrap();
    let hc = h.m.root(c);
    let t = DictTable::try_with_capacity(4).unwrap();
    for (i, &s) in strs.iter().take(6).enumerate() {
        unsafe { t.try_insert_new(i as u64, Value::small_int(i as i64), s, Access::Local) }.unwrap();
    }
    let t2 = t.try_clone().unwrap();
    let d = h.m.alloc_dict(t2);
    let hd = h.m.root(d);
    drop(t);
    h.gc();
    h.gc();
    let names = |v: Value| -> Vec<String> {
        let items = unsafe { as_list(v) }.try_to_vec().unwrap();
        items.iter().map(|&s| str_of(s).unwrap().to_string()).collect()
    };
    let mut all: Vec<String> = (0..12).map(|i| format!("v{i}")).collect();
    all.push("v0".into());
    assert_eq!(names(h.m.get(ha)), all);
    assert_eq!(unsafe { as_list(h.m.get(ha)) }.capacity(), 20);
    assert_eq!(names(h.m.get(hb)), ["v0", "v1"]);
    assert_eq!(names(h.m.get(hc)), all[3..11]);
    assert_eq!(unsafe { as_list(h.m.get(hc)) }.capacity(), 10);
    let d = h.m.get(hd);
    let entries: Vec<String> = ownership::read_dict(h.m, d, |t| {
        t.iter().map(|e| format!("{}={}", ops::int_as_i64(e.key).unwrap(), str_of(e.value).unwrap())).collect()
    })
    .unwrap();
    assert_eq!(entries, ["0=v0", "1=v1", "2=v2", "3=v3", "4=v4", "5=v5"]);
    h.finish();
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
                // Read at least once: the writer may finish before this thread is scheduled.
                let mut reads = 0usize;
                loop {
                    let list = ownership::read_list(w.m, l).unwrap();
                    for i in 0..list.len() + 2 {
                        if let Some(v) = list.get(i) {
                            assert!(v.is_nil() || str_of(v).is_some_and(|s| s.starts_with('s')));
                        }
                    }
                    reads += 1;
                    catallaxy_gc::safepoint(&w.m.rec);
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                }
                assert!(reads > 0);
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
    stop.store(true, std::sync::atomic::Ordering::Release);
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
                // Read until the writer stops, then once more: the writer may finish before this
                // thread is first scheduled, and the final pass sees its last state.
                loop {
                    let done = stop.load(std::sync::atomic::Ordering::Acquire);
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
                    if done {
                        break;
                    }
                }
                assert!(hits > 0, "no key was ever found");
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
    stop.store(true, std::sync::atomic::Ordering::Release);
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

fn young(v: Value) -> bool {
    v.header().is_some_and(|h| h.is_young())
}

#[test]
fn minor_collection_frees_young_garbage_and_keeps_roots() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let keep = h.m.alloc_str("kept");
    let hk = h.m.root(keep);
    assert!(young(keep), "new objects start young");
    let before = catallaxy_gc::stats::minors().3;
    for i in 0..n(2000) {
        let _ = h.m.alloc_str(&format!("garbage {i}"));
        let _ = h.m.alloc_list(vec![Value::small_int(i as i64)]);
    }
    h.minor();
    assert!(catallaxy_gc::stats::minors().3 > before, "garbage was freed");
    let keep = h.m.get(hk);
    assert_eq!(str_of(keep), Some("kept"));
    assert!(!young(keep), "survivors are promoted");
    h.finish();
}

#[test]
fn root_only_large_objects_stay_young_until_the_heap_references_them() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let text = "x".repeat(4096);
    let list = h.m.alloc_list(Vec::new());
    let hl = h.m.root(list);
    h.gc(); // the list is old now
    assert!(!young(h.m.get(hl)));

    // Reachable only from a root: it survives a minor collection without being promoted.
    let s = h.m.alloc_str(&text);
    let hs = h.m.root(s);
    h.minor();
    let s = h.m.get(hs);
    assert!(young(s), "a root-only large object stays young");
    assert_eq!(str_of(s), Some(text.as_str()));

    // Stored into an old list (through the write barrier) and dropped from the roots: the next
    // minor collection finds it through the remembered set, and promotes it.
    ownership::write_list(h.m, h.m.get(hl), |l, access| unsafe { l.push(s, access) }).unwrap();
    h.m.set(hs, Value::NIL);
    let _ = h.m.alloc_str(&"garbage ".repeat(600));
    h.minor();
    let s = ownership::read_list(h.m, h.m.get(hl)).unwrap().get(0).unwrap();
    assert!(!young(s), "a heap-referenced large object is promoted");
    assert_eq!(str_of(s), Some(text.as_str()));

    // Referenced by a young object as well as a root: promoted with it.
    let s2 = h.m.alloc_str(&text);
    let pair = h.m.alloc_tuple(&[s2]);
    let hp = h.m.root(pair);
    h.m.root(s2);
    h.minor();
    let pair = h.m.get(hp);
    assert!(!young(pair) && !young(unsafe { as_tuple(pair) }.elems()[0]));

    // A root-only large object freed once the root lets go.
    let s3 = h.m.alloc_str(&text);
    let h3 = h.m.root(s3);
    h.minor();
    assert!(young(h.m.get(h3)));
    h.m.set(h3, Value::NIL);
    let before = catallaxy_gc::stats::minors().3;
    h.minor();
    assert!(catallaxy_gc::stats::minors().3 >= before + text.len(), "freed by the next minor collection");
    h.finish();
}

#[test]
fn remembered_set_keeps_young_values_in_old_containers() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let list = h.m.alloc_list(Vec::new());
    let hl = h.m.root(list);
    let d = h.m.alloc_dict(DictTable::new());
    let hd = h.m.root(d);
    h.gc(); // both containers are old now
    assert!(!young(h.m.get(hl)) && !young(h.m.get(hd)));
    for round in 0..3 {
        for i in 0..n(300) {
            let (list, d) = (h.m.get(hl), h.m.get(hd));
            let s = h.m.alloc_str(&format!("r{round}-{i}"));
            ownership::write_list(h.m, list, |l, access| unsafe { l.push(s, access) }).unwrap();
            let key = Value::small_int((round * 100_000 + i) as i64);
            let hash = ops::hash_value(&mut h, key).unwrap();
            let v = h.m.alloc_list(vec![s]);
            ownership::write_dict(h.m, d, |t, access| unsafe { t.insert_new(hash, key, v, access) }).unwrap();
            let _ = h.m.alloc_str("garbage");
        }
        h.minor();
    }
    let items = ownership::read_list(h.m, h.m.get(hl)).unwrap().to_vec();
    assert_eq!(items.len(), 3 * n(300));
    for (k, v) in items.iter().enumerate() {
        assert_eq!(str_of(*v), Some(format!("r{}-{}", k / n(300), k % n(300)).as_str()));
        assert!(!young(*v));
    }
    let entries = ownership::read_dict(h.m, h.m.get(hd), |t| t.iter().map(|e| e.value).collect::<Vec<_>>()).unwrap();
    assert_eq!(entries.len(), 3 * n(300));
    for v in entries {
        let inner = unsafe { as_list(v) }.to_vec();
        assert!(str_of(inner[0]).is_some_and(|s| s.starts_with('r')));
    }
    h.finish();
}

#[test]
fn escaping_values_are_tenured() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    // Freezing: a young list holding young (frozen-at-birth) strings.
    let s1 = h.m.alloc_str("a");
    let l1 = h.m.alloc_list(vec![s1]);
    h.m.root(l1);
    ownership::freeze_deep(h.m, l1).unwrap();
    assert!(!young(l1) && !young(s1));
    // Publishing into a shared list.
    let shared = h.m.alloc_list(Vec::new());
    h.m.root(shared);
    ownership::share_deep(h.m, shared, false).unwrap();
    assert!(!young(shared));
    let s2 = h.m.alloc_str("b");
    let l2 = h.m.alloc_list(vec![s2]);
    ownership::publish_into_shared(h.m, l2).unwrap();
    ownership::write_list(h.m, shared, |l, access| unsafe { l.push(l2, access) }).unwrap();
    assert!(!young(l2) && !young(s2));
    // An old list written since the last minor collection: its young items escape with it.
    let old = h.m.alloc_list(Vec::new());
    let ho = h.m.root(old);
    h.gc();
    let s3 = h.m.alloc_str("c");
    ownership::write_list(h.m, h.m.get(ho), |l, access| unsafe { l.push(s3, access) }).unwrap();
    ownership::transfer_deep(h.m.get(ho), h.m.tid, ownership::TRANSIT_TID);
    assert!(!young(s3));
    h.minor();
    assert_eq!(str_of(s1), Some("a"));
    assert_eq!(str_of(s2), Some("b"));
    assert_eq!(str_of(s3), Some("c"));
    h.finish();
}

#[test]
fn moved_values_survive_the_senders_minor_collection() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Host::new();
    let items: Vec<Value> = (0..n(200)).map(|i| a.m.alloc_str(&format!("m{i}"))).collect();
    let list = a.m.alloc_list(items);
    // Move it out (as a channel send does), then let the sender collect its nursery.
    ownership::transfer_deep(list, a.m.tid, ownership::TRANSIT_TID);
    let addr = list.0;
    for _ in 0..n(500) {
        let _ = a.m.alloc_str("sender garbage");
    }
    a.minor();
    let reader = std::thread::spawn(move || {
        let mut b = Host::new();
        let list = Value(addr);
        ownership::transfer_deep(list, ownership::TRANSIT_TID, b.m.tid);
        b.m.root(list);
        b.minor();
        let items = ownership::read_list(b.m, list).unwrap().to_vec();
        let ok = items.iter().enumerate().all(|(i, v)| str_of(*v) == Some(format!("m{i}").as_str()));
        b.finish();
        ok
    });
    assert!(catallaxy_gc::blocking(&a.m.rec, || reader.join().unwrap()));
    a.finish();
}
