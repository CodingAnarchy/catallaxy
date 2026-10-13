//! Helpers shared by the runtime's integration tests. Each test file is its own binary, so under
//! Miri each starts with a fresh heap (see `strings.rs`).
#![allow(dead_code)]

use std::sync::Mutex as StdMutex;

use catallaxy_gc::{enter_native, install, leave_native, register_thread, unregister_thread, RootSource, Tracer};
use catallaxy_runtime::ops::Interop;
use catallaxy_runtime::{Mutator, RtResult, Sym, Value};

/// A minimal host: one mutator whose roots are its handle stack.
pub struct Host {
    /// Shared with the collector's root scan, so owned through a raw pointer.
    raw: *mut Mutator,
    pub m: &'static mut Mutator,
    finished: bool,
}

struct HandleRoots(*const Vec<Value>);
impl RootSource for HandleRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        for &v in unsafe { &*self.0 } {
            if v.is_ptr() {
                t.mark(v.as_ptr());
            }
        }
    }
}

impl Interop for Host {
    fn mutator(&mut self) -> &mut Mutator {
        self.m
    }
    fn call_method(&mut self, _: Value, _: Sym, _: &[Value]) -> RtResult<Option<Value>> {
        Ok(None)
    }
}

impl Host {
    pub fn new() -> Self {
        let rec = register_thread();
        let raw = Box::into_raw(Box::new(Mutator::new(rec)));
        let roots: Box<dyn RootSource> = Box::new(HandleRoots(unsafe { (*raw).handles_ptr() }));
        let m = unsafe { &mut *raw };
        unsafe { install(&m.rec, Box::into_raw(roots), m.heap_ptr()) };
        leave_native(&m.rec);
        Host { raw, m, finished: false }
    }
    pub fn gc(&mut self) {
        catallaxy_gc::collect_now(&self.m.rec);
    }
    pub fn minor(&mut self) {
        catallaxy_gc::collect_minor(&self.m.rec);
    }
    pub fn finish(mut self) {
        self.unregister();
    }
    fn unregister(&mut self) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        enter_native(&self.m.rec);
        unregister_thread(&self.m.rec);
        self.m.release_pages();
        // The mutator is intentionally leaked: the collector may still hold its root pointer.
        let _ = self.raw;
    }
}

impl Drop for Host {
    /// Also runs when a test thread panics: a thread left registered as running would make
    /// every later collection wait for it forever, turning one failure into a hung test run.
    fn drop(&mut self) {
        self.unregister();
    }
}

pub static SERIAL: StdMutex<()> = StdMutex::new(());

pub fn n(i: usize) -> usize {
    if cfg!(miri) {
        (i / 50).max(3)
    } else {
        i
    }
}
