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
    buf: AtomicPtr<WordBuf>,
    /// Just before `inline`: where an out-of-line buffer keeps its written range, the inline
    /// buffer has the length, which reads as the range `0..len` (see `note_written`).
    len: AtomicUsize,
    /// The inline buffer's capacity word; its slots follow the struct. Capacity 0 for lists
    /// created out of line.
    inline: WordBuf,
}

const _: () = assert!(std::mem::offset_of!(ListObj, inline) == std::mem::offset_of!(ListObj, len) + 8);

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
        if let Err(layout) = Self::try_init(p, items, icap) {
            std::alloc::handle_alloc_error(layout);
        }
    }

    /// As [`ListObj::init`], but if the buffer cannot be had, frees `items`, leaves an empty
    /// list and returns the layout the allocator refused.
    ///
    /// # Safety
    /// As for [`ListObj::init`].
    pub(crate) unsafe fn try_init(p: *mut ListObj, items: Vec<Value>, icap: usize) -> Result<(), std::alloc::Layout> {
        let len = items.len();
        if icap >= len && icap > 0 {
            // Small: copy into the object and give the vector's allocation back.
            Self::init_inline(p, items.iter().map(|v| v.0), icap, len);
            return Ok(());
        }
        Self::init_inline(p, std::iter::empty(), 0, 0);
        // `Value` is a transparent `u64`, so the vector's allocation is reused as is.
        let mut items = std::mem::ManuallyDrop::new(items);
        let words = Vec::from_raw_parts(items.as_mut_ptr() as *mut u64, len, items.capacity());
        match WordBuf::try_from_vec(words, Value::NIL.0) {
            Ok(buf) => Self::init_buf(p, buf, len),
            Err(layout) => {
                Self::init_buf(p, WordBuf::empty(), 0);
                return Err(layout);
            }
        }
        Ok(())
    }

    /// As [`ListObj::init`], copying `items` into a buffer of `cap` (at least `items.len()`):
    /// the inline one if `icap` is at least `cap`.
    ///
    /// # Safety
    /// As for [`ListObj::init`].
    pub(crate) unsafe fn init_from_slice(p: *mut ListObj, items: &[Value], cap: usize, icap: usize) {
        if let Err(layout) = Self::try_init_from_slice(p, items, cap, icap) {
            std::alloc::handle_alloc_error(layout);
        }
    }

    /// As [`ListObj::init_from_slice`], but if the buffer cannot be had, leaves an empty list
    /// and returns the layout the allocator refused.
    ///
    /// # Safety
    /// As for [`ListObj::init`].
    pub(crate) unsafe fn try_init_from_slice(
        p: *mut ListObj,
        items: &[Value],
        cap: usize,
        icap: usize,
    ) -> Result<(), std::alloc::Layout> {
        let cap = cap.max(items.len());
        if icap >= cap && icap > 0 {
            Self::init_inline(p, items.iter().map(|v| v.0), icap, items.len());
            return Ok(());
        }
        Self::init_inline(p, std::iter::empty(), 0, 0);
        match WordBuf::try_alloc_from(cap, items.iter().map(|v| v.0), Value::NIL.0) {
            Ok(buf) => Self::init_buf(p, buf, items.len()),
            Err(layout) => {
                Self::init_buf(p, WordBuf::empty(), 0);
                return Err(layout);
            }
        }
        Ok(())
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

    /// As [`ListObj::to_vec`], raising a `MemoryError` if the vector cannot be had.
    pub fn try_to_vec(&self) -> crate::RtResult<Vec<Value>> {
        let n = self.len();
        let slots = self.slots();
        let items = &slots[..n.min(slots.len())];
        let mut v = crate::try_vec(items.len())?;
        v.extend(items.iter().map(|x| Value(x.load(Ordering::Acquire))));
        Ok(v)
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
        let buf = self.buf.load(Ordering::Relaxed);
        WordBuf::slots(buf)[i].store(v.0, Ordering::Release);
        self.note_store(buf, i, v);
        true
    }

    /// The slots a minor collection would scan if the list were remembered: all of them when
    /// inline, else those recorded as written since the last one. For tests.
    #[doc(hidden)]
    pub fn written_range(&self) -> std::ops::Range<usize> {
        let buf = self.buf.load(Ordering::Acquire);
        if buf == self.inline_addr() {
            return 0..self.len();
        }
        // SAFETY: an out-of-line buffer is allocated by `WordBuf`.
        unsafe { WordBuf::written(buf) }
    }

    /// Records slots `lo..hi` of `buf`, the current buffer, as written (see
    /// [`WordBuf::note_written`]).
    ///
    /// A young list records nothing: its buffer's range covers every slot (a new buffer's does,
    /// and only a minor collection of an old list narrows it), so the common case costs one test
    /// of a header bit. Nor does the inline buffer: it is small enough that a minor collection
    /// scans it whole. It has no range word, but the word before it is the length, which reads as
    /// the range `0..len`; so storing into an existing element passes the check below without
    /// telling the two cases apart, and only the slow path does.
    #[inline(always)]
    unsafe fn note_written(&self, buf: *mut WordBuf, lo: usize, hi: usize) {
        if !self.header.is_young() && !WordBuf::range_covers(WordBuf::range_word(buf), lo, hi) {
            self.widen_written(buf, lo, hi);
        }
    }

    /// Records slot `i` of `buf`, the current buffer, as written with `v`. The range only has to
    /// cover slots that may hold a young reference, so storing a number or an old object records
    /// nothing (a slot that held a young reference before is still in the range).
    #[inline(always)]
    unsafe fn note_store(&self, buf: *mut WordBuf, i: usize, v: Value) {
        if v.header().is_some_and(|h| h.is_young())
            && !self.header.is_young()
            && !WordBuf::range_covers(WordBuf::range_word(buf), i, i + 1)
        {
            self.widen_written(buf, i, i + 1);
        }
    }

    #[cold]
    #[inline(never)]
    unsafe fn widen_written(&self, buf: *mut WordBuf, lo: usize, hi: usize) {
        if buf != self.inline_addr() {
            WordBuf::widen_written(buf, lo, hi);
        }
    }

    /// # Safety
    /// See above.
    pub unsafe fn push(&self, v: Value, access: Access) {
        if let Err(layout) = self.try_push(v, access) {
            std::alloc::handle_alloc_error(layout);
        }
    }

    /// As [`ListObj::push`], but if the allocator refuses to grow the list, returns the
    /// refused layout and leaves the list as it was.
    ///
    /// # Safety
    /// See above.
    pub unsafe fn try_push(&self, v: Value, access: Access) -> Result<(), std::alloc::Layout> {
        let n = self.len.load(Ordering::Relaxed);
        let mut buf = self.buf.load(Ordering::Relaxed);
        if n == WordBuf::cap(buf) {
            self.grow(n + 1, access)?;
            buf = self.buf.load(Ordering::Relaxed);
        }
        WordBuf::slots(buf)[n].store(v.0, Ordering::Release);
        self.note_store(buf, n, v);
        self.len.store(n + 1, Ordering::Release);
        Ok(())
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
        if let Err(layout) = self.try_replace(items, access) {
            std::alloc::handle_alloc_error(layout);
        }
    }

    /// As [`ListObj::replace`], but if the allocator refuses a larger buffer, returns the
    /// refused layout and leaves the list as it was.
    ///
    /// # Safety
    /// See above.
    pub unsafe fn try_replace(&self, items: &[Value], access: Access) -> Result<(), std::alloc::Layout> {
        let old_len = self.len.load(Ordering::Relaxed);
        let n = items.len();
        if n > self.capacity() {
            // A new buffer counts as written throughout.
            let buf = WordBuf::try_alloc_from(n.max(4), items.iter().map(|v| v.0), Value::NIL.0)?;
            self.swap_buf(buf, access);
            self.len.store(n, Ordering::Release);
            return Ok(());
        }
        let buf = self.buf.load(Ordering::Relaxed);
        if n > 0 {
            self.note_written(buf, 0, n);
        }
        let slots = WordBuf::slots(buf);
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
        Ok(())
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

    /// Replaces the `remove` items starting at `at` with `insert`, editing the buffer in place
    /// (growing it first if it is too small): the items after them move by memmove when no other
    /// thread can be reading the list, else one atomic store at a time. Only the slots whose
    /// contents changed are recorded as written. `at + remove` must not exceed the length.
    ///
    /// Lock-free readers of a shared list see, as with every other in-place write, each slot
    /// holding either its old or its new value, and never a slot past the length they loaded:
    /// a longer length is published after the slots it covers, a shorter one before the slots
    /// past it are cleared.
    ///
    /// If the allocator refuses a larger buffer, returns the refused layout and leaves the list
    /// as it was.
    ///
    /// # Safety
    /// See above.
    pub unsafe fn try_splice(
        &self,
        at: usize,
        remove: usize,
        insert: &[Value],
        access: Access,
    ) -> Result<(), std::alloc::Layout> {
        let n = self.len.load(Ordering::Relaxed);
        assert!(at <= n && remove <= n - at, "splice out of bounds");
        let ins = insert.len();
        let Some(new_n) = (n - remove).checked_add(ins) else {
            return Err(std::alloc::Layout::new::<[u64; 2]>());
        };
        if new_n > WordBuf::cap(self.buf.load(Ordering::Relaxed)) {
            self.grow(new_n, access)?;
        }
        let buf = self.buf.load(Ordering::Relaxed);
        let slots = WordBuf::slots(buf);
        let (tail, tail_len) = (at + remove, n - at - remove);
        // Written: the inserted items, and the moved tail.
        let hi = if ins != remove && tail_len > 0 { new_n } else { at + ins };
        if at < hi {
            self.note_written(buf, at, hi);
        }
        if new_n < n {
            self.len.store(new_n, Ordering::Release);
        }
        if ins != remove {
            move_slots(slots, tail, at + ins, tail_len, access);
        }
        for (s, v) in slots[at..at + ins].iter().zip(insert) {
            s.store(v.0, Ordering::Release);
        }
        if new_n < n {
            for s in &slots[new_n..n] {
                s.store(Value::NIL.0, Ordering::Release);
            }
        } else if new_n > n {
            self.len.store(new_n, Ordering::Release);
        }
        Ok(())
    }

    /// Reverses the items in place.
    ///
    /// # Safety
    /// See above.
    pub unsafe fn reverse(&self) {
        let n = self.len.load(Ordering::Relaxed);
        if n < 2 {
            return;
        }
        let buf = self.buf.load(Ordering::Relaxed);
        self.note_written(buf, 0, n);
        let slots = &WordBuf::slots(buf)[..n];
        let (lo, hi) = slots.split_at(n / 2);
        for (a, b) in lo.iter().zip(hi.iter().rev()) {
            let (x, y) = (a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
            a.store(y, Ordering::Release);
            b.store(x, Ordering::Release);
        }
    }

    /// Moves the items to a buffer with room for `need`, or returns the layout the allocator
    /// refused, leaving the list as it was.
    unsafe fn grow(&self, need: usize, access: Access) -> Result<(), std::alloc::Layout> {
        let n = self.len.load(Ordering::Relaxed);
        let mut cap = need.max(self.capacity() * 2).max(4);
        if self.is_inline() {
            // Leaving the odd inline capacity: continue with powers of two, as a list that
            // started empty out of line would.
            cap = cap.next_power_of_two();
        }
        let old_buf = self.buf.load(Ordering::Relaxed);
        let old = &WordBuf::slots(old_buf)[..n];
        let buf = WordBuf::try_alloc_from(cap, old.iter().map(|s| s.load(Ordering::Relaxed)), Value::NIL.0)?;
        // The items keep their indices, so they keep their written range. Items that were
        // inline count as written throughout (the new buffer's initial range).
        if old_buf != self.inline_addr() {
            WordBuf::copy_written(old_buf, buf);
        }
        self.swap_buf(buf, access);
        Ok(())
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

/// Moves `count` slots from `from` to `to` (the ranges may overlap). With local access no other
/// thread can be reading the slots, so this is a memmove; otherwise each slot is moved with an
/// atomic store, in the order that never loses an item (from the end when moving right).
unsafe fn move_slots(slots: &[AtomicU64], from: usize, to: usize, count: usize, access: Access) {
    if count == 0 || from == to {
        return;
    }
    assert!(from.max(to) + count <= slots.len());
    if access == Access::Local {
        // SAFETY: in bounds (checked above); `AtomicU64` has the layout of `u64` and is a cell,
        // so the slots may be written through a pointer derived from a shared reference, and no
        // other thread accesses them while this thread has local write access.
        let base = slots.as_ptr() as *mut u64;
        std::ptr::copy(base.add(from), base.add(to), count);
    } else if to < from {
        for i in 0..count {
            slots[to + i].store(slots[from + i].load(Ordering::Relaxed), Ordering::Release);
        }
    } else {
        for i in (0..count).rev() {
            slots[to + i].store(slots[from + i].load(Ordering::Relaxed), Ordering::Release);
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
    /// The number of characters in `buf`, kept up to date by every mutation (under the same
    /// write access) so that `length` is O(1).
    pub chars: UnsafeCell<usize>,
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
    // No thread is writing the list (the world is stopped, or a minor collection of its owner
    // runs); slots past the length are nil.
    let l = &*(h as *const ListObj);
    let slots = WordBuf::slots(l.buf.load(Ordering::Relaxed));
    for s in &slots[..l.len.load(Ordering::Relaxed).min(slots.len())] {
        mark_value(t, Value(s.load(Ordering::Relaxed)));
    }
}
/// A minor collection of the owner, for an old list it wrote since the last one: only the slots
/// recorded as written.
unsafe fn trace_list_written(h: *const Header, t: &mut Tracer) {
    let l = &*(h as *const ListObj);
    let buf = l.buf.load(Ordering::Relaxed);
    if buf == l.inline_addr() {
        return trace_list(h, t);
    }
    let slots = WordBuf::slots(buf);
    let r = WordBuf::take_written(buf);
    let n = l.len.load(Ordering::Relaxed).min(r.end);
    for s in &slots[r.start.min(n)..n] {
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
unsafe fn trace_dict_written(h: *const Header, t: &mut Tracer) {
    (*(h as *const DictObj)).table.trace_written(t);
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
    let trace_remembered: Option<unsafe fn(*const Header, &mut Tracer)> = match kind {
        ObjKind::List => Some(trace_list_written),
        ObjKind::Dict | ObjKind::Set => Some(trace_dict_written),
        _ => None,
    };
    let leaf =
        matches!(kind, ObjKind::Immediate | ObjKind::Str | ObjKind::BigInt | ObjKind::StringBuilder | ObjKind::Native);
    GcVTable { trace, finalize, size, trace_remembered, leaf }
}

// ---------------------------------------------------------------------------------------------
// Typed views.
//
// The `as_*` views are unchecked: the caller must have verified the object's kind just before
// (with `kind_of`, a class compare, or a match on the class's kind). The static types are not
// enough: a `Dyn` value or a hole in the checker can put a value of any type where another is
// expected. Natives and other code taking values from programs use the checked `*_of` views
// below, which raise a `TypeError` naming the expected and actual types instead.

macro_rules! view {
    ($fn:ident, $ty:ty) => {
        /// # Safety
        /// `v` must be a pointer to an object of this kind, checked by the caller (static types
        /// are not enough; see the checked `*_of` views).
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

/// A `TypeError` panic: `expected {want}, got {the class of v}`.
#[cold]
#[inline(never)]
pub fn type_error<T>(want: &str, v: Value) -> crate::RtResult<T> {
    crate::panic(format!("TypeError: expected {want}, got {}", crate::class::class_of(v).name))
}

macro_rules! checked_view {
    ($fn:ident, $view:ident, $ty:ty, $kind:pat, $name:expr) => {
        #[doc = concat!("The `", $name, "` behind `v`, or a `TypeError` panic if `v` is not one.")]
        #[inline(always)]
        pub fn $fn<'a>(v: Value) -> crate::RtResult<&'a $ty> {
            if matches!(kind_of(v), $kind) {
                // SAFETY: the kind was checked just above.
                Ok(unsafe { $view(v) })
            } else {
                type_error($name, v)
            }
        }
    };
}
checked_view!(str_obj_of, as_str_obj, StrObj, ObjKind::Str, "String");
checked_view!(list_of, as_list, ListObj, ObjKind::List, "List");
checked_view!(dict_of, as_dict, DictObj, ObjKind::Dict | ObjKind::Set, "Dict or Set");
checked_view!(tuple_of, as_tuple, TupleObj, ObjKind::Tuple, "Tuple");
checked_view!(sb_of, as_sb, StringBuilderObj, ObjKind::StringBuilder, "StringBuilder");
checked_view!(mutex_of, as_mutex, MutexObj, ObjKind::Mutex, "Mutex");
checked_view!(atomic_of, as_atomic, AtomicObj, ObjKind::Atomic, "Atomic");
checked_view!(channel_of, as_channel, ChannelObj, ObjKind::Channel, "Channel");
checked_view!(thread_of, as_thread, ThreadObj, ObjKind::Thread, "Thread");

/// The instance behind `v` if it is of class `class_id` exactly (builtin and prelude classes,
/// which have no subclasses), or a `TypeError` panic naming `want`.
#[inline(always)]
pub fn instance_of<'a>(v: Value, class_id: u32, want: &str) -> crate::RtResult<&'a InstanceObj> {
    if v.is_ptr() {
        let cls = unsafe { heap_class(v.as_ptr()) };
        if cls.id == class_id && cls.kind == ObjKind::Instance {
            // SAFETY: the kind was checked just above.
            return Ok(unsafe { as_instance(v) });
        }
    }
    type_error(want, v)
}

/// The instance behind `v` if it is an instance with a field `i` (of any class), else `None`.
/// Reading the field is then memory-safe; whether it is the expected class's field is up to the
/// caller.
#[inline(always)]
pub fn instance_with_field<'a>(v: Value, i: usize) -> Option<&'a InstanceObj> {
    if v.is_ptr() && i < unsafe { heap_class(v.as_ptr()) }.instance_fields as usize {
        // SAFETY: only instance classes have `instance_fields`, and this one has field `i`.
        return Some(unsafe { as_instance(v) });
    }
    None
}

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
