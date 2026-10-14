//! Slow paths of the interpreter: container access with ownership checks, generic operators,
//! runtime type conformance and error construction.

use catallaxy_runtime::class::{class_id_of, class_of, heap_class, ids, ObjKind};
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops::{self, Interop};
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::{intern, panic, RtResult, Trap, Value};

use crate::bytecode::{Op, RtType};
use crate::interp::Vm;
use crate::program;

/// Whether a fused compare-and-jump jumps, for operands off the small-int fast path.
#[cold]
#[inline(never)]
#[allow(clippy::neg_cmp_op_on_partial_ord)] // a NaN comparison is false, so it jumps
pub fn fused_jump(op: Op, x: Value, y: Value) -> RtResult<bool> {
    Ok(match op {
        Op::JNLtF | Op::JNLeF => {
            let (x, y) = (f(x)?, f(y)?);
            if op == Op::JNLtF {
                !(x < y)
            } else {
                !(x <= y)
            }
        }
        Op::JNEqI | Op::JEqI => {
            // As EqI: non-Int operands are simply unequal.
            let eq = ops::is_int(x) && ops::is_int(y) && ops::int_cmp(x, y).is_eq();
            eq == (op == Op::JEqI)
        }
        _ => {
            expect_ints(x, y, if op == Op::JNLtI { "<" } else { "<=" })?;
            let ord = ops::int_cmp(x, y);
            if op == Op::JNLtI {
                !ord.is_lt()
            } else {
                !ord.is_le()
            }
        }
    })
}

/// Compares a non-small-int operand with a small constant for the `J*IK` jumps: big integers
/// compare by value; anything else is a type error, except equality, where it is simply unequal.
#[cold]
#[inline(never)]
pub fn cmp_int_imm(op: Op, x: Value, k: i64) -> RtResult<std::cmp::Ordering> {
    if ops::is_int(x) {
        return Ok(ops::int_cmp(x, Value::small_int(k)));
    }
    if matches!(op, Op::JNEqIK | Op::JEqIK) {
        // Not equal to any Int: report "greater" so equality is false.
        return Ok(std::cmp::Ordering::Greater);
    }
    let sym = match op {
        Op::JNLtIK => "<",
        Op::JNLeIK => "<=",
        Op::JNGtIK => ">",
        _ => ">=",
    };
    expect_ints(x, Value::small_int(k), sym)?;
    unreachable!()
}

/// `GetField` off its fast path: errors, shared objects and foreign owners.
#[cold]
#[inline(never)]
pub fn get_field(vm: &mut Vm, o: Value, i: usize) -> RtResult<Value> {
    if !o.is_ptr() {
        return Err(field_on_non_object(o));
    }
    let h = unsafe { &*o.as_ptr() };
    let inst = unsafe { as_instance(o) };
    Ok(if h.is_local_or_frozen(vm.tid()) {
        inst.get(i)
    } else {
        ownership::check_read(&vm.m, h)?;
        inst.get_acquire(i)
    })
}

pub fn expect_ints(x: Value, y: Value, op: &str) -> RtResult<()> {
    if ops::is_int(x) && ops::is_int(y) {
        Ok(())
    } else {
        panic(format!("TypeError: `{op}` expects Int operands, got {} and {}", class_of(x).name, class_of(y).name))
    }
}

#[inline]
pub fn f(v: Value) -> RtResult<f64> {
    if v.is_float() {
        Ok(v.as_f64())
    } else if ops::is_int(v) {
        Ok(ops::int_to_f64(v))
    } else {
        panic(format!("TypeError: expected Float, got {}", class_of(v).name))
    }
}

#[cold]
pub fn field_on_non_object(v: Value) -> Trap {
    Trap::Panic(format!("internal error: field access on {}", class_of(v).name))
}

fn type_name(v: Value) -> String {
    class_of(v).name.to_string()
}

fn index_error(i: i64, len: usize) -> Trap {
    Trap::Panic(format!("IndexError: index {i} out of range for length {len}"))
}

fn norm_index(i: Value, len: usize) -> RtResult<usize> {
    let Some(mut idx) = (if ops::is_int(i) { ops::int_as_i64(i) } else { None }) else {
        return panic(format!("TypeError: index must be Int, got {}", type_name(i)));
    };
    let orig = idx;
    if idx < 0 {
        idx += len as i64;
    }
    if idx < 0 || idx as usize >= len {
        return Err(index_error(orig, len));
    }
    Ok(idx as usize)
}

pub fn list_get(vm: &mut Vm, l: Value, i: Value) -> RtResult<Value> {
    if kind_of(l) != ObjKind::List {
        return panic(format!("TypeError: expected List, got {}", type_name(l)));
    }
    let list = ownership::read_list(&vm.m, l)?;
    let ix = norm_index(i, list.len())?;
    // A racing shrink can leave the index out of bounds after the check.
    list.get(ix).ok_or_else(|| index_error(ops::int_as_i64(i).unwrap_or(0), list.len()))
}

pub fn list_set(vm: &mut Vm, l: Value, i: Value, v: Value) -> RtResult<()> {
    if kind_of(l) != ObjKind::List {
        return panic(format!("TypeError: expected List, got {}", type_name(l)));
    }
    let h = unsafe { &*l.as_ptr() };
    if ownership::check_write(&vm.m, h)? == Access::Shared {
        ownership::publish_into_shared(&vm.m, v)?;
    }
    ownership::write_list(&vm.m, l, |list, _| {
        norm_index(i, list.len()).map(|ix| {
            unsafe { list.set(ix, v) };
        })
    })?
}

pub fn list_push(vm: &mut Vm, l: Value, v: Value) -> RtResult<()> {
    let h = unsafe { &*l.as_ptr() };
    if ownership::check_write(&vm.m, h)? == Access::Shared {
        ownership::publish_into_shared(&vm.m, v)?;
    }
    ownership::write_list(&vm.m, l, |list, access| unsafe { list.push(v, access) })
}

/// `recv.name` on a `Dyn` value: a record (dict) key, a field, or a zero-argument method.
/// Record hits update the site's inline cache with `(shape id + 1, slot)`.
pub fn dyn_get(vm: &mut Vm, recv: Value, ic: &program::Ic) -> RtResult<Value> {
    let name = catallaxy_runtime::symbol::name(ic.name);
    if kind_of(recv) == ObjKind::Dict {
        let key = vm.m.alloc_str(name);
        let Some(idx) = ops::dict_lookup(vm, recv, key)? else { return Ok(Value::NIL) };
        let (v, shape) = ownership::read_dict(&vm.m, recv, |t| (t.entry(idx).value, t.shape().map(|s| s.id)))?;
        if let Some(id) = shape {
            ic.cache.store(((id.wrapping_add(1) as u64) << 32) | idx as u64, std::sync::atomic::Ordering::Relaxed);
        }
        return Ok(v);
    }
    dyn_member(vm, recv, name)
}

/// `recv.name` on a `Dyn` value that is not a record.
pub fn dyn_member(vm: &mut Vm, recv: Value, n: &str) -> RtResult<Value> {
    let sym = intern(n);
    let cls = class_of(recv);
    if cls.kind == ObjKind::Instance {
        if let Some(i) = cls.field_index(sym) {
            return Ok(unsafe { as_instance(recv) }.get(i));
        }
    }
    if let Some(fid) = cls.lookup(sym) {
        return vm.call_func(fid, &[recv]);
    }
    if recv.is_nil() {
        return panic(format!("NoMethodError: undefined method `{n}` for nil (a Dyn value was nil)"));
    }
    panic(format!("NoMethodError: undefined method `{n}` for {}", cls.name))
}

pub fn dict_get(vm: &mut Vm, d: Value, k: Value) -> RtResult<Option<Value>> {
    match ops::dict_lookup(vm, d, k)? {
        Some(idx) => Ok(Some(ownership::read_dict(&vm.m, d, |t| t.entry(idx).value)?)),
        None => Ok(None),
    }
}

pub fn dict_set(vm: &mut Vm, d: Value, k: Value, v: Value) -> RtResult<()> {
    let h = unsafe { &*d.as_ptr() };
    if ownership::check_write(&vm.m, h)? == Access::Shared {
        ownership::publish_into_shared(&vm.m, k)?;
        ownership::publish_into_shared(&vm.m, v)?;
    }
    let hash = ops::hash_value(vm, k)?;
    loop {
        // A lookup may run user `==`, which can mutate the table; act only if the version
        // is unchanged since before it.
        let version = ownership::read_dict(&vm.m, d, |t| t.version())?;
        let found = ops::dict_lookup_hashed(vm, d, k, hash)?;
        let done = ownership::write_dict(&vm.m, d, |t, access| {
            if t.version() != version {
                return false;
            }
            unsafe {
                match found {
                    Some(idx) => t.set_value_at(idx, v, access),
                    None => t.insert_new(hash, k, v, access),
                }
            }
            true
        })?;
        if done {
            return Ok(());
        }
    }
}

fn char_at(s: &StrObj, i: Value) -> RtResult<Option<&str>> {
    let Some(mut idx) = (if ops::is_int(i) { ops::int_as_i64(i) } else { None }) else {
        return panic(format!("TypeError: string index must be Int, got {}", type_name(i)));
    };
    let n = s.char_count() as i64;
    if idx < 0 {
        idx += n;
    }
    if idx < 0 || idx >= n {
        return Ok(None);
    }
    Ok(Some(s.char_at(idx as usize)))
}

/// The [start, end) bounds selected by a range over a sequence of `len` items, clamped.
fn range_bounds(r: Value, len: usize) -> RtResult<(usize, usize)> {
    let inst = unsafe { as_instance(r) };
    let (first, last, excl) = (inst.get(0), inst.get(1), inst.get(2).is_true());
    let (Some(a), Some(b)) = (ops::int_as_i64(first), ops::int_as_i64(last)) else {
        return panic("RangeError: range bounds too large");
    };
    let n = len as i64;
    let a = if a < 0 { a + n } else { a };
    let b = if b < 0 { b + n } else { b };
    let end = if excl { b } else { b + 1 };
    let start = a.clamp(0, n);
    let end = end.clamp(start, n);
    Ok((start as usize, end as usize))
}

fn is_range(v: Value) -> bool {
    v.is_ptr() && unsafe { heap_class(v.as_ptr()) }.id == ids::RANGE
}

/// Generic `x[i]` for lists, dicts, strings, tuples and ranges (and `Dyn` values).
pub fn index_get(vm: &mut Vm, x: Value, i: Value) -> RtResult<Value> {
    if is_range(i) {
        match kind_of(x) {
            ObjKind::List => {
                let items = ownership::read_list(&vm.m, x)?.to_vec();
                let (a, b) = range_bounds(i, items.len())?;
                return Ok(vm.m.alloc_list_from_slice(&items[a..b], b - a));
            }
            ObjKind::Str => {
                let s = unsafe { as_str_obj(x) };
                let (a, b) = range_bounds(i, s.char_count())?;
                return Ok(vm.m.alloc_str(s.char_slice(a, b)));
            }
            _ => {}
        }
    }
    match kind_of(x) {
        ObjKind::List => list_get(vm, x, i),
        ObjKind::Dict => Ok(dict_get(vm, x, i)?.unwrap_or(Value::NIL)),
        ObjKind::Str => {
            let s = unsafe { as_str_obj(x) };
            match char_at(s, i)? {
                Some(c) => Ok(vm.m.alloc_str(c)),
                None => Ok(Value::NIL),
            }
        }
        ObjKind::Tuple => {
            let t = unsafe { as_tuple(x) };
            let ix = norm_index(i, t.len)?;
            Ok(t.elems()[ix])
        }
        _ => {
            let cls = class_of(x);
            if let Some(fid) = cls.lookup(intern("[]")) {
                return vm.call_func(fid, &[x, i]);
            }
            panic(format!("NoMethodError: `[]` is not defined for {}", cls.name))
        }
    }
}

/// `x[i]` on a `Dyn` value: a list index out of range is nil, like a missing object key.
pub fn dyn_index_get(vm: &mut Vm, x: Value, i: Value) -> RtResult<Value> {
    if kind_of(x) == ObjKind::List {
        if let Some(idx) = ops::int_as_i64(i) {
            let len = ownership::read_list(&vm.m, x)?.len() as i64;
            if idx >= len || idx < -len {
                return Ok(Value::NIL);
            }
        }
    }
    index_get(vm, x, i)
}

pub fn index_set(vm: &mut Vm, x: Value, i: Value, v: Value) -> RtResult<()> {
    match kind_of(x) {
        ObjKind::List => list_set(vm, x, i, v),
        ObjKind::Dict => dict_set(vm, x, i, v),
        _ => {
            let cls = class_of(x);
            if let Some(fid) = cls.lookup(intern("[]=")) {
                vm.call_func(fid, &[x, i, v])?;
                return Ok(());
            }
            panic(format!("NoMethodError: `[]=` is not defined for {}", cls.name))
        }
    }
}

/// Arithmetic on values whose types are only known at runtime (`Dyn`, unions, erased generics).
pub fn generic_arith(vm: &mut Vm, op: Op, x: Value, y: Value) -> RtResult<Value> {
    let xi = ops::is_int(x);
    let yi = ops::is_int(y);
    if xi && yi {
        let m = &mut vm.m;
        return match op {
            Op::Add => Ok(ops::int_add(m, x, y)),
            Op::Sub => Ok(ops::int_sub(m, x, y)),
            Op::Mul => Ok(ops::int_mul(m, x, y)),
            Op::Div => ops::int_div(m, x, y),
            Op::Mod => ops::int_mod(m, x, y),
            Op::Pow => ops::int_pow(m, x, y),
            _ => unreachable!(),
        };
    }
    if (x.is_float() || xi) && (y.is_float() || yi) {
        let (a, b) = (f(x)?, f(y)?);
        let r = match op {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div => a / b,
            Op::Mod => {
                let m = a % b;
                if m != 0.0 && (m < 0.0) != (b < 0.0) {
                    m + b
                } else {
                    m
                }
            }
            Op::Pow => a.powf(b),
            _ => unreachable!(),
        };
        return Ok(Value::from_f64(r));
    }
    if op == Op::Add {
        if let (Some(a), Some(b)) = (str_of(x), str_of(y)) {
            let s = format!("{a}{b}");
            return Ok(vm.m.alloc_str(&s));
        }
        if kind_of(x) == ObjKind::List && kind_of(y) == ObjKind::List {
            let mut items = ownership::read_list(&vm.m, x)?.to_vec();
            let more = ownership::read_list(&vm.m, y)?.to_vec();
            items.extend(more);
            return Ok(vm.m.alloc_list(items));
        }
    }
    if op == Op::Mul {
        if let (Some(s), true) = (str_of(x), yi) {
            let n = ops::int_as_i64(y).unwrap_or(0).max(0) as usize;
            let r = s.repeat(n);
            return Ok(vm.m.alloc_str(&r));
        }
    }
    let name = match op {
        Op::Add => "+",
        Op::Sub => "-",
        Op::Mul => "*",
        Op::Div => "/",
        Op::Mod => "%",
        _ => "**",
    };
    let cls = class_of(x);
    if let Some(fid) = cls.lookup(intern(name)) {
        return vm.call_func(fid, &[x, y]);
    }
    panic(format!("NoMethodError: undefined operator `{name}` for {} and {}", cls.name, type_name(y)))
}

pub fn generic_neg(vm: &mut Vm, x: Value) -> RtResult<Value> {
    if ops::is_int(x) {
        return Ok(ops::int_neg(&mut vm.m, x));
    }
    if x.is_float() {
        return Ok(Value::from_f64(-x.as_f64()));
    }
    let cls = class_of(x);
    if let Some(fid) = cls.lookup(intern("-")) {
        return vm.call_func(fid, &[x]);
    }
    panic(format!("NoMethodError: undefined unary `-` for {}", cls.name))
}

pub fn has_iface(v: Value, id: u32) -> bool {
    let cls = class_of(v);
    program::iface(id).iter().all(|&m| cls.lookup(m).is_some())
}

/// Checks `v` against runtime type `id` (deeply for containers).
pub fn conforms(vm: &mut Vm, v: Value, id: u32) -> RtResult<bool> {
    conforms_ty(vm, v, program::rt_type(id))
}

fn conforms_ty(vm: &mut Vm, v: Value, t: &RtType) -> RtResult<bool> {
    Ok(match t {
        RtType::Any => true,
        RtType::Int => ops::is_int(v),
        RtType::Float => v.is_float(),
        RtType::Bool => v.is_bool(),
        RtType::Nil => v.is_nil(),
        RtType::Str => kind_of(v) == ObjKind::Str,
        RtType::Fn => kind_of(v) == ObjKind::Closure,
        RtType::Class(cid) => {
            if *cid < ids::FIRST_USER {
                class_id_of(v) == *cid
            } else {
                v.is_ptr()
                    && unsafe { heap_class(v.as_ptr()) }.is_subclass_of(catallaxy_runtime::class::class_by_id(*cid))
            }
        }
        RtType::Enum(eid) => v.is_ptr() && unsafe { heap_class(v.as_ptr()) }.enum_id == Some(*eid),
        RtType::Optional(inner) => v.is_nil() || conforms_ty(vm, v, inner)?,
        RtType::Union(ts) => {
            for t in ts {
                if conforms_ty(vm, v, t)? {
                    return Ok(true);
                }
            }
            false
        }
        RtType::Iface(id) => has_iface(v, *id),
        RtType::Tuple(ts) => {
            if kind_of(v) != ObjKind::Tuple {
                return Ok(false);
            }
            let elems = unsafe { as_tuple(v) }.elems().to_vec();
            if elems.len() != ts.len() {
                return Ok(false);
            }
            for (e, t) in elems.into_iter().zip(ts) {
                if !conforms_ty(vm, e, t)? {
                    return Ok(false);
                }
            }
            true
        }
        RtType::List(et) | RtType::Set(et) => {
            let want = if matches!(t, RtType::List(_)) { ObjKind::List } else { ObjKind::Set };
            if kind_of(v) != want {
                return Ok(false);
            }
            if **et == RtType::Any {
                return Ok(true);
            }
            let items: Vec<Value> = if want == ObjKind::List {
                ownership::read_list(&vm.m, v)?.to_vec()
            } else {
                ownership::read_dict(&vm.m, v, |t| t.iter().map(|e| e.key).collect())?
            };
            for x in items {
                if !conforms_ty(vm, x, et)? {
                    return Ok(false);
                }
            }
            true
        }
        RtType::Dict(kt, vt) => {
            if kind_of(v) != ObjKind::Dict {
                return Ok(false);
            }
            if **kt == RtType::Any && **vt == RtType::Any {
                return Ok(true);
            }
            let entries: Vec<(Value, Value)> =
                ownership::read_dict(&vm.m, v, |t| t.iter().map(|e| (e.key, e.value)).collect())?;
            for (k, x) in entries {
                if !conforms_ty(vm, k, kt)? || !conforms_ty(vm, x, vt)? {
                    return Ok(false);
                }
            }
            true
        }
    })
}

/// Builds `Ok(v)` / `Err(v)` values of the prelude's `Result`.
pub fn make_result(vm: &mut Vm, ok: bool, v: Value) -> Value {
    let (okc, errc) = program::result_classes();
    let cls = catallaxy_runtime::class::class_by_id(if ok { okc } else { errc });
    let r = vm.m.alloc_instance(cls);
    unsafe { as_instance(r) }.set(0, v);
    r
}

#[allow(dead_code)]
fn unused(_: &dyn Interop) {}
