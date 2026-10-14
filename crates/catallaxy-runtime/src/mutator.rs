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

/// Copies `parts` one after another to `dst`.
///
/// # Safety
/// `dst` must have room for all the parts' bytes.
unsafe fn copy_parts<'a>(mut dst: *mut u8, parts: impl Iterator<Item = &'a str>) {
    for p in parts {
        std::ptr::copy_nonoverlapping(p.as_ptr(), dst, p.len());
        dst = dst.add(p.len());
    }
}

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

/// Checks a request for `count` items of `size` bytes before allocating it, so that sizes the
/// machine cannot provide raise a `MemoryError` panic (with the program's stack trace) instead
/// of aborting the process in the allocator. Returns the size in bytes.
pub fn check_alloc(count: usize, size: usize) -> RtResult<usize> {
    let Some(bytes) = count.checked_mul(size) else {
        return panic(format!("MemoryError: cannot allocate {count} items of {size} bytes"));
    };
    catallaxy_gc::limits::check_request(bytes).map_err(|e| Trap::Panic(format!("MemoryError: {e}")))?;
    Ok(bytes)
}

/// An empty vector with room for exactly `n` items, or a `MemoryError` panic if that much
/// memory cannot be had (see [`check_alloc`]).
pub fn try_vec<T>(n: usize) -> RtResult<Vec<T>> {
    let bytes = check_alloc(n, std::mem::size_of::<T>())?;
    let mut v = Vec::new();
    if v.try_reserve_exact(n).is_err() {
        return refused(format!("cannot allocate {bytes} bytes"));
    }
    Ok(v)
}

/// The `MemoryError` for a request the allocator refused (`what` says which). Hands the
/// collector's headroom back to the OS first, so that reporting the error does not fail too.
#[cold]
pub fn refused<T>(what: impl std::fmt::Display) -> RtResult<T> {
    catallaxy_gc::limits::release_headroom();
    panic(format!("MemoryError: {what} (the allocator refused)"))
}

/// An empty string with room for exactly `bytes` bytes, or a `MemoryError` panic.
pub fn try_string(bytes: usize) -> RtResult<String> {
    check_alloc(bytes, 1)?;
    let mut s = String::new();
    if s.try_reserve_exact(bytes).is_err() {
        return refused(format!("cannot allocate {bytes} bytes"));
    }
    Ok(s)
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
        self.alloc_str_parts(&[s])
    }

    /// Allocates the concatenation of `parts`.
    pub fn alloc_str_parts(&mut self, parts: &[&str]) -> Value {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        let chars = (len >= StrObj::INDEX_MIN)
            .then(|| parts.iter().map(|p| if p.is_ascii() { p.len() } else { p.chars().count() }).sum());
        // SAFETY: the parts are UTF-8 and together `len` bytes with `chars` characters.
        unsafe { self.alloc_str_with(len, chars, |dst| copy_parts(dst, parts.iter().copied())) }
    }

    /// Allocates the concatenation of `n` strings, `part(i)` being the `i`th, copying each once.
    /// The character count comes from the parts' cached counts, so no byte is scanned twice.
    pub fn alloc_concat<'a>(&mut self, n: usize, part: impl Fn(usize) -> &'a StrObj) -> Value {
        let len: usize = (0..n).map(|i| part(i).len).sum();
        let chars = (len >= StrObj::INDEX_MIN).then(|| (0..n).map(|i| part(i).char_count()).sum());
        // SAFETY: as above. Allocation never collects, so the parts stay valid while copied.
        unsafe { self.alloc_str_with(len, chars, |dst| copy_parts(dst, (0..n).map(|i| part(i).as_str()))) }
    }

    /// As [`Mutator::alloc_str`], but a string the allocator refuses raises a `MemoryError`
    /// instead of aborting. A refusal first runs a collection and retries, so every value the
    /// caller holds must be rooted (as interpreter registers and native arguments are); `s`
    /// itself may point into a rooted string, since objects never move.
    pub fn try_alloc_str(&mut self, s: &str) -> RtResult<Value> {
        let chars = (s.len() >= StrObj::INDEX_MIN).then(|| if s.is_ascii() { s.len() } else { s.chars().count() });
        // SAFETY: `s` is UTF-8 with `chars` characters.
        unsafe { self.try_alloc_str_with(s.len(), chars, |dst| copy_parts(dst, std::iter::once(s))) }
    }

    /// As [`Mutator::alloc_concat`], raising a `MemoryError` as [`Mutator::try_alloc_str`] does.
    pub fn try_alloc_concat<'a>(&mut self, n: usize, part: impl Fn(usize) -> &'a StrObj) -> RtResult<Value> {
        let len: usize = (0..n).map(|i| part(i).len).sum();
        let chars = (len >= StrObj::INDEX_MIN).then(|| (0..n).map(|i| part(i).char_count()).sum());
        // SAFETY: as for `alloc_concat`; the parts are rooted, so a collection keeps them.
        unsafe { self.try_alloc_str_with(len, chars, |dst| copy_parts(dst, (0..n).map(|i| part(i).as_str()))) }
    }

    /// As [`Mutator::alloc_str_with`], for the `try_` variants.
    ///
    /// # Safety
    /// As for [`Mutator::alloc_str_with`].
    #[inline]
    unsafe fn try_alloc_str_with(
        &mut self,
        len: usize,
        chars: Option<usize>,
        fill: impl FnOnce(*mut u8),
    ) -> RtResult<Value> {
        let (size, word) = StrObj::layout(len, chars);
        if size > catallaxy_gc::MAX_SMALL {
            // Refuse what the machine cannot provide before the allocator hands out memory
            // that overcommit cannot back.
            check_alloc(size, 1)?;
        }
        let vtable = &BUILTINS.string.gc as *const _;
        let mut h = (*self.heap).try_alloc_uninit(size, vtable);
        if h.is_null() {
            h = self.retry_str(size, len)?;
        }
        Ok(self.init_str(h, size, len, word, fill))
    }

    /// The allocator refused a string of `len` bytes (`size` with its header): garbage may be
    /// holding the memory, so collects and tries once more.
    #[cold]
    #[inline(never)]
    fn retry_str(&mut self, size: usize, len: usize) -> RtResult<*mut Header> {
        catallaxy_gc::collect_to_free(&self.rec);
        // SAFETY: as for the first attempt; the caller initializes the object.
        let h = unsafe { (*self.heap).try_alloc_uninit(size, &BUILTINS.string.gc as *const _) };
        if h.is_null() {
            return refused(format!("cannot allocate a string of {len} bytes"));
        }
        Ok(h)
    }

    /// Allocates a frozen string of `len` bytes; `fill` writes them. `chars` (the character
    /// count) must be known for strings of at least [`StrObj::INDEX_MIN`] bytes.
    ///
    /// # Safety
    /// `fill` must write exactly `len` bytes of UTF-8 holding `chars` characters.
    unsafe fn alloc_str_with(&mut self, len: usize, chars: Option<usize>, fill: impl FnOnce(*mut u8)) -> Value {
        debug_assert!(len < StrObj::INDEX_MIN || chars.is_some());
        let (size, word) = StrObj::layout(len, chars);
        // Large strings skip zeroing: every byte is written below, before any safepoint.
        let h = (*self.heap).alloc_uninit(size, &BUILTINS.string.gc as *const _);
        self.init_str(h, size, len, word, fill)
    }

    /// Fills in a string object of `size` bytes just allocated at `h`.
    ///
    /// # Safety
    /// As for [`Mutator::alloc_str_with`]; `size` and `word` come from `StrObj::layout`.
    #[inline(always)]
    unsafe fn init_str(
        &mut self,
        h: *mut Header,
        size: usize,
        len: usize,
        word: u64,
        fill: impl FnOnce(*mut u8),
    ) -> Value {
        let o = h as *mut StrObj;
        std::ptr::addr_of_mut!((*o).len).write(len);
        std::ptr::addr_of_mut!((*o).hash).write(AtomicU64::new(0));
        std::ptr::addr_of_mut!((*o).chars).write(AtomicU64::new(word));
        let data = (h as *mut u8).add(StrObj::BASE);
        fill(data);
        // The char index (and padding) start zeroed.
        std::ptr::write_bytes(data.add(len), 0, size - StrObj::BASE - len);
        (*h).set_state(ObjState::Frozen);
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

    /// A list holding `items`, which takes over the vector's allocation (and capacity).
    /// Small lists copy the items into the list object instead (see [`ListObj`]).
    pub fn alloc_list(&mut self, items: Vec<Value>) -> Value {
        let icap = ListObj::inline_cap_for(items.len());
        let h = self.alloc_raw(ListObj::size_for(icap), BUILTINS.list);
        unsafe { ListObj::init(h as *mut ListObj, items, icap) };
        Value::from_ptr(h)
    }

    /// A list holding a copy of `items`, with room for at least `cap` of them.
    pub fn alloc_list_from_slice(&mut self, items: &[Value], cap: usize) -> Value {
        let icap = ListObj::inline_cap_for(cap.max(items.len()));
        let h = self.alloc_raw(ListObj::size_for(icap), BUILTINS.list);
        unsafe { ListObj::init_from_slice(h as *mut ListObj, items, cap, icap) };
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

    /// Allocates a frozen native handle: one whose data is immutable or synchronizes itself, so
    /// it may be stored in frozen objects and used from any thread (`NativeHandle` in the prelude).
    pub fn alloc_native_frozen(&mut self, data: Box<dyn NativeData>) -> Value {
        let v = self.alloc_native(data);
        // A leaf, like a string: no children to escape.
        unsafe { (*v.as_ptr()).set_state(ObjState::Frozen) };
        v
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
