//! Garbage collector for the Catallaxy VM.
//!
//! A non-moving, parallel, stop-the-world mark-sweep collector with thread-local allocation and
//! lazy sweeping. See `docs/design.md` ("Memory management") for the design. Objects never move,
//! so raw pointers held between safepoints stay valid.
//!
//! The collector knows nothing about the runtime's value representation: objects are described
//! by a [`GcVTable`] that traces their references, and roots are supplied by [`RootSource`]s.

mod collector;
mod header;
mod heap;
pub mod stats;

pub use collector::{
    add_global_roots, collect_now, enter_native, install, is_gc_requested, leave_native, register_thread, request_gc, retire,
    safepoint, safepoint_slow, unregister_thread, NoGcGuard, RootSource, ThreadRecord, ThreadState,
    Tracer,
};
pub use header::{GcVTable, Header, ObjLockGuard, ObjState, HEADER_SIZE};
pub use heap::{size_class_of, total_heap_bytes, LocalHeap, MAX_SMALL, SIZE_CLASSES};

/// Runs `f` with the thread marked as in native code (blocking I/O, waiting on locks or
/// channels). `f` must not touch the heap.
pub fn blocking<R>(rec: &ThreadRecord, f: impl FnOnce() -> R) -> R {
    enter_native(rec);
    let r = f();
    leave_native(rec);
    r
}
