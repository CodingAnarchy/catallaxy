//! Marking with a mark stack that cannot grow (issue #139): the collector leaves objects
//! unmarked, then rescans the roots, the remembered set and every marked object until it has
//! marked everything live. The stack is capped at a few entries here so that this happens on
//! every collection. Its own test binary, so that Miri runs it on a fresh heap.

use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ownership;
use catallaxy_runtime::{ops, Value};

mod common;
use common::*;

fn young(v: Value) -> bool {
    v.header().is_some_and(|h| h.is_young())
}

/// Resets the cap even if the test fails.
struct Cap;
impl Drop for Cap {
    fn drop(&mut self) {
        catallaxy_gc::set_mark_stack_limit(0);
    }
}

/// `[["s<i>"], ...]` as strings.
fn nested(v: Value) -> Vec<String> {
    unsafe { as_list(v) }
        .to_vec()
        .iter()
        .map(|&l| str_of(unsafe { as_list(l) }.get(0).unwrap()).unwrap().to_string())
        .collect()
}

fn churn(h: &mut Host) {
    for i in 0..n(400) {
        let _ = h.m.alloc_str(&format!("junk{i}"));
        let _ = h.m.alloc_list(vec![Value::small_int(i as i64), Value::NIL]);
    }
}

#[test]
fn collections_rescan_when_the_mark_stack_cannot_grow() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let _cap = Cap;
    catallaxy_gc::set_mark_stack_limit(4);
    let k = n(300).max(20);
    // A young list of young lists (containers, so each is pushed), reached from a root.
    let items: Vec<Value> = (0..k)
        .map(|i| {
            let s = h.m.alloc_str(&format!("y{i}"));
            h.m.alloc_list(vec![s, Value::NIL])
        })
        .collect();
    let y = h.m.alloc_list(items);
    let hy = h.m.root(y);
    let before = catallaxy_gc::stats::mark_rescans();
    h.minor();
    assert!(catallaxy_gc::stats::mark_rescans() > before);
    let want: Vec<String> = (0..k).map(|i| format!("y{i}")).collect();
    churn(&mut h);
    h.minor();
    assert_eq!(nested(h.m.get(hy)), want);

    // Old containers written with young containers: found through the remembered set, whose
    // written-range scan runs once; the rescan traces them in full.
    let l = h.m.alloc_list(Vec::new());
    let hl = h.m.root(l);
    let d = h.m.alloc_dict(DictTable::new());
    let hd = h.m.root(d);
    h.gc();
    for i in 0..k {
        let s = h.m.alloc_str(&format!("o{i}"));
        let inner = h.m.alloc_list(vec![s, Value::NIL]);
        ownership::write_list(h.m, h.m.get(hl), |l, access| unsafe { l.push(inner, access) }).unwrap();
        let key = Value::small_int(i as i64);
        let hash = ops::hash_value(&mut h, key).unwrap();
        let inner = h.m.alloc_list(vec![s, Value::NIL]);
        ownership::write_dict(h.m, h.m.get(hd), |t, access| unsafe { t.insert_new(hash, key, inner, access) }).unwrap();
    }
    h.minor();
    churn(&mut h);
    h.minor();
    let want: Vec<String> = (0..k).map(|i| format!("o{i}")).collect();
    let l = h.m.get(hl);
    assert_eq!(nested(l), want);
    assert!(unsafe { as_list(l) }.to_vec().iter().all(|&v| !young(v)));
    let vals: Vec<String> = ownership::read_dict(h.m, h.m.get(hd), |t| {
        t.iter().map(|e| str_of(unsafe { as_list(e.value) }.get(0).unwrap()).unwrap().to_string()).collect()
    })
    .unwrap();
    assert_eq!(vals, want);

    // Major collections, with garbage between them that reuses wrongly freed slots.
    for _ in 0..2 {
        churn(&mut h);
        h.gc();
    }
    churn(&mut h);
    assert_eq!(nested(h.m.get(hl)), want);
    assert_eq!(nested(h.m.get(hy)).len(), k);
    h.finish();
}
