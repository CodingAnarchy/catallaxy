//! Heap object layouts and their GC hooks.
//!
//! Every object starts with a [`Header`]. Instances keep their fields inline as atomic words, so a
//! racing reader can never see a torn value. Containers keep their elements in Rust-side buffers;
//! the ownership rules (and per-object locks for shared objects) guarantee those buffers are never
//! mutated while another thread reads them.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use catallaxy_gc::{GcVTable, Header, Tracer, HEADER_SIZE};
use num_bigint::BigInt;

use crate::class::{heap_class, ObjKind};
use crate::dict::DictTable;
use crate::value::Value;

#[inline(always)]
pub fn mark_value(t: &mut Tracer, v: Value) {
    if v.is_ptr() {
        t.mark(v.as_ptr());
    }
}

#[repr(C)]
pub struct InstanceObj {
    pub header: Header,
    fields: [AtomicU64; 0],
}

impl InstanceObj {
    #[inline(always)]
    pub fn field(&self, i: usize) -> &AtomicU64 {
        // SAFETY: instances are allocated with `nfields` trailing words and callers index within it.
        unsafe { &*self.fields.as_ptr().add(i) }
    }
    #[inline(always)]
    pub fn get(&self, i: usize) -> Value {
        Value(self.field(i).load(Ordering::Relaxed))
    }
    #[inline(always)]
    pub fn get_acquire(&self, i: usize) -> Value {
        Value(self.field(i).load(Ordering::Acquire))
    }
    #[inline(always)]
    pub fn set(&self, i: usize, v: Value) {
        self.field(i).store(v.0, Ordering::Relaxed)
    }
    #[inline(always)]
    pub fn set_release(&self, i: usize, v: Value) {
        self.field(i).store(v.0, Ordering::Release)
    }
    pub fn nfields(&self) -> usize {
        unsafe { heap_class(&self.header).nfields() }
    }
    pub fn size_for(nfields: usize) -> usize {
        HEADER_SIZE + 8 * nfields
    }
}

#[repr(C)]
pub struct StrObj {
    pub header: Header,
    pub len: usize,
    /// Cached hash; 0 means not yet computed.
    pub hash: AtomicU64,
    /// Cached char count plus one; 0 means not yet computed.
    pub chars: AtomicU64,
    data: [u8; 0],
}

impl StrObj {
    pub const BASE: usize = std::mem::size_of::<StrObj>();
    pub fn as_str(&self) -> &str {
        // SAFETY: strings are created from valid UTF-8 and never mutated.
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.data.as_ptr(), self.len)) }
    }
    pub(crate) fn data_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
    pub fn char_count(&self) -> usize {
        let c = self.chars.load(Ordering::Relaxed);
        if c != 0 {
            return (c - 1) as usize;
        }
        let s = self.as_str();
        let n = if s.is_ascii() { s.len() } else { s.chars().count() };
        self.chars.store(n as u64 + 1, Ordering::Relaxed);
        n
    }
    pub fn hash_code(&self) -> u64 {
        let h = self.hash.load(Ordering::Relaxed);
        if h != 0 {
            return h;
        }
        let mut h = crate::ops::hash_bytes(self.as_str().as_bytes());
        if h == 0 {
            h = 1;
        }
        self.hash.store(h, Ordering::Relaxed);
        h
    }
}

#[repr(C)]
pub struct BigIntObj {
    pub header: Header,
    pub value: BigInt,
}

#[repr(C)]
pub struct ListObj {
    pub header: Header,
    pub vec: UnsafeCell<Vec<Value>>,
}

impl ListObj {
    /// # Safety
    /// The caller must have read access per the ownership rules (see `ownership::with_read`).
    #[inline(always)]
    pub unsafe fn items(&self) -> &Vec<Value> {
        &*self.vec.get()
    }
    /// # Safety
    /// The caller must have write access per the ownership rules, and must not hold another
    /// reference to the items.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn items_mut(&self) -> &mut Vec<Value> {
        &mut *self.vec.get()
    }
}

#[repr(C)]
pub struct DictObj {
    pub header: Header,
    pub table: UnsafeCell<DictTable>,
}

impl DictObj {
    /// # Safety
    /// As for [`ListObj::items`].
    #[inline(always)]
    pub unsafe fn table(&self) -> &DictTable {
        &*self.table.get()
    }
    /// # Safety
    /// As for [`ListObj::items_mut`].
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn table_mut(&self) -> &mut DictTable {
        &mut *self.table.get()
    }
}

#[repr(C)]
pub struct TupleObj {
    pub header: Header,
    pub len: usize,
    elems: [Value; 0],
}

impl TupleObj {
    pub fn elems(&self) -> &[Value] {
        unsafe { std::slice::from_raw_parts(self.elems.as_ptr(), self.len) }
    }
    pub(crate) fn elems_mut(&mut self) -> *mut Value {
        self.elems.as_mut_ptr()
    }
    pub fn size_for(n: usize) -> usize {
        std::mem::size_of::<TupleObj>() + 8 * n
    }
}

#[repr(C)]
pub struct ClosureObj {
    pub header: Header,
    pub func: u32,
    pub ncaps: u32,
    caps: [Value; 0],
}

impl ClosureObj {
    pub fn caps(&self) -> &[Value] {
        unsafe { std::slice::from_raw_parts(self.caps.as_ptr(), self.ncaps as usize) }
    }
    pub(crate) fn caps_mut(&mut self) -> *mut Value {
        self.caps.as_mut_ptr()
    }
    pub fn size_for(n: usize) -> usize {
        std::mem::size_of::<ClosureObj>() + 8 * n
    }
}

#[repr(C)]
pub struct CellObj {
    pub header: Header,
    pub value: AtomicU64,
}

#[repr(C)]
pub struct StringBuilderObj {
    pub header: Header,
    pub buf: UnsafeCell<String>,
}

#[repr(C)]
pub struct MutexObj {
    pub header: Header,
    pub raw: parking_lot::ReentrantMutex<()>,
    pub value: AtomicU64,
}

#[repr(C)]
pub struct AtomicObj {
    pub header: Header,
    pub value: AtomicU64,
}

pub struct ChannelInner {
    pub queue: parking_lot::Mutex<ChannelState>,
    pub not_empty: parking_lot::Condvar,
    pub not_full: parking_lot::Condvar,
    pub capacity: usize,
}

pub struct ChannelState {
    pub items: std::collections::VecDeque<Value>,
    pub closed: bool,
}

#[repr(C)]
pub struct ChannelObj {
    pub header: Header,
    pub inner: Box<ChannelInner>,
}

/// Outcome of a finished thread: its value or its panic message.
pub enum ThreadOutcome {
    Running,
    Done(Value),
    Panicked(String),
    Joined,
}

pub struct ThreadInner {
    pub outcome: parking_lot::Mutex<ThreadOutcome>,
    pub done: parking_lot::Condvar,
    pub handle: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Thread id of the running thread (for transferring its result).
    pub tid: AtomicU64,
}

#[repr(C)]
pub struct ThreadObj {
    pub header: Header,
    pub inner: std::sync::Arc<ThreadInner>,
}

pub trait NativeData: std::any::Any + Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;
}
impl<T: std::any::Any + Send + Sync> NativeData for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[repr(C)]
pub struct NativeObj {
    pub header: Header,
    pub data: Box<dyn NativeData>,
}

// ---------------------------------------------------------------------------------------------
// GC hooks per object kind.

unsafe fn trace_nothing(_: *const Header, _: &mut Tracer) {}

unsafe fn trace_instance(h: *const Header, t: &mut Tracer) {
    let o = &*(h as *const InstanceObj);
    for i in 0..heap_class(h).nfields() {
        mark_value(t, o.get(i));
    }
}
unsafe fn size_instance(h: *const Header) -> usize {
    InstanceObj::size_for(heap_class(h).nfields())
}

unsafe fn size_str(h: *const Header) -> usize {
    StrObj::BASE + (*(h as *const StrObj)).len
}

unsafe fn fin_bigint(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut BigIntObj)).value);
}
unsafe fn size_bigint(h: *const Header) -> usize {
    std::mem::size_of::<BigIntObj>() + (*(h as *const BigIntObj)).value.bits() as usize / 8
}

unsafe fn trace_list(h: *const Header, t: &mut Tracer) {
    for &v in (*(*(h as *const ListObj)).vec.get()).iter() {
        mark_value(t, v);
    }
}
unsafe fn fin_list(h: *mut Header) {
    std::ptr::drop_in_place((*(h as *mut ListObj)).vec.get());
}
unsafe fn size_list(h: *const Header) -> usize {
    std::mem::size_of::<ListObj>() + 8 * (*(*(h as *const ListObj)).vec.get()).capacity()
}

unsafe fn trace_dict(h: *const Header, t: &mut Tracer) {
    (*(*(h as *const DictObj)).table.get()).trace(t);
}
unsafe fn fin_dict(h: *mut Header) {
    std::ptr::drop_in_place((*(h as *mut DictObj)).table.get());
}
unsafe fn size_dict(h: *const Header) -> usize {
    std::mem::size_of::<DictObj>() + (*(*(h as *const DictObj)).table.get()).heap_bytes()
}

unsafe fn trace_tuple(h: *const Header, t: &mut Tracer) {
    for &v in (*(h as *const TupleObj)).elems() {
        mark_value(t, v);
    }
}
unsafe fn size_tuple(h: *const Header) -> usize {
    TupleObj::size_for((*(h as *const TupleObj)).len)
}

unsafe fn trace_closure(h: *const Header, t: &mut Tracer) {
    for &v in (*(h as *const ClosureObj)).caps() {
        mark_value(t, v);
    }
}
unsafe fn size_closure(h: *const Header) -> usize {
    ClosureObj::size_for((*(h as *const ClosureObj)).ncaps as usize)
}

unsafe fn trace_cell(h: *const Header, t: &mut Tracer) {
    mark_value(t, Value((*(h as *const CellObj)).value.load(Ordering::Relaxed)));
}
unsafe fn size_cell(_: *const Header) -> usize {
    std::mem::size_of::<CellObj>()
}

unsafe fn fin_sb(h: *mut Header) {
    std::ptr::drop_in_place((*(h as *mut StringBuilderObj)).buf.get());
}
unsafe fn size_sb(h: *const Header) -> usize {
    std::mem::size_of::<StringBuilderObj>() + (*(*(h as *const StringBuilderObj)).buf.get()).capacity()
}

unsafe fn trace_mutex(h: *const Header, t: &mut Tracer) {
    mark_value(t, Value((*(h as *const MutexObj)).value.load(Ordering::Relaxed)));
}
unsafe fn fin_mutex(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut MutexObj)).raw);
}
unsafe fn size_mutex(_: *const Header) -> usize {
    std::mem::size_of::<MutexObj>()
}

unsafe fn trace_atomic(h: *const Header, t: &mut Tracer) {
    mark_value(t, Value((*(h as *const AtomicObj)).value.load(Ordering::Relaxed)));
}
unsafe fn size_atomic(_: *const Header) -> usize {
    std::mem::size_of::<AtomicObj>()
}

unsafe fn trace_channel(h: *const Header, t: &mut Tracer) {
    // The world is stopped; no thread holds the queue lock across a safepoint.
    let inner = &(*(h as *const ChannelObj)).inner;
    let q = inner.queue.data_ptr();
    for &v in (*q).items.iter() {
        mark_value(t, v);
    }
}
unsafe fn fin_channel(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut ChannelObj)).inner);
}
unsafe fn size_channel(h: *const Header) -> usize {
    std::mem::size_of::<ChannelObj>() + 8 * (*(*(h as *const ChannelObj)).inner.queue.data_ptr()).items.capacity()
}

unsafe fn trace_thread(h: *const Header, t: &mut Tracer) {
    let inner = &(*(h as *const ThreadObj)).inner;
    if let ThreadOutcome::Done(v) = &*inner.outcome.data_ptr() {
        mark_value(t, *v);
    }
}
unsafe fn fin_thread(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut ThreadObj)).inner);
}
unsafe fn size_thread(_: *const Header) -> usize {
    std::mem::size_of::<ThreadObj>() + 128
}

unsafe fn fin_native(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut NativeObj)).data);
}
unsafe fn size_native(_: *const Header) -> usize {
    std::mem::size_of::<NativeObj>()
}

unsafe fn size_zero(_: *const Header) -> usize {
    0
}

#[allow(clippy::type_complexity)]
pub(crate) fn vtable_for(kind: ObjKind) -> GcVTable {
    let (trace, finalize, size): (
        unsafe fn(*const Header, &mut Tracer),
        Option<unsafe fn(*mut Header)>,
        unsafe fn(*const Header) -> usize,
    ) = match kind {
        ObjKind::Immediate => (trace_nothing, None, size_zero),
        ObjKind::Instance => (trace_instance, None, size_instance),
        ObjKind::Str => (trace_nothing, None, size_str),
        ObjKind::BigInt => (trace_nothing, Some(fin_bigint), size_bigint),
        ObjKind::List => (trace_list, Some(fin_list), size_list),
        ObjKind::Dict | ObjKind::Set => (trace_dict, Some(fin_dict), size_dict),
        ObjKind::Tuple => (trace_tuple, None, size_tuple),
        ObjKind::Closure => (trace_closure, None, size_closure),
        ObjKind::Cell => (trace_cell, None, size_cell),
        ObjKind::StringBuilder => (trace_nothing, Some(fin_sb), size_sb),
        ObjKind::Mutex => (trace_mutex, Some(fin_mutex), size_mutex),
        ObjKind::Atomic => (trace_atomic, None, size_atomic),
        ObjKind::Channel => (trace_channel, Some(fin_channel), size_channel),
        ObjKind::Thread => (trace_thread, Some(fin_thread), size_thread),
        ObjKind::Native => (trace_nothing, Some(fin_native), size_native),
    };
    GcVTable { trace, finalize, size }
}

// ---------------------------------------------------------------------------------------------
// Typed views. Callers must have checked the kind (usually statically, by the type checker).

macro_rules! view {
    ($fn:ident, $ty:ty) => {
        /// # Safety
        /// `v` must be a pointer to an object of this kind.
        #[inline(always)]
        pub unsafe fn $fn<'a>(v: Value) -> &'a $ty {
            &*(v.as_ptr() as *const $ty)
        }
    };
}
view!(as_instance, InstanceObj);
view!(as_str_obj, StrObj);
view!(as_bigint, BigIntObj);
view!(as_list, ListObj);
view!(as_dict, DictObj);
view!(as_tuple, TupleObj);
view!(as_closure, ClosureObj);
view!(as_cell, CellObj);
view!(as_sb, StringBuilderObj);
view!(as_mutex, MutexObj);
view!(as_atomic, AtomicObj);
view!(as_channel, ChannelObj);
view!(as_thread, ThreadObj);
view!(as_native, NativeObj);

/// The object kind of a value (`Immediate` for non-pointers).
#[inline]
pub fn kind_of(v: Value) -> ObjKind {
    if v.is_ptr() {
        unsafe { heap_class(v.as_ptr()).kind }
    } else {
        ObjKind::Immediate
    }
}

/// Returns the string slice if `v` is a string.
#[inline]
pub fn str_of<'a>(v: Value) -> Option<&'a str> {
    if kind_of(v) == ObjKind::Str {
        Some(unsafe { as_str_obj(v) }.as_str())
    } else {
        None
    }
}
