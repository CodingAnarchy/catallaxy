//! Exhaustive interleaving tests of the per-object lock and mark protocols.
//! Run with: RUSTFLAGS="--cfg loom" cargo test -p catallaxy-gc --test loom --release

#![cfg(loom)]

use catallaxy_gc::{GcVTable, Header, Tracer};
use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;

unsafe fn no_trace(_: *const Header, _: &mut Tracer) {}
unsafe fn no_size(_: *const Header) -> usize {
    16
}
static VT: GcVTable = GcVTable { trace: no_trace, finalize: None, size: no_size };

struct Obj {
    header: Header,
    data: UnsafeCell<u64>,
}
unsafe impl Sync for Obj {}
unsafe impl Send for Obj {}

fn new_obj() -> Arc<Obj> {
    let mut o = std::mem::MaybeUninit::<Header>::uninit();
    unsafe { Header::init(o.as_mut_ptr(), &VT, 1, 1, false) };
    Arc::new(Obj { header: unsafe { o.assume_init() }, data: UnsafeCell::new(0) })
}

#[test]
fn lock_gives_mutual_exclusion() {
    loom::model(|| {
        let o = new_obj();
        let hs: Vec<_> = (0..2)
            .map(|i| {
                let o = o.clone();
                thread::spawn(move || {
                    o.header.lock(10 + i);
                    o.data.with_mut(|p| unsafe { *p += 1 });
                    o.header.unlock();
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(o.data.with(|p| unsafe { *p }), 2);
        assert!(!o.header.is_locked());
    });
}

#[test]
fn reentrant_lock_releases_fully() {
    loom::model(|| {
        let o = new_obj();
        let o2 = o.clone();
        let t = thread::spawn(move || {
            o2.header.lock(3);
            assert!(o2.header.try_lock(3));
            o2.data.with_mut(|p| unsafe { *p += 1 });
            o2.header.unlock();
            o2.header.unlock();
        });
        o.header.lock(4);
        o.data.with_mut(|p| unsafe { *p += 1 });
        o.header.unlock();
        t.join().unwrap();
        assert_eq!(o.data.with(|p| unsafe { *p }), 2);
        assert!(!o.header.is_locked());
    });
}

#[test]
fn exactly_one_marker_wins() {
    loom::model(|| {
        let o = new_obj();
        let wins: Vec<_> = (0..2)
            .map(|_| {
                let o = o.clone();
                thread::spawn(move || o.header.try_mark(2))
            })
            .collect();
        let n = wins.into_iter().map(|h| h.join().unwrap() as u32).sum::<u32>();
        assert_eq!(n, 1);
    });
}

#[test]
fn marking_does_not_lose_lock_bits() {
    loom::model(|| {
        let o = new_obj();
        let o2 = o.clone();
        let t = thread::spawn(move || {
            o2.header.lock(5);
            o2.header.unlock();
        });
        o.header.try_mark(2);
        t.join().unwrap();
        assert!(!o.header.is_locked());
        assert!(!o.header.try_mark(2), "mark survived concurrent lock traffic");
    });
}
