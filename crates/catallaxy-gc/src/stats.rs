//! Allocation accounting that decides when to request a collection.

use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static LIVE_AFTER_GC: AtomicUsize = AtomicUsize::new(0);
static THRESHOLD: AtomicUsize = AtomicUsize::new(MIN_THRESHOLD);

/// Whether `CATX_GC_STRESS` is on.
pub(crate) fn stress() -> bool {
    min_threshold() < MIN_THRESHOLD
}

/// `CATX_GC_STRESS=1` makes collections very frequent (for finding rooting bugs).
fn min_threshold() -> usize {
    static STRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *STRESS.get_or_init(|| match std::env::var("CATX_GC_STRESS") {
        Ok(v) if v != "0" => {
            let t = 64 * 1024;
            THRESHOLD.store(t, Ordering::Relaxed);
            t
        }
        _ => MIN_THRESHOLD,
    })
}
static COLLECTIONS: AtomicUsize = AtomicUsize::new(0);
static PAUSE_TOTAL_NS: AtomicUsize = AtomicUsize::new(0);
static MINORS: AtomicUsize = AtomicUsize::new(0);
static MINOR_NS: AtomicUsize = AtomicUsize::new(0);
static MINOR_MAX_NS: AtomicUsize = AtomicUsize::new(0);
static PROMOTED: AtomicUsize = AtomicUsize::new(0);
static MINOR_FREED: AtomicUsize = AtomicUsize::new(0);
static PAUSE_MAX_NS: AtomicUsize = AtomicUsize::new(0);

pub const MIN_THRESHOLD: usize = 8 * 1024 * 1024;

/// Records `bytes` of new heap (or Rust-side buffer) memory, requesting a collection once the
/// budget since the last collection is spent.
#[inline]
pub fn note_allocated(bytes: usize) {
    let total = ALLOCATED.fetch_add(bytes, Ordering::Relaxed) + bytes;
    let _ = min_threshold();
    if total >= THRESHOLD.load(Ordering::Relaxed) {
        crate::collector::request_gc();
    }
}

pub(crate) fn note_freed(_bytes: usize) {}

pub(crate) fn after_collection(live_bytes: usize, mutators: usize) {
    LIVE_AFTER_GC.store(live_bytes, Ordering::Relaxed);
    ALLOCATED.store(0, Ordering::Relaxed);
    let floor = min_threshold();
    // The allocation budget between collections grows with the live heap (heap growth factor
    // 2) and with the number of allocating threads, so adding threads does not multiply pauses.
    let per_thread = MIN_THRESHOLD * mutators.clamp(1, 32);
    let t = if floor < MIN_THRESHOLD { floor } else { live_bytes.max(per_thread) };
    THRESHOLD.store(t, Ordering::Relaxed);
    COLLECTIONS.fetch_add(1, Ordering::Relaxed);
}

/// The allocation budget until the next major collection.
pub(crate) fn threshold() -> usize {
    THRESHOLD.load(Ordering::Relaxed)
}

static RELEASED: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn note_released(bytes: usize) {
    RELEASED.fetch_add(bytes, Ordering::Relaxed);
}

/// Heap bytes returned to the OS so far.
pub fn released_bytes() -> usize {
    RELEASED.load(Ordering::Relaxed)
}

/// Records one stop-the-world pause.
pub(crate) fn note_pause(ns: usize) {
    PAUSE_TOTAL_NS.fetch_add(ns, Ordering::Relaxed);
    PAUSE_MAX_NS.fetch_max(ns, Ordering::Relaxed);
}

/// Total and longest stop-the-world pause so far, in nanoseconds.
pub fn pauses() -> (usize, usize) {
    (PAUSE_TOTAL_NS.load(Ordering::Relaxed), PAUSE_MAX_NS.load(Ordering::Relaxed))
}

pub(crate) fn note_minor(ns: usize, freed: usize, promoted: usize) {
    MINORS.fetch_add(1, Ordering::Relaxed);
    MINOR_NS.fetch_add(ns, Ordering::Relaxed);
    MINOR_MAX_NS.fetch_max(ns, Ordering::Relaxed);
    MINOR_FREED.fetch_add(freed, Ordering::Relaxed);
    PROMOTED.fetch_add(promoted, Ordering::Relaxed);
}

/// Thread-local minor collections: (count, total ns, longest ns, bytes freed, bytes promoted).
pub fn minors() -> (usize, usize, usize, usize, usize) {
    (
        MINORS.load(Ordering::Relaxed),
        MINOR_NS.load(Ordering::Relaxed),
        MINOR_MAX_NS.load(Ordering::Relaxed),
        MINOR_FREED.load(Ordering::Relaxed),
        PROMOTED.load(Ordering::Relaxed),
    )
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
