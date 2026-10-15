//! A shared old list written and read by other threads while its owner runs minor and major
//! collections (issue #120). Split from `written_ranges.rs` so that Miri runs it on a fresh
//! heap: after the other tests there it took several times as long (see `strings.rs`, #146).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use catallaxy_runtime::object::*;
use catallaxy_runtime::ownership;
use catallaxy_runtime::Value;

mod common;
use common::*;

fn strs(v: Value) -> Vec<String> {
    unsafe { as_list(v) }.to_vec().iter().map(|&s| str_of(s).unwrap_or("<nil>").to_string()).collect()
}

/// As in `written_ranges.rs`.
fn churn(h: &mut Host) {
    for i in 0..n(400) {
        let _ = h.m.alloc_str(&format!("junk{i}"));
        let _ = h.m.alloc_list(vec![Value::small_int(i as i64)]);
    }
    h.minor();
}

fn set(h: &mut Host, l: Value, i: usize, v: Value) {
    ownership::write_list(h.m, l, |l, _| assert!(unsafe { l.set(i, v) })).unwrap();
}

/// A large list written by its owner (so remembered) is shared; other threads push and set
/// under its lock (updating its written range) and read without locking, while the first thread
/// runs minor and major collections.
#[test]
fn shared_large_list_written_and_read_concurrently_with_minor_collections() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let items: Vec<Value> = (0..n(64).max(12)).map(|i| h.m.alloc_str(&format!("s{i}"))).collect();
    let l = h.m.alloc_list(items);
    let hl = h.m.root(l);
    h.gc();
    let s = h.m.alloc_str("s-young");
    let l = h.m.get(hl);
    set(&mut h, l, 5, s);
    ownership::share_deep(h.m, h.m.get(hl), false).unwrap();
    let addr = h.m.get(hl).0;
    let stop = Arc::new(AtomicBool::new(false));
    let rounds = n(300);
    let threads: Vec<_> = (0..3)
        .map(|k| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let w = Host::new();
                let l = Value(addr);
                let mut i = 0usize;
                loop {
                    if k == 0 {
                        // Reader.
                        let list = ownership::read_list(w.m, l).unwrap();
                        for j in 0..list.len() + 2 {
                            if let Some(v) = list.get(j) {
                                assert!(str_of(v).is_some_and(|s| s.starts_with('s')));
                            }
                        }
                    } else if i < rounds {
                        let s = w.m.alloc_str(&format!("s{k}-{i}"));
                        w.m.scope(|m| {
                            // Rooted: a writer waiting for the list's lock does so in native
                            // state, where a collection can run.
                            m.root(s);
                            ownership::publish_into_shared(m, s).unwrap();
                            ownership::write_list(m, l, |list, access| unsafe {
                                if i % 3 == 0 {
                                    list.set(i % list.len(), s);
                                } else {
                                    list.push(s, access);
                                }
                            })
                            .unwrap();
                        });
                        i += 1;
                    }
                    catallaxy_gc::safepoint(&w.m.rec);
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if k == 0 || i >= rounds {
                        pause(&w.m.rec);
                    }
                    if k != 0 && i >= rounds {
                        catallaxy_gc::blocking(&w.m.rec, std::thread::yield_now);
                    }
                }
                w.finish();
            })
        })
        .collect();
    for r in 0..n(60).max(4) {
        churn(&mut h);
        if r % 10 == 0 || r == 2 {
            h.gc();
        }
    }
    stop.store(true, Ordering::Release);
    catallaxy_gc::blocking(&h.m.rec, || {
        for t in threads {
            t.join().unwrap();
        }
    });
    let all = strs(h.m.get(hl));
    assert!(all.iter().all(|s| s.starts_with('s')));
    h.gc();
    h.finish();
}
