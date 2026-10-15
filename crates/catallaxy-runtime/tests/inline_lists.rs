//! Small lists keep their elements inline in the list object and move them out of line when
//! they grow. Kept in its own test binary so that Miri runs it on a fresh heap (see `strings.rs`),
//! and because it installs a counting allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::Value;

mod common;
use common::*;

/// Counts this thread's allocations, so a test can check that building a small list does not
/// call the allocator.
struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        System.alloc_zeroed(l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        System.realloc(p, l, size)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> usize {
    ALLOCS.with(|c| c.get())
}

fn strs(v: Value) -> Vec<String> {
    unsafe { as_list(v) }.to_vec().iter().map(|&s| str_of(s).unwrap_or("<nil>").to_string()).collect()
}

fn young(v: Value) -> bool {
    v.header().is_some_and(|h| h.is_young())
}

#[test]
fn small_lists_are_inline_and_large_ones_adopt_their_vector() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let s: Vec<Value> = (0..20).map(|i| h.m.alloc_str(&format!("v{i}"))).collect();
    let l = |v: Value| unsafe { as_list(v) };

    // Empty and small lists, from vectors and slices: inline, with an odd capacity of at least 3.
    let empty = h.m.alloc_list(Vec::new());
    assert!(l(empty).is_inline() && l(empty).capacity() == ListObj::INLINE_MIN);
    let two = h.m.alloc_list(s[..2].to_vec());
    assert!(l(two).is_inline() && l(two).capacity() == 3);
    let four = h.m.alloc_list_from_slice(&s[..4], 4);
    assert!(l(four).is_inline() && l(four).capacity() == 5);
    let max = h.m.alloc_list_from_slice(&s[..ListObj::INLINE_MAX], 0);
    assert!(l(max).is_inline() && l(max).capacity() == ListObj::INLINE_MAX);
    // A few items in a vector with room for many: copied in, the vector freed.
    let mut roomy = Vec::with_capacity(64);
    roomy.extend_from_slice(&s[..3]);
    let roomy = h.m.alloc_list(roomy);
    assert!(l(roomy).is_inline() && l(roomy).capacity() == 3);
    // A slice copy asked to leave room beyond the inline maximum goes out of line.
    let spare = h.m.alloc_list_from_slice(&s[..2], ListObj::INLINE_MAX + 1);
    assert!(!l(spare).is_inline() && l(spare).capacity() == ListObj::INLINE_MAX + 1);
    // A large vector keeps its allocation and capacity (PR #76).
    let mut big = Vec::with_capacity(33);
    big.extend_from_slice(&s);
    let big = h.m.alloc_list(big);
    assert!(!l(big).is_inline() && l(big).capacity() == 33);

    let lists = [empty, two, four, max, roomy, spare, big];
    let handles: Vec<_> = lists.iter().map(|&v| h.m.root(v)).collect();
    h.minor();
    h.gc();
    let get = |h: &Host, i: usize| h.m.get(handles[i]);
    assert!(strs(get(&h, 0)).is_empty());
    assert_eq!(strs(get(&h, 1)), ["v0", "v1"]);
    assert_eq!(strs(get(&h, 2)), ["v0", "v1", "v2", "v3"]);
    assert_eq!(strs(get(&h, 3)).len(), ListObj::INLINE_MAX);
    assert_eq!(strs(get(&h, 4)), ["v0", "v1", "v2"]);
    assert_eq!(strs(get(&h, 5)), ["v0", "v1"]);
    assert_eq!(strs(get(&h, 6)).len(), 20);
    assert_eq!(strs(get(&h, 6))[19], "v19");
    h.finish();
}

#[test]
fn small_list_literals_do_not_call_the_allocator() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = Host::new();
    let items = [Value::small_int(1), Value::small_int(2), Value::small_int(3)];
    let lists = n(2000);
    // Warm up: the heap's first page for this size class.
    for _ in 0..10 {
        let _ = h.m.alloc_list_from_slice(&items, items.len());
    }
    let before = allocs();
    for _ in 0..lists {
        let _ = h.m.alloc_list_from_slice(&items, items.len());
    }
    let inline = allocs() - before;
    // Only taking a new heap page may allocate (bookkeeping, and the segment itself under Miri).
    assert!(inline * 20 < lists, "{inline} allocations for {lists} small lists");
    // Out of line, every list allocates its buffer.
    let before = allocs();
    for _ in 0..lists {
        let _ = h.m.alloc_list_from_slice(&items, ListObj::INLINE_MAX + 1);
    }
    assert!(allocs() - before >= lists);
    h.finish();
}

#[test]
fn inline_lists_spill_out_of_line_as_they_grow() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let list = h.m.alloc_list(Vec::new());
    let hl = h.m.root(list);
    let push = |h: &mut Host, i: usize| {
        let s = h.m.alloc_str(&format!("p{i}"));
        ownership::write_list(h.m, h.m.get(hl), |l, access| unsafe { l.push(s, access) }).unwrap();
    };
    for i in 0..ListObj::INLINE_MIN {
        push(&mut h, i);
    }
    let l = unsafe { as_list(h.m.get(hl)) };
    assert!(l.is_inline() && l.capacity() == ListObj::INLINE_MIN);
    push(&mut h, ListObj::INLINE_MIN);
    assert!(!l.is_inline());
    assert_eq!(l.capacity(), 8, "growth continues in powers of two");
    h.minor();
    for i in ListObj::INLINE_MIN + 1..40 {
        push(&mut h, i);
        if i % 9 == 0 {
            h.minor();
        }
    }
    h.gc();
    let want: Vec<String> = (0..40).map(|i| format!("p{i}")).collect();
    assert_eq!(strs(h.m.get(hl)), want);
    // Shrinking keeps the out-of-line buffer; the list never moves back inline.
    unsafe {
        let l = as_list(h.m.get(hl));
        let keep: Vec<Value> = l.to_vec().into_iter().take(2).collect();
        l.replace(&keep, Access::Local);
        assert!(!l.is_inline() && l.len() == 2);
        assert_eq!(l.pop().map(|v| str_of(v).unwrap().to_string()).as_deref(), Some("p1"));
    }
    // A small list replaced by more items than fit inline.
    let small = h.m.alloc_list(vec![Value::small_int(7)]);
    let hs = h.m.root(small);
    let many: Vec<Value> = (0..12).map(Value::small_int).collect();
    unsafe { as_list(small).replace(&many, Access::Local) };
    assert!(!unsafe { as_list(small) }.is_inline());
    // Lists that die inline and after spilling are both finalized by the collector.
    for i in 0..n(200) {
        let v = h.m.alloc_list(vec![Value::small_int(i as i64)]);
        if i % 2 == 0 {
            for _ in 0..10 {
                unsafe { as_list(v).push(Value::small_int(0), Access::Local) };
            }
        }
    }
    h.minor();
    h.gc();
    assert_eq!(strs(h.m.get(hl)), ["p0"]);
    assert_eq!(unsafe { as_list(h.m.get(hs)) }.to_vec(), many);
    h.finish();
}

#[test]
fn inline_lists_and_the_remembered_set() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    // Old small lists, written with young strings: once staying inline, once spilling.
    let stay = h.m.alloc_list(Vec::new());
    let hs = h.m.root(stay);
    let spill = h.m.alloc_list(Vec::new());
    let hp = h.m.root(spill);
    h.gc();
    assert!(!young(h.m.get(hs)) && !young(h.m.get(hp)));
    for round in 0..3 {
        let s = h.m.alloc_str(&format!("s{round}"));
        ownership::write_list(h.m, h.m.get(hs), |l, access| unsafe { l.push(s, access) }).unwrap();
        for i in 0..4 {
            let t = h.m.alloc_str(&format!("t{round}-{i}"));
            ownership::write_list(h.m, h.m.get(hp), |l, access| unsafe { l.push(t, access) }).unwrap();
        }
        for _ in 0..n(300) {
            let _ = h.m.alloc_str("garbage");
            let _ = h.m.alloc_list(vec![Value::small_int(1)]);
        }
        h.minor();
    }
    assert!(unsafe { as_list(h.m.get(hs)) }.is_inline());
    assert!(!unsafe { as_list(h.m.get(hp)) }.is_inline());
    assert_eq!(strs(h.m.get(hs)), ["s0", "s1", "s2"]);
    let want: Vec<String> = (0..3).flat_map(|r| (0..4).map(move |i| format!("t{r}-{i}"))).collect();
    assert_eq!(strs(h.m.get(hp)), want);
    for v in unsafe { as_list(h.m.get(hp)) }.to_vec() {
        assert!(!young(v));
    }
    h.gc();
    assert_eq!(strs(h.m.get(hs)), ["s0", "s1", "s2"]);
    h.finish();
}

#[test]
fn shared_inline_list_spills_safely_under_lock_free_readers() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let rounds = n(150);
    for round in 0..2 {
        // Starts inline; the writer pushes it out of line while readers read without locking.
        let items: Vec<Value> = (0..2).map(|i| h.m.alloc_str(&format!("s{i}"))).collect();
        let list = h.m.alloc_list(items);
        let hl = h.m.root(list);
        ownership::share_deep(h.m, list, false).unwrap();
        assert!(unsafe { as_list(list) }.is_inline());
        let addr = list.0;
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let w = Host::new();
                    let mut reads = 0usize;
                    loop {
                        let list = ownership::read_list(w.m, Value(addr)).unwrap();
                        for i in 0..list.len() + 2 {
                            if let Some(v) = list.get(i) {
                                assert!(v.is_nil() || str_of(v).is_some_and(|s| s.starts_with('s')));
                            }
                        }
                        let snap = list.to_vec();
                        assert!(snap.iter().all(|v| v.is_nil() || str_of(*v).is_some()));
                        reads += 1;
                        catallaxy_gc::safepoint(&w.m.rec);
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        pause(&w.m.rec);
                    }
                    assert!(reads > 0);
                    w.finish();
                })
            })
            .collect();
        for r in 0..rounds {
            let s = h.m.alloc_str(&format!("s{r}"));
            ownership::publish_into_shared(h.m, s).unwrap();
            ownership::write_list(h.m, h.m.get(hl), |l, access| unsafe {
                if round == 0 {
                    for _ in 0..3 {
                        l.push(s, access);
                    }
                } else if r == rounds / 2 {
                    l.push(s, access);
                }
                if r % 4 == 0 {
                    l.pop();
                }
                if round == 1 && r % 7 == 0 {
                    l.set(0, s);
                }
            })
            .unwrap();
            if r % 25 == 0 {
                h.gc();
            }
        }
        stop.store(true, Ordering::Release);
        catallaxy_gc::blocking(&h.m.rec, || {
            for t in readers {
                t.join().unwrap();
            }
        });
        let l = unsafe { as_list(h.m.get(hl)) };
        // Round 0 grew far past the inline slots; round 1 stayed (mostly) small.
        assert_eq!(round == 0, !l.is_inline() && l.len() > ListObj::INLINE_MAX);
        h.gc();
        h.m.set(hl, Value::NIL);
    }
    h.gc();
    h.finish();
}

#[test]
fn element_tags_survive_growth_and_leave_capacities_alone() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let l = |v: Value| unsafe { as_list(v) };
    let list = h.m.alloc_list(Vec::new());
    assert_eq!(l(list).elem_tag(), 0);
    // Only an untagged list takes a tag; the capacity reads as before.
    assert!(l(list).set_elem_tag(0, 7));
    assert!(!l(list).set_elem_tag(0, 9));
    assert_eq!((l(list).elem_tag(), l(list).capacity()), (7, ListObj::INLINE_MIN));
    // Growing out of line and collecting keep the tag (it lives in the object, not the buffer).
    let hl = h.m.root(list);
    for i in 0..(ListObj::INLINE_MAX + 2) as i64 {
        let v = h.m.get(hl);
        unsafe { as_list(v).push(Value::small_int(i), Access::Local) };
    }
    h.minor();
    h.gc();
    let list = h.m.get(hl);
    assert!(!l(list).is_inline() && l(list).len() == ListObj::INLINE_MAX + 2);
    assert_eq!(l(list).elem_tag(), 7);
    assert!(l(list).set_elem_tag(7, 8) && l(list).elem_tag() == 8);
    // Dicts and sets keep a key and a value tag.
    let d = h.m.alloc_dict(DictTable::new());
    let dict = unsafe { as_dict(d) };
    assert_eq!(dict.tags(), (0, 0));
    assert!(dict.set_tags((0, 0), (3, 4)) && !dict.set_tags((0, 0), (5, 6)));
    assert_eq!(dict.tags(), (3, 4));
    h.finish();
}
