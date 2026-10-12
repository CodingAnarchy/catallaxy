//! Allocation accounting that decides when to request a collection.

use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static LIVE_AFTER_GC: AtomicUsize = AtomicUsize::new(0);
static THRESHOLD: AtomicUsize = AtomicUsize::new(MIN_THRESHOLD);
static COLLECTIONS: AtomicUsize = AtomicUsize::new(0);

pub const MIN_THRESHOLD: usize = 8 * 1024 * 1024;

/// Records `bytes` of new heap (or Rust-side buffer) memory, requesting a collection once the
/// budget since the last collection is spent.
#[inline]
pub fn note_allocated(bytes: usize) {
    let total = ALLOCATED.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if total >= THRESHOLD.load(Ordering::Relaxed) {
        crate::collector::request_gc();
    }
}

pub(crate) fn note_freed(_bytes: usize) {}

pub(crate) fn after_collection(live_bytes: usize) {
    LIVE_AFTER_GC.store(live_bytes, Ordering::Relaxed);
    ALLOCATED.store(0, Ordering::Relaxed);
    THRESHOLD.store(live_bytes.max(MIN_THRESHOLD), Ordering::Relaxed);
    COLLECTIONS.fetch_add(1, Ordering::Relaxed);
}

/// Number of completed collections.
pub fn collections() -> usize {
    COLLECTIONS.load(Ordering::Relaxed)
}

/// Approximate live bytes found by the last collection.
pub fn live_bytes() -> usize {
    LIVE_AFTER_GC.load(Ordering::Relaxed)
}

/// Bytes allocated since the last collection.
pub fn allocated_since_gc() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}
