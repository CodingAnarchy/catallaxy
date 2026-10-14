//! Heap object layouts and their GC hooks.
//!
//! Every object starts with a [`Header`]. Instances keep their fields inline as atomic words, so a
//! racing reader can never see a torn value. Lists do the same with their elements and swap whole
//! buffers on growth, so their reads never lock. Dicts and other containers keep Rust-side
//! buffers; the ownership rules (and per-object locks for shared objects) guarantee those are
//! never mutated while another thread reads them.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use catallaxy_gc::{GcVTable, Header, Tracer, HEADER_SIZE};
use num_bigint::BigInt;

use crate::buf::WordBuf;
use crate::class::{heap_class, ObjKind};
use crate::dict::DictTable;
use crate::ownership::Access;
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

/// A string: immutable UTF-8 bytes with a cached hash and character count.
///
/// Strings of at least [`StrObj::INDEX_MIN`] bytes get their character count when they are
/// allocated. When such a string is not ASCII, its allocation also reserves a sparse index after
/// the bytes: one `u32` byte offset for every [`StrObj::STRIDE`]th character, filled in the first
/// time a character position is looked up. That makes `s[i]` and slicing O(1) for ASCII strings
/// (character count equals byte length) and O(`STRIDE`) for the rest.
#[repr(C)]
pub struct StrObj {
    pub header: Header,
    pub len: usize,
    /// Cached hash; 0 means not yet computed.
    pub hash: AtomicU64,
    /// Cached char count plus one (0 means not yet computed), and [`StrObj::INDEX_READY`] once
    /// the char index has been filled in.
    pub chars: AtomicU64,
    data: [u8; 0],
}

impl StrObj {
    pub const BASE: usize = std::mem::size_of::<StrObj>();
    /// Strings at least this long have an eagerly computed char count, and an index if not ASCII.
    pub const INDEX_MIN: usize = 256;
    /// Characters per index entry.
    pub const STRIDE: usize = 64;
    const INDEX_READY: u64 = 1 << 63;

    /// The allocation size of a string of `len` bytes and `chars` characters, and the initial
    /// value of its `chars` word. `chars` must be known (`Some`) when `len >= INDEX_MIN`.
    pub(crate) fn layout(len: usize, chars: Option<usize>) -> (usize, u64) {
        let word = chars.map_or(0, |c| c as u64 + 1);
        (Self::size_with(len, word), word)
    }

    /// Number of index entries for a string of `len` bytes whose `chars` word is `word`.
    fn index_entries(len: usize, word: u64) -> usize {
        let word = word & !Self::INDEX_READY;
        if len < Self::INDEX_MIN || len > u32::MAX as usize || word == 0 || (word - 1) as usize == len {
            return 0;
        }
        ((word - 1) as usize).div_ceil(Self::STRIDE)
    }

    fn size_with(len: usize, word: u64) -> usize {
        match Self::index_entries(len, word) {
            0 => Self::BASE + len,
            n => Self::index_offset(len) + 4 * n,
        }
    }

    /// Total bytes the object occupies (header, bytes and index padding included).
    pub fn alloc_size(&self) -> usize {
        Self::size_with(self.len, self.chars.load(Ordering::Relaxed))
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: strings are created from valid UTF-8 and never mutated.
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.data.as_ptr(), self.len)) }
    }
    pub fn char_count(&self) -> usize {
        let c = self.chars.load(Ordering::Relaxed) & !Self::INDEX_READY;
        if c != 0 {
            return (c - 1) as usize;
        }
        let s = self.as_str();
        let n = if s.is_ascii() { s.len() } else { s.chars().count() };
        // Only strings shorter than INDEX_MIN get here, and they have no index bit to lose.
        self.chars.store(n as u64 + 1, Ordering::Relaxed);
        n
    }
    /// Whether every character is a single byte. O(1) once the char count is known.
    #[inline]
    pub fn is_ascii(&self) -> bool {
        self.char_count() == self.len
    }

    /// The index slots, if this string has an index.
    fn index(&self) -> Option<&[AtomicU32]> {
        let n = Self::index_entries(self.len, self.chars.load(Ordering::Relaxed));
        if n == 0 {
            return None;
        }
        // SAFETY: the allocation reserved `n` slots right after the (4-aligned) bytes; they
        // start zeroed and are only ever accessed atomically.
        unsafe {
            let base = (self as *const StrObj as *const u8).add(Self::index_offset(self.len));
            Some(std::slice::from_raw_parts(base as *const AtomicU32, n))
        }
    }

    fn index_offset(len: usize) -> usize {
        (Self::BASE + len).next_multiple_of(4)
    }

    /// The filled-in index. Racing builders store identical values.
    fn ready_index(&self) -> Option<&[AtomicU32]> {
        let ix = self.index()?;
        if self.chars.load(Ordering::Acquire) & Self::INDEX_READY == 0 {
            for (slot, (b, _)) in ix.iter().zip(self.as_str().char_indices().step_by(Self::STRIDE)) {
                slot.store(b as u32, Ordering::Relaxed);
            }
            self.chars.fetch_or(Self::INDEX_READY, Ordering::Release);
        }
        Some(ix)
    }

    /// The byte offset of character `i` (`i` may equal the char count, giving the length).
    ///
    /// O(1) for ASCII strings, O([`StrObj::STRIDE`]) for indexed ones, O(n) for short ones.
    pub fn byte_offset(&self, i: usize) -> usize {
        let n = self.char_count();
        if n == self.len || i == 0 {
            return i.min(self.len);
        }
        if i >= n {
            return self.len;
        }
        let bytes = self.as_str().as_bytes();
        let (mut b, mut skip) = match self.ready_index() {
            Some(ix) => (ix[i / Self::STRIDE].load(Ordering::Relaxed) as usize, i % Self::STRIDE),
            None => (0, i),
        };
        while skip > 0 {
            b += 1;
            while bytes[b] & 0xC0 == 0x80 {
                b += 1;
            }
            skip -= 1;
        }
        b
    }

    /// The character position of byte offset `b`, which must be a char boundary.
    pub fn char_offset(&self, b: usize) -> usize {
        if self.is_ascii() {
            return b;
        }
        let s = self.as_str();
        let (c0, b0) = match self.ready_index() {
            Some(ix) => {
                // The last entry at or before `b`.
                let k = ix.partition_point(|x| x.load(Ordering::Relaxed) as usize <= b).max(1) - 1;
                (k * Self::STRIDE, ix[k].load(Ordering::Relaxed) as usize)
            }
            None => (0, 0),
        };
        c0 + s[b0..b].chars().count()
    }

    /// The `i`th character (`i < char_count()`).
    pub fn char_at(&self, i: usize) -> &str {
        let b = self.byte_offset(i);
        let s = self.as_str();
        let w = if self.is_ascii() { 1 } else { s[b..].chars().next().map_or(0, char::len_utf8) };
        &s[b..b + w]
    }

    /// The characters `[start, end)` (clamped to the string).
    pub fn char_slice(&self, start: usize, end: usize) -> &str {
        let n = self.char_count();
        let (start, end) = (start.min(n), end.min(n));
        if end <= start {
            return "";
        }
        let a = self.byte_offset(start);
        &self.as_str()[a..self.byte_offset(end)]
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

/// A list: header, length, a pointer to a backing array, and room for a few elements inline.
///
/// Reads never lock, even on shared lists. The length and buffer pointer are atomic words, and a
/// reader bounds-checks against the buffer it loaded, so it sees some consistent-enough state and
/// never reads out of bounds. A writer (the owner, or a thread holding a shared list's lock) that
/// outgrows the buffer copies it into a new one and swaps the pointer; the old buffer is retired
/// to the collector, which frees it only once the world is stopped and no reader can hold it.
///
/// Small lists keep their elements in the object itself: `inline` is a [`WordBuf`] embedded at
/// the end of the object (its slots follow the struct), and `buf` points at it. Readers do not
/// tell the two cases apart; they load `buf` as always. A list that outgrows its inline slots
/// moves to an out-of-line buffer exactly as above, and never moves back, so the inline slots are
/// never written again. A racing reader that loaded the inline pointer before the move reads
/// values the list held then, from memory that lives as long as the list does, so the inline
/// buffer needs no retiring. It is never freed on its own: it goes with the object.
#[repr(C)]
pub struct ListObj {
    pub header: Header,
    len: AtomicUsize,
    buf: AtomicPtr<WordBuf>,
    /// The inline buffer's capacity word; its slots follow the struct. Capacity 0 for lists
    /// created out of line.
    inline: WordBuf,
}

// Size classes are 16-byte steps; the inline capacities chosen below (odd) fill the object's
// slot exactly only if the struct ends half way through a step.
const _: () = assert!(std::mem::size_of::<ListObj>() % 16 == 8 && ListObj::INLINE_MAX % 2 == 1);

impl ListObj {
    /// Most elements a new list keeps inline (a 96-byte object). Odd, so the object fills its
    /// size class. Chosen by measurement (issue #104).
    pub const INLINE_MAX: usize = 7;
    /// Inline capacity of a new empty (or one-element) list.
    pub const INLINE_MIN: usize = 3;

    /// The inline capacity of a new list that should have room for `cap` elements, or 0 if
    /// they belong out of line. Rounded up to odd: an even count would leave a word of the
    /// size-class slot unused.
    #[inline]
    pub fn inline_cap_for(cap: usize) -> usize {
        if cap > Self::INLINE_MAX {
            0
        } else {
            cap.max(Self::INLINE_MIN) | 1
        }
    }

    /// The allocation size of a list with inline capacity `icap`.
    #[inline]
    pub fn size_for(icap: usize) -> usize {
        std::mem::size_of::<ListObj>() + 8 * icap
    }

    /// # Safety
    /// `p` must point to freshly allocated, unpublished list memory of
    /// [`ListObj::size_for`]`(icap)` bytes.
    pub(crate) unsafe fn init(p: *mut ListObj, items: Vec<Value>, icap: usize) {
        let len = items.len();
        if icap >= len && icap > 0 {
            // Small: copy into the object and give the vector's allocation back.
            Self::init_inline(p, items.iter().map(|v| v.0), icap, len);
            return;
        }
        Self::init_inline(p, std::iter::empty(), 0, 0);
        // `Value` is a transparent `u64`, so the vector's allocation is reused as is.
        let mut items = std::mem::ManuallyDrop::new(items);
        let words = Vec::from_raw_parts(items.as_mut_ptr() as *mut u64, len, items.capacity());
        Self::init_buf(p, WordBuf::from_vec(words, Value::NIL.0), len);
    }

    /// As [`ListObj::init`], copying `items` into a buffer of `cap` (at least `items.len()`):
    /// the inline one if `icap` is at least `cap`.
    ///
    /// # Safety
    /// As for [`ListObj::init`].
    pub(crate) unsafe fn init_from_slice(p: *mut ListObj, items: &[Value], cap: usize, icap: usize) {
        let cap = cap.max(items.len());
        if icap >= cap && icap > 0 {
            Self::init_inline(p, items.iter().map(|v| v.0), icap, items.len());
            return;
        }
        Self::init_inline(p, std::iter::empty(), 0, 0);
        let buf = WordBuf::alloc_from(cap, items.iter().map(|v| v.0), Value::NIL.0);
        Self::init_buf(p, buf, items.len());
    }

    /// Writes the inline buffer (`icap` slots holding `items`, then nil) and, if `icap > 0`,
    /// points the list at it.
    unsafe fn init_inline(p: *mut ListObj, items: impl IntoIterator<Item = u64>, icap: usize, len: usize) {
        let inline = std::ptr::addr_of_mut!((*p).inline);
        WordBuf::init_in_place(inline, icap, items, Value::NIL.0);
        if icap > 0 {
            Self::init_buf(p, inline, len);
        }
    }

    unsafe fn init_buf(p: *mut ListObj, buf: *mut WordBuf, len: usize) {
        std::ptr::addr_of_mut!((*p).len).write(AtomicUsize::new(len));
        std::ptr::addr_of_mut!((*p).buf).write(AtomicPtr::new(buf));
    }

    /// The address of the inline buffer, for comparison only.
    #[inline(always)]
    fn inline_addr(&self) -> *mut WordBuf {
        std::ptr::addr_of!(self.inline) as *mut WordBuf
    }

    /// Whether the elements are currently stored in the object itself.
    pub fn is_inline(&self) -> bool {
        self.buf.load(Ordering::Acquire) == self.inline_addr()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline(always)]
    fn slots(&self) -> &[AtomicU64] {
        unsafe { WordBuf::slots(self.buf.load(Ordering::Acquire)) }
    }

    /// The item at `i`, if in bounds. Safe against concurrent writers; the caller must still have
    /// passed the ownership read check.
    #[inline(always)]
    pub fn get(&self, i: usize) -> Option<Value> {
        if i >= self.len() {
            return None;
        }
        self.slots().get(i).map(|x| Value(x.load(Ordering::Acquire)))
    }

    /// As [`ListObj::get`] for a list owned by this thread or frozen, where nothing races.
    #[inline(always)]
    pub fn get_local(&self, i: usize) -> Option<Value> {
        if i >= self.len.load(Ordering::Relaxed) {
            return None;
        }
        let slots = unsafe { WordBuf::slots(self.buf.load(Ordering::Relaxed)) };
        slots.get(i).map(|x| Value(x.load(Ordering::Relaxed)))
    }

    /// A snapshot of the items.
    pub fn to_vec(&self) -> Vec<Value> {
        let n = self.len();
        let slots = self.slots();
        slots[..n.min(slots.len())].iter().map(|x| Value(x.load(Ordering::Acquire))).collect()
    }

    pub fn capacity(&self) -> usize {
        unsafe { WordBuf::cap(self.buf.load(Ordering::Acquire)) }
    }

    // Writers. Each requires write access per the ownership rules: the owning thread, or the
    // holder of a shared list's lock. `access` says which, so replaced buffers are freed at once
    // when no other thread can be reading them.

    /// # Safety
    /// See above.
    pub unsafe fn set(&self, i: usize, v: Value) -> bool {
        if i >= self.len.load(Ordering::Relaxed) {
            return false;
        }
        self.slots()[i].store(v.0, Ordering::Release);
        true
    }

    /// # Safety
    /// See above.
    pub unsafe fn push(&self, v: Value, access: Access) {
        let n = self.len.load(Ordering::Relaxed);
        if n == self.capacity() {
            self.grow(n + 1, access);
        }
        self.slots()[n].store(v.0, Ordering::Release);
        self.len.store(n + 1, Ordering::Release);
    }

    /// # Safety
    /// See above.
    pub unsafe fn pop(&self) -> Option<Value> {
        let n = self.len.load(Ordering::Relaxed);
        if n == 0 {
            return None;
        }
        let slot = &self.slots()[n - 1];
        let v = Value(slot.load(Ordering::Relaxed));
        self.len.store(n - 1, Ordering::Release);
        slot.store(Value::NIL.0, Ordering::Release);
        Some(v)
    }

    /// Replaces the contents with `items`, reusing the buffer when it is large enough.
    ///
    /// # Safety
    /// See above.
    pub unsafe fn replace(&self, items: &[Value], access: Access) {
        let old_len = self.len.load(Ordering::Relaxed);
        let n = items.len();
        if n > self.capacity() {
            let buf = WordBuf::alloc_from(n.max(4), items.iter().map(|v| v.0), Value::NIL.0);
            self.swap_buf(buf, access);
            self.len.store(n, Ordering::Release);
            return;
        }
        let slots = self.slots();
        if n < old_len {
            self.len.store(n, Ordering::Release);
        }
        for (s, v) in slots.iter().zip(items) {
            s.store(v.0, Ordering::Release);
        }
        for s in &slots[n..old_len.max(n)] {
            s.store(Value::NIL.0, Ordering::Release);
        }
        if n >= old_len {
            self.len.store(n, Ordering::Release);
        }
    }

    /// Runs `f` on a copy of the items and writes the result back.
    ///
    /// # Safety
    /// See above.
    pub unsafe fn modify<R>(&self, access: Access, f: impl FnOnce(&mut Vec<Value>) -> R) -> R {
        let mut items = self.to_vec();
        let r = f(&mut items);
        self.replace(&items, access);
        r
    }

    unsafe fn grow(&self, need: usize, access: Access) {
        let n = self.len.load(Ordering::Relaxed);
        let mut cap = need.max(self.capacity() * 2).max(4);
        if self.is_inline() {
            // Leaving the odd inline capacity: continue with powers of two, as a list that
            // started empty out of line would.
            cap = cap.next_power_of_two();
        }
        let old = &self.slots()[..n];
        let buf = WordBuf::alloc_from(cap, old.iter().map(|s| s.load(Ordering::Relaxed)), Value::NIL.0);
        self.swap_buf(buf, access);
    }

    unsafe fn swap_buf(&self, buf: *mut WordBuf, access: Access) {
        let old = self.buf.swap(buf, Ordering::AcqRel);
        // The inline buffer is part of the object: racing readers may still read it, and it
        // is freed with the list.
        if old != self.inline_addr() {
            crate::buf::release(old, access);
        }
    }
}

#[repr(C)]
pub struct DictObj {
    pub header: Header,
    pub table: DictTable,
}

impl DictObj {
    /// The table. Reads are safe under any race (see [`DictTable`]); writes need write access.
    #[inline(always)]
    pub fn table(&self) -> &DictTable {
        &self.table
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
    /// Wakers of fibers waiting to send or receive; all are woken on any change. Registered
    /// while holding `queue`, so a change made after the check cannot be missed.
    pub waiters: parking_lot::Mutex<Vec<Wake>>,
}

/// Wakes a parked fiber (see the standard library's fiber scheduler).
pub type Wake = std::sync::Arc<dyn Fn() + Send + Sync>;

impl ChannelInner {
    /// Wakes every fiber waiting on the channel.
    pub fn wake_fibers(&self) {
        let ws = std::mem::take(&mut *self.waiters.lock());
        for w in ws {
            w();
        }
    }
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
    (*(h as *const StrObj)).alloc_size()
}

unsafe fn fin_bigint(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut BigIntObj)).value);
}
unsafe fn size_bigint(h: *const Header) -> usize {
    std::mem::size_of::<BigIntObj>() + (*(h as *const BigIntObj)).value.bits() as usize / 8
}

unsafe fn trace_list(h: *const Header, t: &mut Tracer) {
    // The world is stopped; slots past the length are nil.
    for s in (*(h as *const ListObj)).slots() {
        mark_value(t, Value(s.load(Ordering::Relaxed)));
    }
}
unsafe fn fin_list(h: *mut Header) {
    let l = &*(h as *const ListObj);
    let buf = l.buf.load(Ordering::Relaxed);
    if buf != l.inline_addr() {
        WordBuf::free(buf as *mut u8);
    }
}
unsafe fn size_list(h: *const Header) -> usize {
    let l = &*(h as *const ListObj);
    let out = if l.is_inline() { 0 } else { l.capacity() };
    ListObj::size_for(WordBuf::cap(&l.inline)) + 8 * out
}

unsafe fn trace_dict(h: *const Header, t: &mut Tracer) {
    (*(h as *const DictObj)).table.trace(t);
}
unsafe fn fin_dict(h: *mut Header) {
    std::ptr::drop_in_place(&mut (*(h as *mut DictObj)).table);
}
unsafe fn size_dict(h: *const Header) -> usize {
    std::mem::size_of::<DictObj>() + (*(h as *const DictObj)).table.heap_bytes()
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
