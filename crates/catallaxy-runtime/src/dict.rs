//! Insertion-ordered open-addressing hash table (the CPython 3.6+ layout): a dense entry array in
//! insertion order plus a sparse index array.
//!
//! Key equality may require calling user code (`==` on a class), which could re-enter and mutate
//! the table. So lookups are split: [`DictTable::candidates`] lists entries whose hash matches and
//! whose keys are not bitwise-identical; the caller compares keys (possibly calling user code with
//! no borrow held) and then acts by entry index, rechecking [`DictTable::version`].

use catallaxy_gc::Tracer;

use crate::object::mark_value;
use crate::value::Value;

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
        self.entries.capacity() * std::mem::size_of::<Entry>() + self.indices.capacity() * 4
    }

    pub fn trace(&self, t: &mut Tracer) {
        for e in &self.entries {
            if e.key != Value::TOMBSTONE {
                mark_value(t, e.key);
                mark_value(t, e.value);
            }
        }
    }

    /// Live entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.key != Value::TOMBSTONE)
    }

    pub fn entry(&self, idx: usize) -> &Entry {
        &self.entries[idx]
    }

    pub fn set_value_at(&mut self, idx: usize, v: Value) {
        self.entries[idx].value = v;
    }

    /// Entry index of the n-th live entry (for indexed iteration that tolerates growth).
    pub fn raw_len(&self) -> usize {
        self.entries.len()
    }
    pub fn raw_entry(&self, i: usize) -> Option<&Entry> {
        self.entries.get(i).filter(|e| e.key != Value::TOMBSTONE)
    }

    pub fn probe(&self, hash: u64, key: Value) -> Probe {
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
        self.entries.clear();
        self.indices.clear();
        self.live = 0;
        self.version += 1;
    }

    pub fn first_live(&self) -> Option<usize> {
        self.entries.iter().position(|e| e.key != Value::TOMBSTONE)
    }
}

impl Clone for DictTable {
    fn clone(&self) -> Self {
        let mut t = DictTable::with_capacity(self.live);
        for e in self.iter() {
            t.insert_new(e.hash, e.key, e.value);
        }
        t
    }
}
