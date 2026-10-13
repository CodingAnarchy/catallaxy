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

/// The safepoint poll handshake (collector.rs): a requester sets the global request and then
/// raises each thread's flag; a thread entering its safepoint lowers its own flag and then reads
/// the request. Whatever the interleaving, the thread either sees the request now or finds its
/// flag raised for the next poll: a major request is never lost.
#[test]
fn poll_requests_are_never_lost() {
    // Store-buffering shape: it needs SeqCst fences (loom models SeqCst accesses only as
    // acquire/release, but models SeqCst fences exactly), as collector.rs has.
    use loom::sync::atomic::{fence, AtomicBool, Ordering::SeqCst};
    loom::model(|| {
        let requested = Arc::new(AtomicBool::new(false));
        let poll = Arc::new(AtomicBool::new(true)); // a minor collection was wanted
        let (r, p) = (requested.clone(), poll.clone());
        let requester = thread::spawn(move || {
            if !r.swap(true, SeqCst) {
                fence(SeqCst);
                p.store(true, SeqCst);
            }
        });
        poll.store(false, SeqCst);
        fence(SeqCst);
        let saw = requested.load(SeqCst);
        requester.join().unwrap();
        assert!(saw || poll.load(SeqCst), "request lost");
    });
}

/// Tenuring (clearing the young bit) racing with another update of the same header word keeps
/// both: generation bits share the word with the identity hash and the lock.
#[test]
fn tenure_and_hash_do_not_lose_updates() {
    loom::model(|| {
        let mut o = std::mem::MaybeUninit::<Header>::uninit();
        unsafe { Header::init(o.as_mut_ptr(), &VT, 1, 1, true) };
        let o = Arc::new(Obj { header: unsafe { o.assume_init() }, data: UnsafeCell::new(0) });
        let o2 = o.clone();
        let t = thread::spawn(move || o2.header.identity_hash());
        o.header.tenure();
        let h = t.join().unwrap();
        assert!(!o.header.is_young());
        assert_eq!(o.header.identity_hash(), h);
    });
}
