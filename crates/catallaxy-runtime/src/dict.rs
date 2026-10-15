//! Insertion-ordered open-addressing hash table (the CPython 3.6+ layout): a dense entry array in
//! insertion order plus a sparse index array.
//!
//! Key equality may require calling user code (`==` on a class), which could re-enter and mutate
//! the table. So lookups are split: [`DictTable::probe`] lists entries whose hash matches and
//! whose keys are not bitwise-identical; the caller compares keys (possibly calling user code with
//! no borrow held) and then acts by entry index, rechecking [`DictTable::version`].
//!
//! **Concurrency.** Every word of the table is atomic and replaced arrays are retired to the
//! collector rather than freed, so a reader racing a writer can never fault; it can only see a
//! mix of old and new words. A sequence counter (odd while a write is in progress) turns that
//! into a seqlock: readers of a *shared* dict run optimistically and retry, or fall back to the
//! object's lock, if a write overlapped ([`DictTable::read_optimistic`]). Optimistic reads never
//! dereference a key, because the key's object may have been published by the overlapping write.
//!
//! **Hidden classes.** `Dyn` records (parsed JSON objects) start in *shape mode*: their string
//! keys live in a shared, immutable [`Shape`], reached through a transition tree from the empty
//! shape, and the table holds only the values. Objects parsed from the same kind of document share
//! one shape, so a `Dyn` member access can cache `(shape, slot)` per site. Deleting a key, adding
//! a non-string key, or growing past [`MAX_SHAPE_KEYS`] converts the table to the ordinary layout.

use std::alloc::Layout;
use std::sync::atomic::{fence, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Once};

use catallaxy_gc::{RootSource, Tracer};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::buf::{release, release_plain, WordBuf};
use crate::object::{mark_value, str_of};
use crate::ownership::Access;
use crate::value::Value;

/// Records with more keys than this use the ordinary table layout.
pub const MAX_SHAPE_KEYS: usize = 32;
/// Past this many shapes, new records use the ordinary layout (bounds memory for documents with
/// unbounded key sets).
const MAX_SHAPES: usize = 1 << 16;

/// A hidden class: an ordered list of string keys. Shapes are immutable and live forever; their
/// keys are frozen strings kept alive by a global root.
pub struct Shape {
    pub id: u32,
    keys: Vec<(u64, Value)>,
    transitions: Mutex<FxHashMap<Box<str>, &'static Shape>>,
}

struct ShapeRegistry {
    count: usize,
    /// Every key held by any shape, for the GC.
    keys: Vec<Value>,
}

static REGISTRY: Mutex<ShapeRegistry> = Mutex::new(ShapeRegistry { count: 1, keys: Vec::new() });
static ROOT_SHAPE: LazyLock<&'static Shape> =
    LazyLock::new(|| Box::leak(Box::new(Shape { id: 0, keys: Vec::new(), transitions: Mutex::default() })));
static SHAPE_ROOTS: Once = Once::new();

struct ShapeKeyRoots;
impl RootSource for ShapeKeyRoots {
    fn trace_roots(&self, t: &mut Tracer) {
        // The world is stopped, so no mutator holds the registry lock.
        for &k in REGISTRY.lock().keys.iter() {
            mark_value(t, k);
        }
    }
}

impl Shape {
    pub fn len(&self) -> usize {
        self.keys.len()
    }
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The shape with `key` appended, or `None` if `key` is not a string or the shape limit is
    /// reached.
    fn add(&'static self, hash: u64, key: Value) -> Option<&'static Shape> {
        let k = str_of(key)?;
        let mut tr = self.transitions.lock();
        if let Some(&next) = tr.get(k) {
            return Some(next);
        }
        let mut reg = REGISTRY.lock();
        if reg.count >= MAX_SHAPES {
            return None;
        }
        SHAPE_ROOTS.call_once(|| catallaxy_gc::add_global_roots(Box::new(ShapeKeyRoots)));
        // The registry is a global root, which minor collections do not scan.
        if let Some(h) = key.header() {
            h.tenure();
        }
        let id = reg.count as u32;
        reg.count += 1;
        reg.keys.push(key);
        let mut keys = self.keys.clone();
        keys.push((hash, key));
        let next: &'static Shape = Box::leak(Box::new(Shape { id, keys, transitions: Mutex::default() }));
        tr.insert(k.into(), next);
        Some(next)
    }
}

#[derive(Copy, Clone)]
pub struct Entry {
    pub hash: u64,
    pub key: Value,
    pub value: Value,
}

const TOMB: Entry = Entry { hash: 0, key: Value::TOMBSTONE, value: Value::NIL };

/// An empty index slot.
const EMPTY_IX: u64 = u64::MAX;
/// Words per entry in table mode: hash, key, value.
const W: usize = 3;

pub struct DictTable {
    /// Even when stable, odd while a write is in progress; advances by 2 per write. Doubles as
    /// the version that lookups recheck.
    seq: AtomicU64,
    live: AtomicUsize,
    /// Entries (or shape slots) in use, tombstones included.
    used: AtomicUsize,
    /// Table mode: `W` words per entry. Shape mode: one value per slot.
    entries: AtomicPtr<WordBuf>,
    /// Table mode only: entry indices, `EMPTY_IX` when free. Power-of-two capacity. A plain
    /// buffer (`WordBuf::try_alloc_plain`): indices are no references, so minor collections never
    /// scan it and it needs no written range.
    indices: AtomicPtr<WordBuf>,
    /// Set in shape mode.
    shape: AtomicPtr<Shape>,
}

/// Result of probing for a key.
pub enum Probe {
    /// An entry with a bitwise-identical key (always equal).
    Found(usize),
    /// No entry with this hash; nothing to compare.
    Absent,
    /// Entries with the same hash that need an equality check, in probe order.
    Check(Vec<usize>),
}

impl Default for DictTable {
    fn default() -> Self {
        DictTable::new()
    }
}

/// Whether `v` is a young object. Only slots written with one need recording for minor
/// collections (see [`WordBuf::note_written`]).
#[inline(always)]
fn young(v: Value) -> bool {
    v.header().is_some_and(|h| h.is_young())
}

fn index_cap(n: usize) -> usize {
    (n * 3 / 2 + 1).next_power_of_two().max(8)
}

impl DictTable {
    pub fn new() -> Self {
        DictTable {
            seq: AtomicU64::new(0),
            live: AtomicUsize::new(0),
            used: AtomicUsize::new(0),
            entries: AtomicPtr::new(WordBuf::empty()),
            indices: AtomicPtr::new(WordBuf::empty()),
            shape: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self::try_with_capacity(n).unwrap_or_else(|layout| std::alloc::handle_alloc_error(layout))
    }

    /// As [`DictTable::with_capacity`], but returns the layout the allocator refused instead
    /// of aborting.
    pub fn try_with_capacity(n: usize) -> Result<Self, Layout> {
        let t = DictTable::new();
        if n > 0 {
            // `n * W` (W = 3) fitting also keeps `index_cap(n)` from overflowing.
            let ents = n.checked_mul(W).ok_or_else(|| Layout::from_size_align(isize::MAX as usize & !7, 8).unwrap())?;
            t.entries.store(WordBuf::try_alloc(ents, Value::NIL.0)?, Ordering::Relaxed);
            // On failure, dropping `t` frees the entries.
            t.indices.store(WordBuf::try_alloc_plain(index_cap(n), EMPTY_IX)?, Ordering::Relaxed);
        }
        Ok(t)
    }

    /// An empty table in shape mode, for `Dyn` records.
    pub fn record(n: usize) -> Self {
        Self::try_record(n).unwrap_or_else(|layout| std::alloc::handle_alloc_error(layout))
    }

    /// As [`DictTable::record`], but returns the layout the allocator refused instead of
    /// aborting.
    pub fn try_record(n: usize) -> Result<Self, Layout> {
        let t = DictTable::new();
        t.entries.store(WordBuf::try_alloc(n, Value::NIL.0)?, Ordering::Relaxed);
        t.shape.store(*ROOT_SHAPE as *const Shape as *mut Shape, Ordering::Relaxed);
        Ok(t)
    }

    // ---- reads ------------------------------------------------------------------------------
    // Safe to call while another thread writes: every load is atomic and bounds-checked against
    // the buffer it came from. Under a racing write the answer may be wrong (and is discarded by
    // the seqlock), but never out of bounds.

    #[inline]
    fn ents(&self) -> &[AtomicU64] {
        unsafe { WordBuf::slots(self.entries.load(Ordering::Acquire)) }
    }

    /// The hidden class, if the table is in shape mode.
    #[inline]
    pub fn shape(&self) -> Option<&'static Shape> {
        let p = self.shape.load(Ordering::Acquire);
        (!p.is_null()).then(|| unsafe { &*p })
    }

    /// The value in `slot` of a shape-mode table.
    #[inline]
    pub fn slot_value(&self, slot: usize) -> Option<Value> {
        if slot >= self.used.load(Ordering::Relaxed) {
            return None;
        }
        self.ents().get(slot).map(|w| Value(w.load(Ordering::Relaxed)))
    }

    pub fn len(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn version(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    pub fn heap_bytes(&self) -> usize {
        unsafe {
            8 * (WordBuf::cap(self.entries.load(Ordering::Relaxed))
                + WordBuf::cap(self.indices.load(Ordering::Relaxed)))
        }
    }

    /// Called with the world stopped.
    pub fn trace(&self, t: &mut Tracer) {
        // Shape keys are global roots.
        for e in self.iter() {
            mark_value(t, e.key);
            mark_value(t, e.value);
        }
    }

    /// For a minor collection of the owner: marks the entries written since the last one (see
    /// [`WordBuf::note_written`]) and forgets them. Entry `i` covers words `i * W..i * W + W` of
    /// the entry buffer in table mode, word `i` in shape mode.
    pub fn trace_written(&self, t: &mut Tracer) {
        // SAFETY: the entry buffer is allocated by `WordBuf` (never in place), and the caller is
        // the owner's minor collection, so no other thread writes the table.
        let r = unsafe { WordBuf::take_written(self.entries.load(Ordering::Relaxed)) };
        let w = if self.shape().is_some() { 1 } else { W };
        for i in r.start / w..r.end.div_ceil(w).min(self.raw_len()) {
            if let Some(e) = self.raw_entry(i) {
                mark_value(t, e.key);
                mark_value(t, e.value);
            }
        }
    }

    /// The words of the entry buffer recorded as written since the last minor collection that
    /// scanned the table. For tests.
    #[doc(hidden)]
    pub fn written_range(&self) -> std::ops::Range<usize> {
        // SAFETY: the entry buffer is allocated by `WordBuf`.
        unsafe { WordBuf::written(self.entries.load(Ordering::Acquire)) }
    }

    /// Live entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = Entry> + '_ {
        (0..self.raw_len()).filter_map(|i| self.raw_entry(i))
    }

    /// The entry at `idx` (a tombstone if out of range).
    pub fn entry(&self, idx: usize) -> Entry {
        let ents = self.ents();
        match self.shape() {
            Some(shape) => match (shape.keys.get(idx), ents.get(idx)) {
                (Some(&(hash, key)), Some(v)) => Entry { hash, key, value: Value(v.load(Ordering::Relaxed)) },
                _ => TOMB,
            },
            None => match ents.get(idx * W..idx * W + W) {
                Some(w) => Entry {
                    hash: w[0].load(Ordering::Relaxed),
                    key: Value(w[1].load(Ordering::Relaxed)),
                    value: Value(w[2].load(Ordering::Relaxed)),
                },
                None => TOMB,
            },
        }
    }

    /// Number of entry slots in use, tombstones included (for indexed iteration that tolerates
    /// growth).
    pub fn raw_len(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    pub fn raw_entry(&self, i: usize) -> Option<Entry> {
        if i >= self.raw_len() {
            return None;
        }
        Some(self.entry(i)).filter(|e| e.key != Value::TOMBSTONE)
    }

    pub fn first_live(&self) -> Option<usize> {
        (0..self.raw_len()).find(|&i| self.raw_entry(i).is_some())
    }

    pub fn probe(&self, hash: u64, key: Value) -> Probe {
        let mut check = Vec::new();
        if let Some(shape) = self.shape() {
            let n = self.raw_len().min(shape.keys.len());
            for (i, &(h, k)) in shape.keys[..n].iter().enumerate() {
                if h == hash {
                    if k == key {
                        return Probe::Found(i);
                    }
                    check.push(i);
                }
            }
            return if check.is_empty() { Probe::Absent } else { Probe::Check(check) };
        }
        let idx = unsafe { WordBuf::slots(self.indices.load(Ordering::Acquire)) };
        if idx.is_empty() {
            return Probe::Absent;
        }
        let mask = idx.len() - 1;
        let mut i = (hash as usize) & mask;
        let mut perturb = hash;
        // Bounded so a reader racing a rebuild cannot loop forever.
        for _ in 0..idx.len() {
            let ix = idx[i].load(Ordering::Relaxed);
            if ix == EMPTY_IX {
                break;
            }
            let e = self.entry(ix as usize);
            if e.key != Value::TOMBSTONE && e.hash == hash {
                if e.key == key {
                    return Probe::Found(ix as usize);
                }
                check.push(ix as usize);
            }
            perturb >>= 5;
            i = (i.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize)) & mask;
        }
        if check.is_empty() {
            Probe::Absent
        } else {
            Probe::Check(check)
        }
    }

    /// Runs the read `f` without the lock, retrying while writes overlap it. `None` means it kept
    /// colliding with writers; the caller should take the lock. `f` must only read the table.
    pub fn read_optimistic<R>(&self, f: &impl Fn(&DictTable) -> R) -> Option<R> {
        for _ in 0..4 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let r = f(self);
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some(r);
            }
        }
        None
    }

    // ---- writes -----------------------------------------------------------------------------
    // Each requires write access per the ownership rules: the owning thread, or the holder of a
    // shared dict's lock. `access` says which.

    fn begin(&self, access: Access) -> u64 {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s + 1, Ordering::Relaxed);
        if access == Access::Shared {
            fence(Ordering::Release);
        }
        s
    }

    fn end(&self, s: u64) {
        self.seq.store(s + 2, Ordering::Release);
    }

    /// # Safety
    /// The caller has write access (see above).
    pub unsafe fn set_value_at(&self, idx: usize, v: Value, access: Access) {
        let s = self.begin(access);
        let slot = if self.shape().is_some() { idx } else { idx * W + 2 };
        let ents = self.entries.load(Ordering::Relaxed);
        if let Some(w) = WordBuf::slots(ents).get(slot) {
            w.store(v.0, Ordering::Relaxed);
            if young(v) {
                WordBuf::note_written(ents, slot, slot + 1);
            }
        }
        self.end(s);
    }

    /// Appends a new entry (the caller has established the key is absent). Aborts if the table
    /// cannot grow; see [`DictTable::try_insert_new`].
    ///
    /// # Safety
    /// The caller has write access.
    pub unsafe fn insert_new(&self, hash: u64, key: Value, value: Value, access: Access) {
        if let Err(layout) = self.try_insert_new(hash, key, value, access) {
            std::alloc::handle_alloc_error(layout);
        }
    }

    /// As [`DictTable::insert_new`], but if the allocator refuses to grow the table, returns
    /// the refused layout and leaves the table as it was.
    ///
    /// # Safety
    /// The caller has write access.
    pub unsafe fn try_insert_new(&self, hash: u64, key: Value, value: Value, access: Access) -> Result<(), Layout> {
        let s = self.begin(access);
        let r = self.insert_in_section(hash, key, value, access);
        self.end(s);
        r
    }

    /// The body of [`DictTable::try_insert_new`], inside its write section.
    unsafe fn insert_in_section(&self, hash: u64, key: Value, value: Value, access: Access) -> Result<(), Layout> {
        if let Some(shape) = self.shape() {
            if shape.len() < MAX_SHAPE_KEYS {
                if let Some(next) = shape.add(hash, key) {
                    let n = self.raw_len();
                    let cap = WordBuf::cap(self.entries.load(Ordering::Relaxed));
                    if n == cap {
                        let buf = WordBuf::try_alloc((cap * 2).max(4), Value::NIL.0)?;
                        let old = self.entries.load(Ordering::Relaxed);
                        for (d, w) in WordBuf::slots(buf).iter().zip(WordBuf::slots(old)) {
                            d.store(w.load(Ordering::Relaxed), Ordering::Relaxed);
                        }
                        // Same indices, so the same written range.
                        WordBuf::copy_written(old, buf);
                        release(self.entries.swap(buf, Ordering::AcqRel), access);
                    }
                    let ents = self.entries.load(Ordering::Relaxed);
                    WordBuf::slots(ents)[n].store(value.0, Ordering::Relaxed);
                    if young(value) {
                        WordBuf::note_written(ents, n, n + 1);
                    }
                    self.shape.store(next as *const Shape as *mut Shape, Ordering::Release);
                    self.used.store(n + 1, Ordering::Relaxed);
                    self.live.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
            self.try_leave_shape_mode(access)?;
        }
        let n = self.raw_len();
        let icap = WordBuf::cap(self.indices.load(Ordering::Relaxed));
        let ecap = WordBuf::cap(self.entries.load(Ordering::Relaxed)) / W;
        if (n + 1) * 3 >= icap * 2 || n == ecap {
            self.rebuild(self.len() + 1, access)?;
        }
        let n = self.raw_len();
        let ents = self.entries.load(Ordering::Relaxed);
        let w = &WordBuf::slots(ents)[n * W..n * W + W];
        w[0].store(hash, Ordering::Relaxed);
        w[1].store(key.0, Ordering::Relaxed);
        w[2].store(value.0, Ordering::Relaxed);
        if young(key) || young(value) {
            WordBuf::note_written(ents, n * W, n * W + W);
        }
        Self::place(WordBuf::slots(self.indices.load(Ordering::Relaxed)), hash, n);
        self.used.store(n + 1, Ordering::Relaxed);
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn place(idx: &[AtomicU64], hash: u64, at: usize) {
        let mask = idx.len() - 1;
        let mut i = (hash as usize) & mask;
        let mut perturb = hash;
        while idx[i].load(Ordering::Relaxed) != EMPTY_IX {
            perturb >>= 5;
            i = (i.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize)) & mask;
        }
        idx[i].store(at as u64, Ordering::Relaxed);
    }

    /// Rebuilds the table layout with room for `want` live entries, dropping tombstones.
    /// Inside a write section. If the allocator refuses, the table is unchanged.
    unsafe fn rebuild(&self, want: usize, access: Access) -> Result<(), Layout> {
        let live = self.len();
        let (ents, idx) = Self::alloc_pair((want.max(live) * 2).max(4) * W, index_cap(want.max(live) * 2))?;
        let (ew, iw) = (WordBuf::slots(ents), WordBuf::slots(idx));
        // Straight from the old buffers into the new ones (no copy of the entries in between,
        // which for a large table would be a large allocation of its own).
        let mut n = 0;
        for (i, e) in self.iter().enumerate() {
            ew[i * W].store(e.hash, Ordering::Relaxed);
            ew[i * W + 1].store(e.key.0, Ordering::Relaxed);
            ew[i * W + 2].store(e.value.0, Ordering::Relaxed);
            Self::place(iw, e.hash, i);
            n = i + 1;
        }
        release(self.entries.swap(ents, Ordering::AcqRel), access);
        release_plain(self.indices.swap(idx, Ordering::AcqRel), access);
        self.used.store(n, Ordering::Relaxed);
        Ok(())
    }

    /// New, unpublished entry and index buffers of these capacities, or neither.
    fn alloc_pair(ecap: usize, icap: usize) -> Result<(*mut WordBuf, *mut WordBuf), Layout> {
        let ents = WordBuf::try_alloc(ecap, Value::NIL.0)?;
        match WordBuf::try_alloc_plain(icap, EMPTY_IX) {
            Ok(idx) => Ok((ents, idx)),
            Err(layout) => {
                // SAFETY: just allocated and never published.
                unsafe { WordBuf::free(ents as *mut u8) };
                Err(layout)
            }
        }
    }

    /// Leaves shape mode, keeping entry indices (entry `i` is slot `i`). Inside a write section.
    unsafe fn leave_shape_mode(&self, access: Access) {
        if let Err(layout) = self.try_leave_shape_mode(access) {
            std::alloc::handle_alloc_error(layout);
        }
    }

    /// As [`DictTable::leave_shape_mode`]; if the allocator refuses, the table is unchanged.
    unsafe fn try_leave_shape_mode(&self, access: Access) -> Result<(), Layout> {
        if self.shape().is_none() {
            return Ok(());
        }
        let n = self.raw_len();
        let (ents, idx) = Self::alloc_pair((n + 1).max(4) * W, index_cap(n + 1))?;
        let (ew, iw) = (WordBuf::slots(ents), WordBuf::slots(idx));
        for i in 0..n {
            let e = self.entry(i);
            ew[i * W].store(e.hash, Ordering::Relaxed);
            ew[i * W + 1].store(e.key.0, Ordering::Relaxed);
            ew[i * W + 2].store(e.value.0, Ordering::Relaxed);
            Self::place(iw, e.hash, i);
        }
        release_plain(self.indices.swap(idx, Ordering::AcqRel), access);
        let old = self.entries.swap(ents, Ordering::AcqRel);
        self.shape.store(std::ptr::null_mut(), Ordering::Release);
        release(old, access);
        Ok(())
    }

    /// Removes the entry at `idx`, returning its value.
    ///
    /// # Safety
    /// The caller has write access.
    pub unsafe fn remove_at(&self, idx: usize, access: Access) -> Value {
        let s = self.begin(access);
        self.leave_shape_mode(access);
        let mut v = Value::NIL;
        if let Some(w) = self.ents().get(idx * W..idx * W + W) {
            v = Value(w[2].load(Ordering::Relaxed));
            // The index slot keeps pointing at the tombstone so probe chains stay intact.
            w[1].store(Value::TOMBSTONE.0, Ordering::Relaxed);
            w[2].store(Value::NIL.0, Ordering::Relaxed);
            self.live.fetch_sub(1, Ordering::Relaxed);
        }
        self.end(s);
        v
    }

    /// # Safety
    /// The caller has write access.
    pub unsafe fn clear(&self, access: Access) {
        let s = self.begin(access);
        if self.shape().is_some() {
            self.shape.store(*ROOT_SHAPE as *const Shape as *mut Shape, Ordering::Release);
        }
        self.used.store(0, Ordering::Relaxed);
        self.live.store(0, Ordering::Relaxed);
        release(self.entries.swap(WordBuf::empty(), Ordering::AcqRel), access);
        release_plain(self.indices.swap(WordBuf::empty(), Ordering::AcqRel), access);
        self.end(s);
    }
}

impl Drop for DictTable {
    fn drop(&mut self) {
        unsafe {
            WordBuf::free(*self.entries.get_mut() as *mut u8);
            WordBuf::free_plain(*self.indices.get_mut() as *mut u8);
        }
    }
}

impl Clone for DictTable {
    fn clone(&self) -> Self {
        self.try_clone().unwrap_or_else(|layout| std::alloc::handle_alloc_error(layout))
    }
}

impl DictTable {
    /// A copy of the table, or the layout the allocator refused.
    pub fn try_clone(&self) -> Result<Self, Layout> {
        let n = self.raw_len();
        if let Some(shape) = self.shape() {
            let t = DictTable::try_record(n)?;
            for (d, w) in t.ents().iter().zip(&self.ents()[..n.min(self.ents().len())]) {
                d.store(w.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            t.shape.store(shape as *const Shape as *mut Shape, Ordering::Relaxed);
            t.used.store(n, Ordering::Relaxed);
            t.live.store(self.len(), Ordering::Relaxed);
            return Ok(t);
        }
        let t = DictTable::try_with_capacity(self.len())?;
        for e in self.iter() {
            // The clone is unpublished: only this thread can see it.
            unsafe { t.try_insert_new(e.hash, e.key, e.value, Access::Local)? };
        }
        Ok(t)
    }
}
