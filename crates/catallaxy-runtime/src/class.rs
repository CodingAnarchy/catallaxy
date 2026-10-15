//! Class descriptors.
//!
//! Every heap object's header word 0 points at a [`Class`], whose first field is the GC vtable.
//! Classes are created when a program loads and live forever. They are frozen: after
//! [`register_class`] returns, nothing in a class changes, so any thread may read them freely.

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::LazyLock;

use catallaxy_gc::{GcVTable, Header};
use rustc_hash::FxHashMap;

use crate::object;
use crate::symbol::Sym;
use crate::value::Value;

/// Physical layout of a class's instances.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ObjKind {
    /// Not a heap object (Int, Float, Bool, Nil).
    Immediate,
    /// Fixed fields inline after the header (user classes, enum variants, ranges).
    Instance,
    Str,
    BigInt,
    List,
    Dict,
    Set,
    Tuple,
    Closure,
    Cell,
    StringBuilder,
    Mutex,
    Atomic,
    Channel,
    Thread,
    /// Opaque native resource (files and the like).
    Native,
}

/// Which ownership state new instances start in, and how the type may cross threads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Capability {
    Owned,
    Frozen,
    Shared,
}

#[repr(C)]
pub struct Class {
    pub gc: GcVTable,
    pub kind: ObjKind,
    pub id: u32,
    /// The number of fields for an instance class, 0 for other kinds: `i < instance_fields`
    /// checks in one compare that a value is an instance with a field `i`.
    pub instance_fields: u32,
    pub name: Box<str>,
    pub capability: Capability,
    pub superclass: Option<&'static Class>,
    pub field_names: Vec<Sym>,
    /// Method name → function id, including inherited methods. Builtin classes get their
    /// methods after registration (when the prelude loads), so the table is swapped atomically.
    methods: AtomicPtr<FxHashMap<Sym, u32>>,
    /// Virtual method table (function ids), for overridable methods of user classes.
    pub vtable: Vec<u32>,
    /// For enum variant classes: the variant's index within its enum.
    pub variant_tag: Option<u32>,
    /// For enum variant classes: the enum's id (shared by all its variants).
    pub enum_id: Option<u32>,
}

impl Class {
    #[inline(always)]
    pub fn nfields(&self) -> usize {
        self.field_names.len()
    }

    pub fn lookup(&self, name: Sym) -> Option<u32> {
        self.methods().get(&name).copied()
    }

    pub fn methods(&self) -> &FxHashMap<Sym, u32> {
        // SAFETY: method tables are leaked and never freed or mutated after publication.
        unsafe { &*self.methods.load(Ordering::Acquire) }
    }

    /// True if `self` is `other` or inherits from it.
    pub fn is_subclass_of(&self, other: &Class) -> bool {
        let mut c = Some(self);
        while let Some(k) = c {
            if k.id == other.id {
                return true;
            }
            c = k.superclass;
        }
        false
    }

    pub fn field_index(&self, name: Sym) -> Option<usize> {
        self.field_names.iter().position(|&f| f == name)
    }
}

// ---------------------------------------------------------------------------------------------
// Registry

const MAX_CLASSES: usize = 1 << 16;

struct Registry {
    slots: Box<[AtomicPtr<Class>]>,
    len: AtomicUsize,
    lock: parking_lot::Mutex<()>,
}

static REGISTRY: LazyLock<Registry> =
    LazyLock::new(|| Registry { slots: null_slots(), len: AtomicUsize::new(0), lock: parking_lot::Mutex::new(()) });

/// `MAX_CLASSES` null slots, as one zeroed allocation: building them one at a time took most
/// of a second natively and about 20 s under Miri, in every test binary (issue #146).
fn null_slots() -> Box<[AtomicPtr<Class>]> {
    let layout = std::alloc::Layout::array::<AtomicPtr<Class>>(MAX_CLASSES).expect("slot table layout");
    // SAFETY: the layout has a nonzero size; an all-zero `AtomicPtr` is a null pointer, and the
    // allocation has the layout `Box<[AtomicPtr<Class>]>` frees with.
    unsafe {
        let p = std::alloc::alloc_zeroed(layout) as *mut AtomicPtr<Class>;
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, MAX_CLASSES))
    }
}

/// Fields of a class under construction. `register_class` assigns the id and the GC vtable.
pub struct ClassSpec {
    pub kind: ObjKind,
    pub name: String,
    pub capability: Capability,
    pub superclass: Option<&'static Class>,
    pub field_names: Vec<Sym>,
    pub methods: FxHashMap<Sym, u32>,
    pub vtable: Vec<u32>,
    pub variant_tag: Option<u32>,
    pub enum_id: Option<u32>,
}

impl ClassSpec {
    pub fn new(kind: ObjKind, name: &str) -> Self {
        ClassSpec {
            kind,
            name: name.to_string(),
            capability: Capability::Owned,
            superclass: None,
            field_names: Vec::new(),
            methods: FxHashMap::default(),
            vtable: Vec::new(),
            variant_tag: None,
            enum_id: None,
        }
    }
}

/// Registers a class, making it immutable and globally visible. Returns its `'static` descriptor.
pub fn register_class(spec: ClassSpec) -> &'static Class {
    let _g = REGISTRY.lock.lock();
    let id = REGISTRY.len.load(Ordering::Relaxed);
    assert!(id < MAX_CLASSES, "class limit exceeded");
    let cls: &'static Class = Box::leak(Box::new(Class {
        gc: object::vtable_for(spec.kind),
        kind: spec.kind,
        id: id as u32,
        instance_fields: if spec.kind == ObjKind::Instance { spec.field_names.len() as u32 } else { 0 },
        name: spec.name.into_boxed_str(),
        capability: spec.capability,
        superclass: spec.superclass,
        field_names: spec.field_names,
        methods: AtomicPtr::new(Box::into_raw(Box::new(spec.methods))),
        vtable: spec.vtable,
        variant_tag: spec.variant_tag,
        enum_id: spec.enum_id,
    }));
    REGISTRY.slots[id].store(cls as *const Class as *mut Class, Ordering::Release);
    REGISTRY.len.store(id + 1, Ordering::Release);
    cls
}

/// Replaces a class's method table (builtin classes get their methods when the prelude loads).
/// The previous table is leaked, so concurrent readers stay valid.
pub fn set_methods(cls: &'static Class, methods: FxHashMap<Sym, u32>) {
    cls.methods.store(Box::into_raw(Box::new(methods)), Ordering::Release);
}

pub fn class_by_id(id: u32) -> &'static Class {
    let p = REGISTRY.slots[id as usize].load(Ordering::Acquire);
    assert!(!p.is_null(), "unknown class id {id}");
    unsafe { &*p }
}

pub fn class_count() -> usize {
    REGISTRY.len.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------------------------
// Builtin classes. Their ids are fixed so the compiler can refer to them.

pub mod ids {
    pub const INT: u32 = 0;
    pub const FLOAT: u32 = 1;
    pub const BOOL: u32 = 2;
    pub const NIL: u32 = 3;
    pub const STRING: u32 = 4;
    pub const BIGINT: u32 = 5;
    pub const LIST: u32 = 6;
    pub const DICT: u32 = 7;
    pub const SET: u32 = 8;
    pub const TUPLE: u32 = 9;
    pub const CLOSURE: u32 = 10;
    pub const CELL: u32 = 11;
    pub const STRING_BUILDER: u32 = 12;
    pub const MUTEX: u32 = 13;
    pub const ATOMIC: u32 = 14;
    pub const CHANNEL: u32 = 15;
    pub const THREAD: u32 = 16;
    pub const RANGE: u32 = 17;
    pub const NATIVE: u32 = 18;
    pub const FIRST_USER: u32 = 19;
}

pub struct Builtins {
    pub int: &'static Class,
    pub float: &'static Class,
    pub bool: &'static Class,
    pub nil: &'static Class,
    pub string: &'static Class,
    pub bigint: &'static Class,
    pub list: &'static Class,
    pub dict: &'static Class,
    pub set: &'static Class,
    pub tuple: &'static Class,
    pub closure: &'static Class,
    pub cell: &'static Class,
    pub string_builder: &'static Class,
    pub mutex: &'static Class,
    pub atomic: &'static Class,
    pub channel: &'static Class,
    pub thread: &'static Class,
    pub range: &'static Class,
    pub native: &'static Class,
}

pub static BUILTINS: LazyLock<Builtins> = LazyLock::new(|| {
    let mk = |kind: ObjKind, name: &str, cap: Capability, fields: &[&str], expect: u32| {
        let mut spec = ClassSpec::new(kind, name);
        spec.capability = cap;
        spec.field_names = fields.iter().map(|f| crate::symbol::intern(f)).collect();
        let c = register_class(spec);
        assert_eq!(c.id, expect, "builtin class {name} registered out of order");
        c
    };
    use Capability::*;
    use ObjKind::*;
    Builtins {
        int: mk(Immediate, "Int", Frozen, &[], ids::INT),
        float: mk(Immediate, "Float", Frozen, &[], ids::FLOAT),
        bool: mk(Immediate, "Bool", Frozen, &[], ids::BOOL),
        nil: mk(Immediate, "Nil", Frozen, &[], ids::NIL),
        string: mk(Str, "String", Frozen, &[], ids::STRING),
        bigint: mk(BigInt, "Int", Frozen, &[], ids::BIGINT),
        list: mk(List, "List", Owned, &[], ids::LIST),
        dict: mk(Dict, "Dict", Owned, &[], ids::DICT),
        set: mk(Set, "Set", Owned, &[], ids::SET),
        tuple: mk(Tuple, "Tuple", Owned, &[], ids::TUPLE),
        closure: mk(Closure, "Fn", Owned, &[], ids::CLOSURE),
        cell: mk(Cell, "Cell", Owned, &[], ids::CELL),
        string_builder: mk(StringBuilder, "StringBuilder", Owned, &[], ids::STRING_BUILDER),
        mutex: mk(Mutex, "Mutex", Shared, &[], ids::MUTEX),
        atomic: mk(Atomic, "Atomic", Shared, &[], ids::ATOMIC),
        channel: mk(Channel, "Channel", Shared, &[], ids::CHANNEL),
        thread: mk(Thread, "Thread", Shared, &[], ids::THREAD),
        range: mk(Instance, "Range", Frozen, &["first", "last", "exclusive"], ids::RANGE),
        native: mk(Native, "NativeHandle", Owned, &[], ids::NATIVE),
    }
});

/// Returns the class of any value, including immediates.
#[inline]
pub fn class_of(v: Value) -> &'static Class {
    if v.is_ptr() {
        unsafe { heap_class(v.as_ptr()) }
    } else {
        immediate_class(v)
    }
}

#[inline]
pub fn immediate_class(v: Value) -> &'static Class {
    let b = &*BUILTINS;
    if v.is_small_int() {
        b.int
    } else if v.is_float() {
        b.float
    } else if v.is_bool() {
        b.bool
    } else {
        b.nil
    }
}

/// The class of a heap object.
///
/// # Safety
/// `h` must point to a live heap object.
#[inline(always)]
pub unsafe fn heap_class(h: *const Header) -> &'static Class {
    &*((*h).vtable() as *const Class)
}

/// Class id of a value; bigints report the `Int` class id.
#[inline]
pub fn class_id_of(v: Value) -> u32 {
    if v.is_ptr() {
        let c = unsafe { heap_class(v.as_ptr()) };
        if c.id == ids::BIGINT {
            ids::INT
        } else {
            c.id
        }
    } else if v.is_small_int() {
        ids::INT
    } else if v.is_float() {
        ids::FLOAT
    } else if v.is_bool() {
        ids::BOOL
    } else {
        ids::NIL
    }
}
