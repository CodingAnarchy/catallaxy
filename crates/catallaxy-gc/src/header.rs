//! The 16-byte object header shared by every heap object.
//!
//! Word 0 points at the object's [`GcVTable`] (in practice the first field of a runtime class
//! descriptor). It is written once at allocation and never again, so readers need no
//! synchronization beyond the publication of the object itself. A zero word 0 marks a free slot.
//!
//! Word 1 packs the mutable metadata:
//!
//! | bits   | field |
//! |--------|-------|
//! | 0..2   | ownership state: owned / frozen / shared |
//! | 2..4   | GC mark color |
//! | 4      | lock held |
//! | 5      | lock has parked waiters |
//! | 6..16  | lock recursion depth |
//! | 16..32 | owner thread id when owned; lock holder when shared and locked |
//! | 32..61 | identity hash, lazily assigned (0 = unassigned) |
//! | 61     | remembered: an old object in its owner's remembered set |
//! | 62     | young: allocated since its owner's last minor collection |
//! | 63     | minor mark |
//!
//! A young object is always owned by the thread that allocated it (or frozen and reachable only
//! from that thread's objects), so that thread can collect it without stopping anyone. Every
//! path that lets another thread or a global reach an object tenures it first (clears `young`).

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::Tracer;

/// Per-type GC hooks. Runtime class descriptors embed this as their first field.
#[repr(C)]
pub struct GcVTable {
    /// Visits every heap reference held by the object.
    pub trace: unsafe fn(obj: *const Header, tracer: &mut Tracer),
    /// Releases Rust-side resources (vector buffers, big integers) when the object dies.
    pub finalize: Option<unsafe fn(obj: *mut Header)>,
    /// Bytes retained by the object, including Rust-side buffers (for heap growth decisions).
    pub size: unsafe fn(obj: *const Header) -> usize,
}

#[repr(C)]
pub struct Header {
    vtable: AtomicUsize,
    meta: AtomicU64,
}

pub const HEADER_SIZE: usize = std::mem::size_of::<Header>();

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjState {
    Owned = 0,
    Frozen = 1,
    Shared = 2,
}

const STATE_MASK: u64 = 0b11;
const COLOR_SHIFT: u32 = 2;
const COLOR_MASK: u64 = 0b11 << COLOR_SHIFT;
pub(crate) const LOCK_BIT: u64 = 1 << 4;
pub(crate) const PARKED_BIT: u64 = 1 << 5;
const DEPTH_SHIFT: u32 = 6;
const DEPTH_MASK: u64 = 0x3ff << DEPTH_SHIFT;
const OWNER_SHIFT: u32 = 16;
const OWNER_MASK: u64 = 0xffff << OWNER_SHIFT;
const HASH_SHIFT: u32 = 32;
const HASH_MASK: u64 = (1 << 29) - 1;
pub(crate) const REMEMBERED_BIT: u64 = 1 << 61;
pub(crate) const YOUNG_BIT: u64 = 1 << 62;
pub(crate) const MINOR_MARK_BIT: u64 = 1 << 63;
const GEN_BITS: u64 = REMEMBERED_BIT | YOUNG_BIT | MINOR_MARK_BIT;

impl Header {
    /// Initializes a header in freshly allocated memory.
    ///
    /// # Safety
    /// `this` must point to writable memory of at least `HEADER_SIZE` bytes.
    pub unsafe fn init(this: *mut Header, vtable: *const GcVTable, owner: u16, color: u8, young: bool) {
        let meta = (ObjState::Owned as u64)
            | ((color as u64) << COLOR_SHIFT)
            | ((owner as u64) << OWNER_SHIFT)
            | if young { YOUNG_BIT } else { 0 };
        std::ptr::write(this, Header { vtable: AtomicUsize::new(vtable as usize), meta: AtomicU64::new(meta) });
    }

    #[inline(always)]
    pub fn vtable(&self) -> *const GcVTable {
        self.vtable.load(Ordering::Relaxed) as *const GcVTable
    }

    #[inline(always)]
    pub(crate) fn is_free(&self) -> bool {
        self.vtable.load(Ordering::Relaxed) == 0
    }

    #[inline(always)]
    pub(crate) fn clear_vtable(&self) {
        self.vtable.store(0, Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn meta(&self) -> u64 {
        self.meta.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn state(&self) -> ObjState {
        Self::state_of(self.meta.load(Ordering::Acquire))
    }

    #[inline(always)]
    pub fn state_of(meta: u64) -> ObjState {
        match meta & STATE_MASK {
            0 => ObjState::Owned,
            1 => ObjState::Frozen,
            _ => ObjState::Shared,
        }
    }

    #[inline(always)]
    pub fn owner_of(meta: u64) -> u16 {
        ((meta & OWNER_MASK) >> OWNER_SHIFT) as u16
    }

    #[inline(always)]
    pub fn owner(&self) -> u16 {
        Self::owner_of(self.meta.load(Ordering::Relaxed))
    }

    /// True when the object is owned by `tid` or frozen: the cases that need no synchronization.
    #[inline(always)]
    pub fn is_local_or_frozen(&self, tid: u16) -> bool {
        let m = self.meta.load(Ordering::Relaxed);
        let st = m & STATE_MASK;
        st == ObjState::Frozen as u64 || (st == ObjState::Owned as u64 && Self::owner_of(m) == tid)
    }

    fn update(&self, f: impl Fn(u64) -> u64) -> u64 {
        let mut cur = self.meta.load(Ordering::Relaxed);
        loop {
            let new = f(cur);
            match self.meta.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return new,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Changes the ownership state. A shared object is visible to every thread, so it is
    /// tenured in the same step.
    pub fn set_state(&self, state: ObjState) {
        let clear = if state == ObjState::Shared { STATE_MASK | YOUNG_BIT | MINOR_MARK_BIT } else { STATE_MASK };
        self.update(|m| (m & !clear) | state as u64);
    }

    // ---- generations ----

    #[inline(always)]
    pub fn is_remembered(&self) -> bool {
        self.meta.load(Ordering::Relaxed) & REMEMBERED_BIT != 0
    }

    #[inline(always)]
    pub fn is_young(&self) -> bool {
        self.meta.load(Ordering::Relaxed) & YOUNG_BIT != 0
    }

    /// True if a write into an object with this metadata must add it to the remembered set:
    /// it is old and not remembered yet.
    #[inline(always)]
    pub fn needs_remember(meta: u64) -> bool {
        meta & (YOUNG_BIT | REMEMBERED_BIT) == 0
    }

    /// Makes the object old. Idempotent.
    #[inline]
    pub fn tenure(&self) {
        if self.meta.load(Ordering::Relaxed) & (YOUNG_BIT | MINOR_MARK_BIT) != 0 {
            self.meta.fetch_and(!(YOUNG_BIT | MINOR_MARK_BIT), Ordering::AcqRel);
        }
    }

    /// Sets the remembered bit; true if this call set it.
    #[inline]
    pub(crate) fn set_remembered(&self) -> bool {
        self.meta.fetch_or(REMEMBERED_BIT, Ordering::AcqRel) & REMEMBERED_BIT == 0
    }

    #[inline]
    pub(crate) fn clear_remembered(&self) {
        self.meta.fetch_and(!REMEMBERED_BIT, Ordering::AcqRel);
    }

    #[inline]
    pub(crate) fn meta_relaxed(&self) -> u64 {
        self.meta.load(Ordering::Relaxed)
    }

    /// Sets the minor mark; true if this call set it.
    #[inline]
    pub(crate) fn try_minor_mark(&self) -> bool {
        let m = self.meta.load(Ordering::Relaxed);
        if m & MINOR_MARK_BIT != 0 {
            return false;
        }
        self.meta.fetch_or(MINOR_MARK_BIT, Ordering::AcqRel) & MINOR_MARK_BIT == 0
    }

    pub fn set_owner(&self, tid: u16) {
        self.update(|m| (m & !OWNER_MASK) | ((tid as u64) << OWNER_SHIFT));
    }

    /// Sets the state to owned by `tid` in one atomic step.
    pub fn set_owned_by(&self, tid: u16) {
        self.update(|m| (m & !(OWNER_MASK | STATE_MASK)) | ((tid as u64) << OWNER_SHIFT));
    }

    #[inline(always)]
    pub(crate) fn color(&self) -> u8 {
        ((self.meta.load(Ordering::Relaxed) & COLOR_MASK) >> COLOR_SHIFT) as u8
    }

    /// Sets the mark color. Returns true if this call changed it (the caller then owns tracing).
    #[inline]
    #[doc(hidden)]
    pub fn try_mark(&self, color: u8) -> bool {
        let want = (color as u64) << COLOR_SHIFT;
        let mut cur = self.meta.load(Ordering::Relaxed);
        loop {
            if cur & COLOR_MASK == want {
                return false;
            }
            // A major collection makes every survivor old and empties remembered sets.
            let new = (cur & !(COLOR_MASK | GEN_BITS)) | want;
            match self.meta.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Returns the identity hash, assigning one on first use.
    pub fn identity_hash(&self) -> u32 {
        let m = self.meta.load(Ordering::Relaxed);
        let h = ((m >> HASH_SHIFT) & HASH_MASK) as u32;
        if h != 0 {
            return h;
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x9e37_79b9);
        let mut fresh = ((NEXT.fetch_add(0x6c07_8965, std::sync::atomic::Ordering::Relaxed) as u32)
            ^ ((self as *const _ as u64 >> 4) as u32))
            & HASH_MASK as u32;
        if fresh == 0 {
            fresh = 1;
        }
        let new = self.update(|m| {
            if (m >> HASH_SHIFT) & HASH_MASK != 0 {
                m
            } else {
                (m & !(HASH_MASK << HASH_SHIFT)) | ((fresh as u64) << HASH_SHIFT)
            }
        });
        ((new >> HASH_SHIFT) & HASH_MASK) as u32
    }

    // ---- per-object lock (spin, then park) ----
    //
    // Only shared objects are locked. Their owner bits are otherwise unused, so they hold the
    // lock holder's thread id, which makes the lock reentrant (bits 6..16 count recursion).

    /// Tries to take the lock for thread `tid` (reentrant). Never blocks.
    #[inline]
    pub fn try_lock(&self, tid: u16) -> bool {
        let cur = self.meta.load(Ordering::Relaxed);
        if cur & LOCK_BIT == 0 {
            let new = (cur & !(OWNER_MASK | DEPTH_MASK)) | LOCK_BIT | ((tid as u64) << OWNER_SHIFT);
            return self.meta.compare_exchange(cur, new, Ordering::Acquire, Ordering::Relaxed).is_ok();
        }
        if Self::owner_of(cur) == tid {
            let depth = (cur & DEPTH_MASK) >> DEPTH_SHIFT;
            assert!(depth + 1 < (DEPTH_MASK >> DEPTH_SHIFT), "object lock recursion too deep");
            // Only the holder modifies the depth; other bits may change concurrently (parked bit).
            self.meta.fetch_add(1 << DEPTH_SHIFT, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Acquires the object's lock for `tid`, spinning briefly and then parking. Callers that may
    /// block while the holder runs safepoints must enter native state first (see the runtime).
    pub fn lock(&self, tid: u16) {
        if self.try_lock(tid) {
            return;
        }
        self.lock_slow(tid);
    }

    #[cold]
    fn lock_slow(&self, tid: u16) {
        let mut spins = 0u32;
        loop {
            if self.try_lock(tid) {
                return;
            }
            let cur = self.meta.load(Ordering::Relaxed);
            if cur & LOCK_BIT == 0 {
                continue;
            }
            #[cfg(loom)]
            {
                let _ = spins;
                loom::thread::yield_now();
                continue;
            }
            #[cfg(not(loom))]
            if spins < 64 {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            if cur & PARKED_BIT == 0
                && self.meta.compare_exchange_weak(cur, cur | PARKED_BIT, Ordering::Relaxed, Ordering::Relaxed).is_err()
            {
                continue;
            }
            let key = self as *const _ as usize;
            // SAFETY: the validate closure re-checks the lock word under the parking-lot bucket lock.
            unsafe {
                parking_lot_core::park(
                    key,
                    || {
                        let m = self.meta.load(Ordering::Relaxed);
                        m & LOCK_BIT != 0 && m & PARKED_BIT != 0
                    },
                    || {},
                    |_, _| {},
                    parking_lot_core::DEFAULT_PARK_TOKEN,
                    None,
                );
            }
            spins = 0;
        }
    }

    /// Releases one level of the lock held by the calling thread.
    pub fn unlock(&self) {
        let cur = self.meta.load(Ordering::Relaxed);
        debug_assert!(cur & LOCK_BIT != 0, "unlock of an unlocked object");
        if cur & DEPTH_MASK != 0 {
            self.meta.fetch_sub(1 << DEPTH_SHIFT, Ordering::Relaxed);
            return;
        }
        let prev = self.meta.fetch_and(!(LOCK_BIT | PARKED_BIT | OWNER_MASK), Ordering::Release);
        #[cfg(not(loom))]
        if prev & PARKED_BIT != 0 {
            let key = self as *const _ as usize;
            // SAFETY: the key is this object's address, matching the park call.
            unsafe {
                parking_lot_core::unpark_all(key, parking_lot_core::DEFAULT_UNPARK_TOKEN);
            }
        }
    }

    pub fn is_locked(&self) -> bool {
        self.meta.load(Ordering::Relaxed) & LOCK_BIT != 0
    }

    /// True if `tid` currently holds this object's lock.
    pub fn is_locked_by(&self, tid: u16) -> bool {
        let m = self.meta.load(Ordering::Relaxed);
        m & LOCK_BIT != 0 && Self::owner_of(m) == tid
    }
}

/// Holds an object's lock until dropped.
pub struct ObjLockGuard<'a>(&'a Header);

impl<'a> ObjLockGuard<'a> {
    pub fn new(h: &'a Header, tid: u16) -> Self {
        h.lock(tid);
        ObjLockGuard(h)
    }
    /// Wraps a lock the caller already holds.
    pub fn adopt(h: &'a Header) -> Self {
        ObjLockGuard(h)
    }
}

impl Drop for ObjLockGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
    }
}
