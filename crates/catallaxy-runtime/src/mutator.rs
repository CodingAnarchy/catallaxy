//! Per-thread runtime context: identity, allocation, and the handle stack that roots values held
//! by native code across calls back into the interpreter.

use std::cell::UnsafeCell;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

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
    Unwind { frame: usize, kind: u8, value: Value },
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
    pub heap: Box<LocalHeap>,
    /// Values rooted by native code. Traced as roots.
    pub handles: UnsafeCell<Vec<Value>>,
}

/// An index into the handle stack, valid inside the [`Mutator::scope`] that created it.
#[derive(Copy, Clone)]
pub struct Handle(usize);

impl Mutator {
    /// Creates the context for an already-registered thread.
    pub fn new(rec: Arc<ThreadRecord>) -> Self {
        let heap = Box::new(LocalHeap::new(rec.tid));
        Mutator { tid: rec.tid, rec, heap, handles: UnsafeCell::new(Vec::new()) }
    }

    // ---- handles ------------------------------------------------------------------------

    /// Runs `f` with a handle scope; handles rooted inside are released afterwards.
    pub fn scope<R>(&mut self, f: impl FnOnce(&mut Mutator) -> R) -> R {
        let base = self.handles_len();
        let r = f(self);
        self.handles.get_mut().truncate(base);
        r
    }
    pub fn root(&mut self, v: Value) -> Handle {
        let hs = self.handles.get_mut();
        hs.push(v);
        Handle(hs.len() - 1)
    }
    pub fn get(&self, h: Handle) -> Value {
        unsafe { (&*self.handles.get())[h.0] }
    }
    pub fn set(&mut self, h: Handle, v: Value) {
        self.handles.get_mut()[h.0] = v;
    }
    fn handles_len(&self) -> usize {
        unsafe { (&*self.handles.get()).len() }
    }
    /// Root values for the GC (called while this thread is stopped).
    pub fn handle_values(&self) -> &[Value] {
        unsafe { &*self.handles.get() }
    }

    // ---- allocation ---------------------------------------------------------------------

    #[inline]
    pub fn alloc_raw(&mut self, size: usize, cls: &'static Class) -> *mut Header {
        self.heap.alloc(size, &cls.gc as *const _)
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
        crate::note_external(items.capacity() * 8);
        let h = self.alloc_raw(std::mem::size_of::<ListObj>(), BUILTINS.list);
        unsafe { std::ptr::write((*(h as *mut ListObj)).vec.get(), items) };
        Value::from_ptr(h)
    }

    pub fn alloc_dict(&mut self, table: DictTable) -> Value {
        self.alloc_table(table, BUILTINS.dict)
    }

    pub fn alloc_set(&mut self, table: DictTable) -> Value {
        self.alloc_table(table, BUILTINS.set)
    }

    fn alloc_table(&mut self, table: DictTable, cls: &'static Class) -> Value {
        crate::note_external(table.heap_bytes());
        let h = self.alloc_raw(std::mem::size_of::<DictObj>(), cls);
        unsafe { std::ptr::write((*(h as *mut DictObj)).table.get(), table) };
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
            std::ptr::write(&mut (*(h as *mut MutexObj)).raw, parking_lot::Mutex::new(()));
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
        });
        unsafe {
            std::ptr::write(&mut (*(h as *mut ChannelObj)).inner, inner);
            (*h).set_state(ObjState::Shared);
        }
        Value::from_ptr(h)
    }

    pub fn alloc_thread(&mut self) -> Value {
        let h = self.alloc_raw(std::mem::size_of::<ThreadObj>(), BUILTINS.thread);
        let inner = Box::new(ThreadInner {
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
