//! Minor collections scan only the written slots of old lists and dicts (issue #120). Each test
//! writes young objects into old containers in a way that moves, copies or replaces their
//! buffers, runs minor collections, then allocates enough to reuse any slot a collection wrongly
//! freed, and checks the contents. Its own test binary, so that Miri runs it on a fresh heap
//! (see `strings.rs`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::{ops, Value};

mod common;
use common::*;

fn young(v: Value) -> bool {
    v.header().is_some_and(|h| h.is_young())
}

fn strs(v: Value) -> Vec<String> {
    unsafe { as_list(v) }.to_vec().iter().map(|&s| str_of(s).unwrap_or("<nil>").to_string()).collect()
}

/// Allocates strings and small lists until the slots of anything a minor collection freed have
/// been handed out again, then collects that garbage.
fn churn(h: &mut Host) {
    for i in 0..n(400) {
        let _ = h.m.alloc_str(&format!("junk{i}"));
        let _ = h.m.alloc_list(vec![Value::small_int(i as i64)]);
    }
    h.minor();
}

fn push(h: &mut Host, l: Value, v: Value) {
    ownership::write_list(h.m, l, |l, access| unsafe { l.push(v, access) }).unwrap();
}

fn set(h: &mut Host, l: Value, i: usize, v: Value) {
    ownership::write_list(h.m, l, |l, _| assert!(unsafe { l.set(i, v) })).unwrap();
}

fn list<'a>(v: Value) -> &'a ListObj {
    unsafe { as_list(v) }
}

#[test]
fn minor_collections_scan_the_written_range_of_old_lists() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let len = n(500).max(64);
    let items: Vec<Value> = (0..len).map(|i| h.m.alloc_str(&format!("old{i}"))).collect();
    let l = h.m.alloc_list(items);
    let hl = h.m.root(l);
    h.gc();
    let l = h.m.get(hl);
    assert!(!young(l) && !list(l).is_inline());
    // A new buffer counts as written throughout; the first minor collection that scans the
    // list (it is remembered once written) clears that.
    assert_eq!(list(l).written_range(), 0..list(l).capacity());
    let s = h.m.alloc_str("first");
    set(&mut h, l, 3, s);
    h.minor();
    assert_eq!(list(l).written_range(), 0..0);
    let mut want: Vec<String> = (0..len).map(|i| format!("old{i}")).collect();
    want[3] = "first".into();

    for round in 0..3 {
        // Scattered sets and pushes; only their span is recorded.
        let (a, b) = (5 + round, len - 7 - round);
        let sa = h.m.alloc_str(&format!("a{round}"));
        set(&mut h, l, a, sa);
        let sb = h.m.alloc_str(&format!("b{round}"));
        set(&mut h, l, b, sb);
        want[a] = format!("a{round}");
        want[b] = format!("b{round}");
        assert_eq!(list(l).written_range(), a..b + 1);
        for i in 0..3 {
            let s = h.m.alloc_str(&format!("p{round}-{i}"));
            push(&mut h, l, s);
            want.push(format!("p{round}-{i}"));
        }
        assert_eq!(list(l).written_range(), a..want.len());
        assert!(young(list(l).get(a).unwrap()));
        churn(&mut h);
        assert_eq!(list(l).written_range(), 0..0);
        assert!(!young(list(l).get(a).unwrap()) && !young(list(l).get(want.len() - 1).unwrap()));
        assert_eq!(strs(l), want);
    }
    h.gc();
    assert_eq!(strs(h.m.get(hl)), want);
    h.finish();
}

#[test]
fn moved_and_replaced_list_buffers_keep_young_items_reachable() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    // An inline list that spills, and an out-of-line one that grows several times.
    let inl = h.m.alloc_list(Vec::new());
    let hi = h.m.root(inl);
    let start: Vec<Value> = (0..16).map(|i| h.m.alloc_str(&format!("o{i}"))).collect();
    let big = h.m.alloc_list(start);
    let hb = h.m.root(big);
    h.gc();
    let (inl, big) = (h.m.get(hi), h.m.get(hb));
    let mut want_i = Vec::new();
    let mut want_b: Vec<String> = (0..16).map(|i| format!("o{i}")).collect();
    // Scan once so the big list's range is empty, then write one young item near the end.
    let s = h.m.alloc_str("tail");
    push(&mut h, big, s);
    want_b.push("tail".into());
    h.minor();
    assert_eq!(list(big).written_range(), 0..0);
    for i in 0..n(200).max(40) {
        let s = h.m.alloc_str(&format!("i{i}"));
        push(&mut h, inl, s);
        want_i.push(format!("i{i}"));
        let s = h.m.alloc_str(&format!("b{i}"));
        push(&mut h, big, s);
        want_b.push(format!("b{i}"));
    }
    // Growth kept the range: it starts at the first push since the scan, not at 0.
    assert_eq!(list(big).written_range(), 17..want_b.len());
    assert!(!list(inl).is_inline());
    churn(&mut h);
    assert_eq!(strs(inl), want_i);
    assert_eq!(strs(big), want_b);

    // Young items moved to other indices in place (reverse, then rotate by one), and a replace
    // that needs a larger buffer.
    let s = h.m.alloc_str("moved");
    set(&mut h, big, 0, s);
    want_b[0] = "moved".into();
    h.minor();
    let s = h.m.alloc_str("young-front");
    set(&mut h, big, 0, s);
    want_b[0] = "young-front".into();
    ownership::write_list(h.m, big, |l, access| unsafe {
        l.modify(access, |v| {
            v.reverse();
            v.rotate_left(1)
        })
    })
    .unwrap();
    want_b.reverse();
    want_b.rotate_left(1);
    assert_eq!(list(big).written_range(), 0..want_b.len());
    churn(&mut h);
    assert_eq!(strs(big), want_b);

    let cap = list(inl).capacity();
    let mut grown = Vec::new();
    for i in 0..cap + 5 {
        grown.push(h.m.alloc_str(&format!("r{i}")));
    }
    ownership::write_list(h.m, inl, |l, access| unsafe { l.replace(&grown, access) }).unwrap();
    churn(&mut h);
    assert_eq!(strs(inl), (0..cap + 5).map(|i| format!("r{i}")).collect::<Vec<_>>());
    // A replace in place, with fewer items.
    let few: Vec<Value> = (0..3).map(|i| h.m.alloc_str(&format!("f{i}"))).collect();
    ownership::write_list(h.m, inl, |l, access| unsafe { l.replace(&few, access) }).unwrap();
    churn(&mut h);
    assert_eq!(strs(inl), ["f0", "f1", "f2"]);
    h.gc();
    assert_eq!(strs(h.m.get(hb)), want_b);
    h.finish();
}

#[test]
fn minor_collections_scan_the_written_entries_of_old_dicts() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let d = h.m.alloc_dict(DictTable::new());
    let hd = h.m.root(d);
    let keys = n(400).max(40);
    let insert = |h: &mut Host, d: Value, k: i64, v: Value| {
        let key = Value::small_int(k);
        let hash = ops::hash_value(h, key).unwrap();
        ownership::write_dict(h.m, d, |t, access| unsafe { t.insert_new(hash, key, v, access) }).unwrap();
    };
    for k in 0..keys {
        let v = h.m.alloc_str(&format!("v{k}"));
        insert(&mut h, d, k as i64, v);
    }
    h.gc();
    let d = h.m.get(hd);
    let s = h.m.alloc_str("first");
    insert(&mut h, d, keys as i64, s);
    h.minor();
    assert_eq!(unsafe { as_dict(d) }.table().written_range(), 0..0);
    let value_of = |h: &mut Host, d: Value, k: i64| {
        let key = Value::small_int(k);
        let ix = ops::dict_lookup(h, d, key).unwrap().expect("key present");
        str_of(unsafe { as_dict(d) }.table().entry(ix).value).unwrap().to_string()
    };
    // Overwrite one value in the middle, then insert until the table is rebuilt.
    let mid = keys as i64 / 2;
    let key = Value::small_int(mid);
    let ix = ops::dict_lookup(&mut h, d, key).unwrap().unwrap();
    let s = h.m.alloc_str("middle");
    ownership::write_dict(h.m, d, |t, access| unsafe { t.set_value_at(ix, s, access) }).unwrap();
    assert_eq!(unsafe { as_dict(d) }.table().written_range(), ix * 3 + 2..ix * 3 + 3);
    churn(&mut h);
    assert_eq!(value_of(&mut h, d, mid), "middle");
    let extra = keys * 2;
    for k in keys + 1..extra {
        let v = h.m.alloc_str(&format!("v{k}"));
        insert(&mut h, d, k as i64, v);
        if k % 97 == 0 {
            churn(&mut h);
        }
    }
    churn(&mut h);
    for k in (0..extra).filter(|&k| k as i64 != mid && k != keys) {
        assert_eq!(value_of(&mut h, d, k as i64), format!("v{k}"));
    }
    assert_eq!(value_of(&mut h, d, keys as i64), "first");

    // A `Dyn` record (shape mode) with string keys, growing its value buffer.
    let r = h.m.alloc_dict(DictTable::record(2));
    let hr = h.m.root(r);
    h.gc();
    let r = h.m.get(hr);
    for i in 0..12 {
        let key = h.m.alloc_str(&format!("k{i}"));
        let hash = ops::hash_value(&mut h, key).unwrap();
        let v = h.m.alloc_str(&format!("rv{i}"));
        ownership::write_dict(h.m, r, |t, access| unsafe { t.insert_new(hash, key, v, access) }).unwrap();
        if i % 5 == 4 {
            churn(&mut h);
        }
    }
    churn(&mut h);
    let vals: Vec<String> =
        unsafe { as_dict(r) }.table().iter().map(|e| str_of(e.value).unwrap().to_string()).collect();
    assert_eq!(vals, (0..12).map(|i| format!("rv{i}")).collect::<Vec<_>>());
    h.gc();
    let d = h.m.get(hd);
    assert_eq!(value_of(&mut h, d, mid), "middle");
    h.finish();
}

/// An old list this thread wrote (so it is in this thread's remembered set) moves to another
/// thread, which writes its own young objects into it. The receiver must remember it itself,
/// and this thread's minor collections must leave it alone.
#[test]
fn a_remembered_list_handed_to_another_thread_is_remembered_by_the_receiver() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let items: Vec<Value> = (0..20).map(|i| h.m.alloc_str(&format!("o{i}"))).collect();
    let l = h.m.alloc_list(items);
    let hl = h.m.root(l);
    h.gc();
    let s = h.m.alloc_str("mine");
    let l = h.m.get(hl);
    set(&mut h, l, 0, s);
    assert!(l.header().unwrap().is_remembered());
    // Hand it over as a channel does: the sender moves it into transit, the receiver takes it.
    ownership::transfer_deep(l, h.m.tid, ownership::TRANSIT_TID);
    let addr = l.0;
    let go = Arc::new(AtomicBool::new(false));
    let go2 = go.clone();
    let t = std::thread::spawn(move || {
        let mut w = Host::new();
        let l = Value(addr);
        ownership::transfer_deep(l, ownership::TRANSIT_TID, w.m.tid);
        let hl = w.m.root(l);
        go2.store(true, Ordering::Release);
        for round in 0..4 {
            let s = w.m.alloc_str(&format!("theirs{round}"));
            let l = w.m.get(hl);
            set(&mut w, l, 1 + round, s);
            churn(&mut w);
        }
        let got = strs(w.m.get(hl));
        assert_eq!(got[0], "mine");
        for round in 0..4 {
            assert_eq!(got[1 + round], format!("theirs{round}"));
        }
        w.finish();
    });
    while !go.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    // This thread's minor collections run while the list belongs to the other thread.
    h.m.set(hl, Value::NIL);
    for _ in 0..4 {
        churn(&mut h);
    }
    catallaxy_gc::blocking(&h.m.rec, || t.join().unwrap());
    h.finish();
}

#[test]
fn young_lists_record_nothing_new() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = Host::new();
    // A young out-of-line list's range already covers every slot; writes leave it as it is.
    let items: Vec<Value> = (0..20).map(Value::small_int).collect();
    let l = h.m.alloc_list(items);
    assert!(young(l));
    assert_eq!(list(l).written_range(), 0..list(l).capacity());
    assert!(unsafe { list(l).set(4, Value::small_int(9)) });
    unsafe { list(l).push(Value::small_int(9), Access::Local) };
    assert_eq!(list(l).written_range(), 0..list(l).capacity());
    h.finish();
}

#[test]
fn storing_numbers_and_old_objects_records_nothing() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let items: Vec<Value> = (0..20).map(|i| h.m.alloc_str(&format!("o{i}"))).collect();
    let l = h.m.alloc_list(items);
    let hl = h.m.root(l);
    let d = h.m.alloc_dict(DictTable::new());
    let hd = h.m.root(d);
    h.gc();
    let (l, d) = (h.m.get(hl), h.m.get(hd));
    let old = list(l).get(1).unwrap();
    assert!(!young(old));
    // Scan both once, so that their ranges are empty.
    let s = h.m.alloc_str("young");
    set(&mut h, l, 0, s);
    let key = Value::small_int(1);
    let hash = ops::hash_value(&mut h, key).unwrap();
    ownership::write_dict(h.m, d, |t, access| unsafe { t.insert_new(hash, key, s, access) }).unwrap();
    h.minor();
    assert_eq!(list(l).written_range(), 0..0);
    assert_eq!(unsafe { as_dict(d) }.table().written_range(), 0..0);
    set(&mut h, l, 2, Value::small_int(5));
    set(&mut h, l, 3, old);
    push(&mut h, l, Value::NIL);
    assert_eq!(list(l).written_range(), 0..0);
    let key = Value::small_int(2);
    let hash = ops::hash_value(&mut h, key).unwrap();
    ownership::write_dict(h.m, d, |t, access| unsafe { t.insert_new(hash, key, old, access) }).unwrap();
    assert_eq!(unsafe { as_dict(d) }.table().written_range(), 0..0);
    // A young one is recorded.
    let s = h.m.alloc_str("young2");
    set(&mut h, l, 7, s);
    assert_eq!(list(l).written_range(), 7..8);
    h.finish();
}
