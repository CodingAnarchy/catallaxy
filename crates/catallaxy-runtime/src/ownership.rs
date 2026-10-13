//! Ownership states at runtime: access checks, deep freeze, promotion to shared, and transfer
//! of owned object graphs between threads.
//!
//! The type checker rejects most cross-thread misuse statically. These checks are the backstop for
//! paths it cannot see (`Dyn`, erased generics, native code), and they are what guarantees memory
//! safety: container buffers are only mutated by their owning thread, or under the object's lock
//! when shared.

use std::sync::atomic::{AtomicBool, Ordering};

use catallaxy_gc::{Header, ObjState};

use crate::class::{heap_class, ObjKind};
use crate::dict::DictTable;
use crate::mutator::{Mutator, RtResult, Trap};
use crate::object::*;
use crate::value::Value;

/// When set, touching another thread's owned object promotes it to shared (with a warning)
/// instead of panicking with `OwnershipError`.
pub static PROMOTE_ON_ACCESS: AtomicBool = AtomicBool::new(false);

/// Reports promotions under `--promote-on-access`. The VM installs a hook that names the site.
static PROMOTION_HOOK: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

pub fn set_promotion_hook(f: fn(&str)) {
    let _ = PROMOTION_HOOK.set(f);
}

/// How an access must be performed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Access {
    /// Owned by this thread, or frozen (for reads): plain loads and stores.
    Local,
    /// Shared: take the object's lock (containers) or use acquire/release (fields).
    Shared,
}

/// Checks that the current thread may read the object at `h`.
#[inline(always)]
pub fn check_read(m: &Mutator, h: &Header) -> RtResult<Access> {
    let meta = h.meta();
    match Header::state_of(meta) {
        ObjState::Owned if Header::owner_of(meta) == m.tid => Ok(Access::Local),
        ObjState::Frozen => Ok(Access::Local),
        ObjState::Shared => Ok(Access::Shared),
        ObjState::Owned => foreign(m, h),
    }
}

/// Checks that the current thread may write the object at `h`. Writing into an old object this
/// thread owns adds it to the remembered set (it may come to point at young objects).
#[inline(always)]
pub fn check_write(m: &Mutator, h: &Header) -> RtResult<Access> {
    let meta = h.meta();
    match Header::state_of(meta) {
        ObjState::Owned if Header::owner_of(meta) == m.tid => {
            if Header::needs_remember(meta) {
                remember(m, h);
            }
            Ok(Access::Local)
        }
        ObjState::Shared => Ok(Access::Shared),
        ObjState::Frozen => Err(frozen_error(h)),
        ObjState::Owned => foreign(m, h),
    }
}

pub fn frozen_error(h: &Header) -> Trap {
    let cls = unsafe { heap_class(h) };
    Trap::Panic(format!("FrozenError: cannot modify frozen {}", cls.name))
}

#[cold]
fn foreign(m: &Mutator, h: &Header) -> RtResult<Access> {
    let cls = unsafe { heap_class(h) };
    if PROMOTE_ON_ACCESS.load(Ordering::Relaxed) {
        share_deep(m, Value::from_ptr(h), true)?;
        if let Some(hook) = PROMOTION_HOOK.get() {
            hook(&cls.name);
        } else {
            eprintln!("warning: promoted {} to shared on access from thread {}", cls.name, m.tid);
        }
        return Ok(Access::Shared);
    }
    Err(Trap::Panic(format!(
        "OwnershipError: {} owned by thread {} accessed from thread {}",
        cls.name,
        h.owner(),
        m.tid
    )))
}

/// Write barrier slow path: records `h`, an old object owned by this thread, in the remembered set.
#[cold]
pub fn remember(m: &Mutator, h: &Header) {
    // SAFETY: the heap belongs to this (running) thread.
    unsafe { (*m.heap_ptr()).remember(h) };
}

/// Makes everything reachable from `v` old that a minor collection of thread `tid` could
/// otherwise free: young objects, found directly or through old objects `tid` owns and wrote
/// since its last minor collection. Every path that lets another thread or a global reach `v`
/// calls this first (sharing, freezing, moving, publishing).
pub fn escape(v: Value, tid: u16) {
    let Some(h) = v.header() else { return };
    let meta = h.meta();
    let owned_old = Header::state_of(meta) == ObjState::Owned && Header::owner_of(meta) == tid && h.is_remembered();
    if !h.is_young() && !owned_old {
        return; // the common case: nothing young below an old, unwritten object
    }
    let mut work = vec![v];
    let mut seen_old = rustc_hash::FxHashSet::default();
    while let Some(x) = work.pop() {
        let Some(h) = x.header() else { continue };
        if h.is_young() {
            h.tenure();
            for_each_child(x, |c| work.push(c));
            continue;
        }
        let meta = h.meta();
        if Header::state_of(meta) == ObjState::Owned
            && Header::owner_of(meta) == tid
            && h.is_remembered()
            && seen_old.insert(x.0)
        {
            for_each_child(x, |c| work.push(c));
        }
    }
}

/// Calls `f` for every value directly referenced by the object `v`.
pub fn for_each_child(v: Value, mut f: impl FnMut(Value)) {
    let Some(h) = v.header() else { return };
    let cls = unsafe { heap_class(h) };
    unsafe {
        match cls.kind {
            ObjKind::Instance => {
                let o = as_instance(v);
                for i in 0..cls.nfields() {
                    f(o.get(i));
                }
            }
            ObjKind::List => {
                for x in as_list(v).to_vec() {
                    f(x)
                }
            }
            ObjKind::Dict | ObjKind::Set => {
                for e in as_dict(v).table().iter() {
                    f(e.key);
                    f(e.value);
                }
            }
            ObjKind::Tuple => {
                for &x in as_tuple(v).elems() {
                    f(x)
                }
            }
            ObjKind::Closure => {
                for &x in as_closure(v).caps() {
                    f(x)
                }
            }
            ObjKind::Cell => f(Value(as_cell(v).value.load(Ordering::Relaxed))),
            ObjKind::Mutex | ObjKind::Atomic => {
                // Shareable wrappers own their contents; they are never walked into.
            }
            _ => {}
        }
    }
}

/// Deeply freezes `v`: every reachable object owned by this thread becomes frozen.
/// Frozen and shared objects are left as they are (they never point to owned ones).
pub fn freeze_deep(m: &Mutator, v: Value) -> RtResult<()> {
    // Frozen objects may be shared freely, so they must be old.
    escape(v, m.tid);
    let mut work = vec![v];
    while let Some(x) = work.pop() {
        let Some(h) = x.header() else { continue };
        let meta = h.meta();
        match Header::state_of(meta) {
            ObjState::Frozen | ObjState::Shared => continue,
            ObjState::Owned if Header::owner_of(meta) != m.tid => {
                let cls = unsafe { heap_class(h) };
                return Err(Trap::Panic(format!(
                    "OwnershipError: cannot freeze {} owned by thread {}",
                    cls.name,
                    h.owner()
                )));
            }
            ObjState::Owned => {
                let kind = unsafe { heap_class(h).kind };
                if matches!(kind, ObjKind::StringBuilder | ObjKind::Native | ObjKind::Thread) {
                    // Mutable native state cannot be frozen; it stays owned.
                    continue;
                }
                h.set_state(ObjState::Frozen);
                for_each_child(x, |c| work.push(c));
            }
        }
    }
    Ok(())
}

/// Deeply promotes `v` to shared. With `steal`, objects owned by other threads are promoted too
/// (`--promote-on-access`); otherwise they are an error.
pub fn share_deep(m: &Mutator, v: Value, steal: bool) -> RtResult<()> {
    escape(v, m.tid);
    let mut work = vec![v];
    while let Some(x) = work.pop() {
        let Some(h) = x.header() else { continue };
        let meta = h.meta();
        match Header::state_of(meta) {
            ObjState::Frozen | ObjState::Shared => continue,
            ObjState::Owned if !steal && Header::owner_of(meta) != m.tid => {
                let cls = unsafe { heap_class(h) };
                return Err(Trap::Panic(format!(
                    "OwnershipError: cannot share {} owned by thread {}",
                    cls.name,
                    h.owner()
                )));
            }
            ObjState::Owned => {
                h.set_state(ObjState::Shared);
                for_each_child(x, |c| work.push(c));
            }
        }
    }
    Ok(())
}

/// Moves ownership of every object reachable from `v` and owned by `from` to `to`.
/// Objects owned by anyone else, frozen or shared are not touched.
pub fn transfer_deep(v: Value, from: u16, to: u16) {
    if from == to {
        return;
    }
    // The receiver must not see objects the sender's next minor collection could free. Values
    // in transit (`from` is the transit id) are already old.
    escape(v, from);
    let mut work = vec![v];
    while let Some(x) = work.pop() {
        let Some(h) = x.header() else { continue };
        let meta = h.meta();
        if Header::state_of(meta) == ObjState::Owned && Header::owner_of(meta) == from {
            h.set_owned_by(to);
            for_each_child(x, |c| work.push(c));
        }
    }
}

/// Owner id used for values in transit through a channel.
pub const TRANSIT_TID: u16 = 0;

/// Prepares a value to be stored into a shared container: owned objects of this thread are
/// promoted to shared so other threads may reach them.
#[inline]
pub fn publish_into_shared(m: &Mutator, v: Value) -> RtResult<()> {
    escape(v, m.tid);
    if let Some(h) = v.header() {
        if h.state() == ObjState::Owned {
            share_deep(m, v, false)?;
        }
    }
    Ok(())
}

/// Takes a shared object's lock. If it is contended, the thread waits in native state so a lock
/// holder that reaches a safepoint (inside `with lock`) can still collect.
pub fn lock_obj<'a>(m: &Mutator, h: &'a Header) -> catallaxy_gc::ObjLockGuard<'a> {
    if !h.try_lock(m.tid) {
        catallaxy_gc::enter_native(&m.rec);
        h.lock(m.tid);
        catallaxy_gc::leave_native(&m.rec);
    }
    catallaxy_gc::ObjLockGuard::adopt(h)
}

/// Checks that the current thread may read the list `v`. List reads never lock (see
/// [`ListObj`]), so this is the only synchronization a list read needs.
#[inline]
pub fn read_list<'a>(m: &Mutator, v: Value) -> RtResult<&'a ListObj> {
    let h = unsafe { &*v.as_ptr() };
    check_read(m, h)?;
    Ok(unsafe { as_list(v) })
}

/// Runs `f` on a list with write access: directly when owned, under the lock when shared.
/// For shared lists, values stored must first go through [`publish_into_shared`].
#[inline]
pub fn write_list<R>(m: &Mutator, v: Value, f: impl FnOnce(&ListObj, Access) -> R) -> RtResult<R> {
    with_write(m, v, |access| f(unsafe { as_list(v) }, access))
}

/// Reads a dict or set. Owned and frozen tables are read directly; shared ones through the
/// table's seqlock, falling back to the object's lock when writers keep overlapping. `f` may run
/// more than once and must only read.
#[inline]
pub fn read_dict<R>(m: &Mutator, v: Value, f: impl Fn(&DictTable) -> R) -> RtResult<R> {
    let h = unsafe { &*v.as_ptr() };
    let t = unsafe { as_dict(v) }.table();
    match check_read(m, h)? {
        Access::Local => Ok(f(t)),
        Access::Shared => match t.read_optimistic(&f) {
            Some(r) => Ok(r),
            None => {
                let _g = lock_obj(m, h);
                Ok(f(t))
            }
        },
    }
}

/// Runs `f` on a dict or set with write access: directly when owned, under the lock when shared.
#[inline]
pub fn write_dict<R>(m: &Mutator, v: Value, f: impl FnOnce(&DictTable, Access) -> R) -> RtResult<R> {
    with_write(m, v, |access| f(unsafe { as_dict(v) }.table(), access))
}

/// Runs `f` on a container's contents with the right synchronization for a read.
#[inline]
pub fn with_read<R>(m: &Mutator, v: Value, f: impl FnOnce() -> R) -> RtResult<R> {
    let h = unsafe { &*v.as_ptr() };
    match check_read(m, h)? {
        Access::Local => Ok(f()),
        Access::Shared => {
            let _g = lock_obj(m, h);
            Ok(f())
        }
    }
}

/// Runs `f` on a container's contents with the right synchronization for a write.
/// For shared containers, values stored must first go through [`publish_into_shared`].
#[inline]
pub fn with_write<R>(m: &Mutator, v: Value, f: impl FnOnce(Access) -> R) -> RtResult<R> {
    let h = unsafe { &*v.as_ptr() };
    match check_write(m, h)? {
        Access::Local => Ok(f(Access::Local)),
        Access::Shared => {
            let _g = lock_obj(m, h);
            Ok(f(Access::Shared))
        }
    }
}
