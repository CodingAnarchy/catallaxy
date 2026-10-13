//! Per-thread runtime context: identity, allocation, and the handle stack that roots values held
//! by native code across calls back into the interpreter.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

// SAFETY: a Mutator is moved to the thread it serves before that thread runs.
unsafe impl Send for Mutator {}

use catallaxy_gc::{Header, LocalHeap, ObjState, ThreadRecord};
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::class::{Class, BUILTINS};
use crate::dict::DictTable;
use crate::object::*;
use crate::value::{fits_small, Value};

/// A recoverable interruption of execution.
#[derive(Debug)]
pub enum Trap {
    /// A panic: unwinds the current thread.
    Panic(String),
    /// Non-local control flow (break/return out of a block) unwinding to an interpreter frame.
    Unwind { frame: u64, kind: u8, value: Value },
    /// `exit(code)`.
    Exit(i32),
}

pub type RtResult<T> = Result<T, Trap>;

pub fn panic<T>(msg: impl Into<String>) -> RtResult<T> {
    Err(Trap::Panic(msg.into()))
}

pub struct Mutator {
    pub tid: u16,
    pub rec: Arc<ThreadRecord>,
    /// The thread's allocator. Shared with the collector (which retires its pages while the
    /// thread is stopped), so it is only ever accessed through this raw pointer.
    heap: *mut LocalHeap,
    /// Values rooted by native code. Traced as roots by the collector, which reads them while
    /// this thread is stopped, so they live in a separate allocation reached by raw pointer.
    handles: *mut Vec<Value>,
}

/// An index into the handle stack, valid inside the [`Mutator::scope`] that created it.
#[derive(Copy, Clone)]
pub struct Handle(usize);

impl Mutator {
    /// Creates the context for an already-registered thread.
    pub fn new(rec: Arc<ThreadRecord>) -> Self {
        let heap = Box::into_raw(Box::new(LocalHeap::new(rec.tid)));
        let handles = Box::into_raw(Box::new(Vec::new()));
        Mutator { tid: rec.tid, rec, heap, handles }
    }

    /// Whether new objects start young (see `catallaxy_gc::LocalHeap::set_young_alloc`).
    pub fn set_young_alloc(&mut self, on: bool) {
        unsafe { (*self.heap).set_young_alloc(on) }
    }

    /// A second context for the same thread (a fiber): same heap and thread record, its own
    /// handle stack. Release it with [`Mutator::free_handles`].
    pub fn sharing(other: &Mutator) -> Self {
        Mutator {
            tid: other.tid,
            rec: other.rec.clone(),
            heap: other.heap,
            handles: Box::into_raw(Box::new(Vec::new())),
        }
    }

    /// Frees the handle stack of a context made by [`Mutator::sharing`].
    ///
    /// # Safety
    /// No root source may still reference the handles.
    pub unsafe fn free_handles(&mut self) {
        drop(Box::from_raw(self.handles));
        self.handles = std::ptr::null_mut();
    }

    /// Raw pointer to the allocator, for registration with the collector.
    pub fn heap_ptr(&self) -> *mut LocalHeap {
        self.heap
    }

    /// Returns the thread's pages to the global pool (on thread exit, in native state).
    pub fn release_pages(&mut self) {
        // SAFETY: the heap pointer is valid for the mutator's lifetime; the caller is in native
        // state so the collector is not using it concurrently... except a collection may be
        // running, which is why callers hold the thread out of `Running` via unregistration.
        unsafe { (*self.heap).release_pages() }
    }

    // ---- handles ------------------------------------------------------------------------

    /// Runs `f` with a handle scope; handles rooted inside are released afterwards.
    pub fn scope<R>(&mut self, f: impl FnOnce(&mut Mutator) -> R) -> R {
        let base = self.handles_len();
        let r = f(self);
        // SAFETY: only this thread mutates its handles, never during a collection.
        unsafe { (*self.handles).truncate(base) };
        r
    }
    pub fn root(&mut self, v: Value) -> Handle {
        unsafe {
            (*self.handles).push(v);
            Handle((*self.handles).len() - 1)
        }
    }
    pub fn get(&self, h: Handle) -> Value {
        unsafe { (&(*self.handles))[h.0] }
    }
    pub fn set(&mut self, h: Handle, v: Value) {
        unsafe { (&mut (*self.handles))[h.0] = v };
    }
    fn handles_len(&self) -> usize {
        unsafe { (*self.handles).len() }
    }
    /// The current depth of the handle stack, for [`Mutator::release_handles`]. Lets foreign
    /// code (the C API) manage scopes explicitly.
    pub fn handle_mark(&self) -> usize {
        self.handles_len()
    }
    /// Releases every handle rooted since `mark`.
    pub fn release_handles(&mut self, mark: usize) {
        unsafe { (*self.handles).truncate(mark) };
    }
    /// Root values for the GC (called while this thread is stopped).
    pub fn handle_values(&self) -> &[Value] {
        unsafe { &*self.handles }
    }
    /// Pointer to the handle stack, for root sources that must not borrow the mutator.
    pub fn handles_ptr(&self) -> *const Vec<Value> {
        self.handles
    }

    // ---- allocation ---------------------------------------------------------------------

    #[inline]
    pub fn alloc_raw(&mut self, size: usize, cls: &'static Class) -> *mut Header {
        // SAFETY: only this (running) thread touches its heap outside of collections.
        unsafe { (*self.heap).alloc(size, &cls.gc as *const _) }
    }

    pub fn alloc_instance(&mut self, cls: &'static Class) -> Value {
        let n = cls.nfields();
        let h = self.alloc_raw(InstanceObj::size_for(n), cls);
        let inst = unsafe { &*(h as *const InstanceObj) };
        for i in 0..n {
            inst.set(i, Value::NIL);
        }
        Value::from_ptr(h)
    }

    pub fn alloc_str(&mut self, s: &str) -> Value {
        let h = self.alloc_raw(StrObj::BASE + s.len(), BUILTINS.string);
        unsafe {
            let o = &mut *(h as *mut StrObj);
            o.len = s.len();
            std::ptr::copy_nonoverlapping(s.as_ptr(), o.data_ptr(), s.len());
            (*h).set_state(ObjState::Frozen);
        }
        Value::from_ptr(h)
    }

    /// Allocates a big integer, or returns a small int if it fits.
    pub fn int_from_big(&mut self, b: BigInt) -> Value {
        if let Some(i) = b.to_i64() {
            if fits_small(i) {
                return Value::small_int(i);
            }
        }
        let h = self.alloc_raw(std::mem::size_of::<BigIntObj>(), BUILTINS.bigint);
        unsafe {
            std::ptr::write(&mut (*(h as *mut BigIntObj)).value, b);
            (*h).set_state(ObjState::Frozen);
        }
        Value::from_ptr(h)
    }

    #[inline]
    pub fn int(&mut self, i: i64) -> Value {
        if fits_small(i) {
            Value::small_int(i)
        } else {
            self.int_from_big(BigInt::from(i))
        }
    }

    pub fn alloc_list(&mut self, items: Vec<Value>) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<ListObj>(), BUILTINS.list);
        unsafe { ListObj::init(h as *mut ListObj, items) };
        Value::from_ptr(h)
    }

    pub fn alloc_dict(&mut self, table: DictTable) -> Value {
        self.alloc_table(table, BUILTINS.dict)
    }

    pub fn alloc_set(&mut self, table: DictTable) -> Value {
        self.alloc_table(table, BUILTINS.set)
    }

    fn alloc_table(&mut self, table: DictTable, cls: &'static Class) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<DictObj>(), cls);
        unsafe { std::ptr::write(std::ptr::addr_of_mut!((*(h as *mut DictObj)).table), table) };
        Value::from_ptr(h)
    }

    pub fn alloc_tuple(&mut self, items: &[Value]) -> Value {
        let h = self.alloc_raw(TupleObj::size_for(items.len()), BUILTINS.tuple);
        unsafe {
            let t = &mut *(h as *mut TupleObj);
            t.len = items.len();
            std::ptr::copy_nonoverlapping(items.as_ptr(), t.elems_mut(), items.len());
        }
        Value::from_ptr(h)
    }

    pub fn alloc_closure(&mut self, func: u32, caps: &[Value]) -> Value {
        let h = self.alloc_raw(ClosureObj::size_for(caps.len()), BUILTINS.closure);
        unsafe {
            let c = &mut *(h as *mut ClosureObj);
            c.func = func;
            c.ncaps = caps.len() as u32;
            std::ptr::copy_nonoverlapping(caps.as_ptr(), c.caps_mut(), caps.len());
        }
        Value::from_ptr(h)
    }

    pub fn alloc_cell(&mut self, v: Value) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<CellObj>(), BUILTINS.cell);
        unsafe { (*(h as *mut CellObj)).value = AtomicU64::new(v.0) };
        Value::from_ptr(h)
    }

    pub fn alloc_string_builder(&mut self, s: String) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<StringBuilderObj>(), BUILTINS.string_builder);
        unsafe { std::ptr::write((*(h as *mut StringBuilderObj)).buf.get(), s) };
        Value::from_ptr(h)
    }

    pub fn alloc_mutex(&mut self, v: Value) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<MutexObj>(), BUILTINS.mutex);
        unsafe {
            std::ptr::write(&mut (*(h as *mut MutexObj)).raw, parking_lot::ReentrantMutex::new(()));
            (*(h as *mut MutexObj)).value = AtomicU64::new(v.0);
            (*h).set_state(ObjState::Shared);
        }
        Value::from_ptr(h)
    }

    pub fn alloc_atomic(&mut self, v: Value) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<AtomicObj>(), BUILTINS.atomic);
        unsafe {
            (*(h as *mut AtomicObj)).value = AtomicU64::new(v.0);
            (*h).set_state(ObjState::Shared);
        }
        Value::from_ptr(h)
    }

    pub fn alloc_channel(&mut self, capacity: usize) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<ChannelObj>(), BUILTINS.channel);
        let inner = Box::new(ChannelInner {
            queue: parking_lot::Mutex::new(ChannelState { items: Default::default(), closed: false }),
            not_empty: parking_lot::Condvar::new(),
            not_full: parking_lot::Condvar::new(),
            capacity,
            waiters: parking_lot::Mutex::new(Vec::new()),
        });
        unsafe {
            std::ptr::write(&mut (*(h as *mut ChannelObj)).inner, inner);
            (*h).set_state(ObjState::Shared);
        }
        Value::from_ptr(h)
    }

    pub fn alloc_thread(&mut self) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<ThreadObj>(), BUILTINS.thread);
        let inner = std::sync::Arc::new(ThreadInner {
            outcome: parking_lot::Mutex::new(ThreadOutcome::Running),
            done: parking_lot::Condvar::new(),
            handle: parking_lot::Mutex::new(None),
            tid: AtomicU64::new(0),
        });
        unsafe {
            std::ptr::write(&mut (*(h as *mut ThreadObj)).inner, inner);
            (*h).set_state(ObjState::Shared);
        }
        Value::from_ptr(h)
    }

    pub fn alloc_native(&mut self, data: Box<dyn NativeData>) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<NativeObj>(), BUILTINS.native);
        unsafe { std::ptr::write(&mut (*(h as *mut NativeObj)).data, data) };
        Value::from_ptr(h)
    }

    pub fn alloc_range(&mut self, first: Value, last: Value, exclusive: bool) -> Value {
        let v = self.alloc_instance(BUILTINS.range);
        let r = unsafe { as_instance(v) };
        r.set(0, first);
        r.set(1, last);
        r.set(2, Value::bool(exclusive));
        unsafe { (*v.as_ptr()).set_state(ObjState::Frozen) };
        v
    }
}

/// Memory held outside GC pages (vector buffers) counts toward the collection budget.
pub fn note_external(bytes: usize) {
    if bytes > 0 {
        catallaxy_gc::stats::note_allocated(bytes);
    }
}

/// A region in which native code can root values across calls back into the interpreter (which
/// may collect). Handles borrow the scope, so the borrow checker keeps them from escaping it;
/// dropping the scope releases them.
///
/// ```ignore
/// let scope = HandleScope::new(&mut vm.m);
/// let list = scope.root(make_list(scope.mutator()));
/// vm_call_that_may_collect();
/// let still_valid = scope.get(list);
/// ```
pub struct HandleScope<'m> {
    m: *mut Mutator,
    base: usize,
    _borrow: std::marker::PhantomData<&'m mut Mutator>,
}

/// A rooted value, valid for the lifetime of its [`HandleScope`].
#[derive(Copy, Clone)]
pub struct Local<'s> {
    index: usize,
    _scope: std::marker::PhantomData<&'s ()>,
}

impl<'m> HandleScope<'m> {
    pub fn new(m: &'m mut Mutator) -> Self {
        let base = m.handles_len();
        HandleScope { m, base, _borrow: std::marker::PhantomData }
    }

    /// A scope that does not borrow the mutator, so native code can keep calling into the VM
    /// while it holds handles. Handles still cannot outlive the scope.
    ///
    /// # Safety
    /// `m` must outlive the scope and belong to the current thread.
    pub unsafe fn detached(m: *mut Mutator) -> HandleScope<'static> {
        let base = (*m).handles_len();
        HandleScope { m, base, _borrow: std::marker::PhantomData }
    }

    /// Roots `v` until the scope ends.
    pub fn root<'s>(&'s self, v: Value) -> Local<'s> {
        // SAFETY: the scope holds the mutator's unique borrow; handles are only read by the
        // collector while this thread is stopped.
        let hs = unsafe { &mut *(*self.m).handles };
        hs.push(v);
        Local { index: hs.len() - 1, _scope: std::marker::PhantomData }
    }

    pub fn get(&self, h: Local<'_>) -> Value {
        unsafe { (&(*(*self.m).handles))[h.index] }
    }

    pub fn set(&self, h: Local<'_>, v: Value) {
        unsafe { (&mut (*(*self.m).handles))[h.index] = v }
    }

    /// The mutator, for allocation. Allocation never collects, so handles stay valid.
    #[allow(clippy::mut_from_ref)]
    pub fn mutator(&self) -> &mut Mutator {
        unsafe { &mut *self.m }
    }
}

impl Drop for HandleScope<'_> {
    fn drop(&mut self) {
        unsafe { (*(*self.m).handles).truncate(self.base) };
    }
}
