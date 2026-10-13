//! Insertion-ordered open-addressing hash table (the CPython 3.6+ layout): a dense entry array in
//! insertion order plus a sparse index array.
//!
//! Key equality may require calling user code (`==` on a class), which could re-enter and mutate
//! the table. So lookups are split: [`DictTable::candidates`] lists entries whose hash matches and
//! whose keys are not bitwise-identical; the caller compares keys (possibly calling user code with
//! no borrow held) and then acts by entry index, rechecking [`DictTable::version`].
//!
//! `Dyn` records (parsed JSON objects) start in *shape mode*: their string keys live in a shared,
//! immutable [`Shape`] (a hidden class), reached through a transition tree from the empty shape,
//! and the table holds only the values. Objects parsed from the same kind of document share one
//! shape, so a `Dyn` member access can cache `(shape, slot)` per site. Deleting a key, adding a
//! non-string key, or growing past [`MAX_SHAPE_KEYS`] converts the table to the ordinary layout.

use std::sync::{Arc, LazyLock, Once};

use catallaxy_gc::{RootSource, Tracer};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::object::{mark_value, str_of};
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
    transitions: Mutex<FxHashMap<Box<str>, Arc<Shape>>>,
}

struct ShapeRegistry {
    count: usize,
    /// Every key held by any shape, for the GC.
    keys: Vec<Value>,
}

static REGISTRY: Mutex<ShapeRegistry> = Mutex::new(ShapeRegistry { count: 1, keys: Vec::new() });
static ROOT_SHAPE: LazyLock<Arc<Shape>> =
    LazyLock::new(|| Arc::new(Shape { id: 0, keys: Vec::new(), transitions: Mutex::new(FxHashMap::default()) }));
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

    /// The slot of the string key `k`, if present.
    #[inline]
    pub fn slot_of(&self, hash: u64, k: &str) -> Option<usize> {
        self.keys.iter().position(|&(h, key)| h == hash && str_of(key) == Some(k))
    }

    /// The shape with `key` appended, or `None` if the shape limit is reached.
    fn add(self: &Arc<Shape>, hash: u64, key: Value) -> Option<Arc<Shape>> {
        let k = str_of(key)?;
        let mut tr = self.transitions.lock();
        if let Some(next) = tr.get(k) {
            return Some(next.clone());
        }
        let mut reg = REGISTRY.lock();
        if reg.count >= MAX_SHAPES {
            return None;
        }
        SHAPE_ROOTS.call_once(|| catallaxy_gc::add_global_roots(Box::new(ShapeKeyRoots)));
        let id = reg.count as u32;
        reg.count += 1;
        reg.keys.push(key);
        let mut keys = self.keys.clone();
        keys.push((hash, key));
        let next = Arc::new(Shape { id, keys, transitions: Mutex::new(FxHashMap::default()) });
        tr.insert(k.into(), next.clone());
        Some(next)
    }
}

#[derive(Copy, Clone)]
pub struct Entry {
    pub hash: u64,
    pub key: Value,
    pub value: Value,
}

const EMPTY: u32 = u32::MAX;

#[derive(Default)]
pub struct DictTable {
    entries: Vec<Entry>,
    indices: Vec<u32>,
    live: usize,
    version: u64,
    /// Set in shape mode; then `values` holds the entries and `entries`/`indices` are empty.
    shape: Option<Arc<Shape>>,
    values: Vec<Value>,
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

impl DictTable {
    pub fn new() -> Self {
        DictTable::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        let mut t = DictTable::default();
        if n > 0 {
            t.entries.reserve(n);
            t.indices = vec![EMPTY; (n * 3 / 2).next_power_of_two().max(8)];
        }
        t
    }

    /// An empty table in shape mode, for `Dyn` records.
    pub fn record(n: usize) -> Self {
        DictTable { shape: Some(ROOT_SHAPE.clone()), values: Vec::with_capacity(n), ..DictTable::default() }
    }

    /// The hidden class, if the table is in shape mode.
    #[inline]
    pub fn shape(&self) -> Option<&Arc<Shape>> {
        self.shape.as_ref()
    }

    /// The value in `slot` of a shape-mode table.
    #[inline]
    pub fn slot_value(&self, slot: usize) -> Option<Value> {
        self.values.get(slot).copied()
    }

    /// Leaves shape mode, keeping entry indices (entry `i` is slot `i`).
    fn leave_shape_mode(&mut self) {
        let Some(shape) = self.shape.take() else { return };
        let values = std::mem::take(&mut self.values);
        let n = values.len();
        self.entries = Vec::with_capacity(n + 1);
        self.indices = vec![EMPTY; ((n + 1) * 3).next_power_of_two().max(8)];
        for (i, (&(hash, key), value)) in shape.keys.iter().zip(values).enumerate() {
            self.entries.push(Entry { hash, key, value });
            self.place(hash, i as u32);
        }
        crate::note_external(self.heap_bytes());
    }

    pub fn len(&self) -> usize {
        self.live
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn heap_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>() + self.indices.capacity() * 4 + self.values.capacity() * 8
    }

    pub fn trace(&self, t: &mut Tracer) {
        // Shape keys are global roots.
        for &v in &self.values {
            mark_value(t, v);
        }
        for e in &self.entries {
            if e.key != Value::TOMBSTONE {
                mark_value(t, e.key);
                mark_value(t, e.value);
            }
        }
    }

    /// Live entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = Entry> + '_ {
        (0..self.raw_len()).filter_map(|i| self.raw_entry(i))
    }

    pub fn entry(&self, idx: usize) -> Entry {
        match &self.shape {
            Some(shape) => {
                let (hash, key) = shape.keys[idx];
                Entry { hash, key, value: self.values[idx] }
            }
            None => self.entries[idx],
        }
    }

    pub fn set_value_at(&mut self, idx: usize, v: Value) {
        match self.shape {
            Some(_) => self.values[idx] = v,
            None => self.entries[idx].value = v,
        }
    }

    /// Entry index of the n-th live entry (for indexed iteration that tolerates growth).
    pub fn raw_len(&self) -> usize {
        match self.shape {
            Some(_) => self.values.len(),
            None => self.entries.len(),
        }
    }
    pub fn raw_entry(&self, i: usize) -> Option<Entry> {
        match self.shape {
            Some(_) => (i < self.values.len()).then(|| self.entry(i)),
            None => self.entries.get(i).filter(|e| e.key != Value::TOMBSTONE).copied(),
        }
    }

    pub fn probe(&self, hash: u64, key: Value) -> Probe {
        if let Some(shape) = &self.shape {
            // Shape keys are all strings, and strings equal only strings.
            return match str_of(key).and_then(|k| shape.slot_of(hash, k)) {
                Some(i) => Probe::Found(i),
                None => Probe::Absent,
            };
        }
        if self.indices.is_empty() {
            return Probe::Absent;
        }
        let mask = self.indices.len() - 1;
        let mut i = (hash as usize) & mask;
        let mut perturb = hash;
        let mut check = Vec::new();
        loop {
            let ix = self.indices[i];
            if ix == EMPTY {
                return if check.is_empty() { Probe::Absent } else { Probe::Check(check) };
            }
            let e = &self.entries[ix as usize];
            if e.key != Value::TOMBSTONE && e.hash == hash {
                if e.key == key {
                    return Probe::Found(ix as usize);
                }
                check.push(ix as usize);
            }
            perturb >>= 5;
            i = (i.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize)) & mask;
        }
    }

    /// Appends a new entry (the caller has established the key is absent).
    pub fn insert_new(&mut self, hash: u64, key: Value, value: Value) {
        if let Some(shape) = &self.shape {
            if shape.len() < MAX_SHAPE_KEYS {
                if let Some(next) = shape.add(hash, key) {
                    self.shape = Some(next);
                    self.values.push(value);
                    self.live += 1;
                    self.version += 1;
                    return;
                }
            }
            self.leave_shape_mode();
        }
        if (self.entries.len() + 1) * 3 >= self.indices.len() * 2 {
            self.grow();
        }
        let idx = self.entries.len() as u32;
        self.entries.push(Entry { hash, key, value });
        self.place(hash, idx);
        self.live += 1;
        self.version += 1;
    }

    fn place(&mut self, hash: u64, idx: u32) {
        let mask = self.indices.len() - 1;
        let mut i = (hash as usize) & mask;
        let mut perturb = hash;
        while self.indices[i] != EMPTY {
            perturb >>= 5;
            i = (i.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize)) & mask;
        }
        self.indices[i] = idx;
    }

    fn grow(&mut self) {
        // Compact tombstones and rebuild the index.
        if self.live < self.entries.len() {
            self.entries.retain(|e| e.key != Value::TOMBSTONE);
        }
        let want = ((self.live + 1) * 3).next_power_of_two().max(8);
        self.indices = vec![EMPTY; want];
        for i in 0..self.entries.len() {
            let h = self.entries[i].hash;
            self.place(h, i as u32);
        }
        crate::note_external(want * 4);
    }

    /// Removes the entry at `idx`, returning its value.
    pub fn remove_at(&mut self, idx: usize) -> Value {
        self.leave_shape_mode();
        let e = &mut self.entries[idx];
        let v = e.value;
        e.key = Value::TOMBSTONE;
        e.value = Value::NIL;
        self.live -= 1;
        self.version += 1;
        // The index slot keeps pointing at the tombstone so probe chains stay intact.
        v
    }

    pub fn clear(&mut self) {
        if self.shape.is_some() {
            self.shape = Some(ROOT_SHAPE.clone());
        }
        self.values.clear();
        self.entries.clear();
        self.indices.clear();
        self.live = 0;
        self.version += 1;
    }

    pub fn first_live(&self) -> Option<usize> {
        if self.shape.is_some() {
            return (!self.values.is_empty()).then_some(0);
        }
        self.entries.iter().position(|e| e.key != Value::TOMBSTONE)
    }
}

impl Clone for DictTable {
    fn clone(&self) -> Self {
        if let Some(shape) = &self.shape {
            return DictTable {
                shape: Some(shape.clone()),
                values: self.values.clone(),
                live: self.live,
                ..DictTable::default()
            };
        }
        let mut t = DictTable::with_capacity(self.live);
        for e in self.iter() {
            t.insert_new(e.hash, e.key, e.value);
        }
        t
    }
}
