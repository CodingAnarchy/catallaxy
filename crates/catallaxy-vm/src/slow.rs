//! Slow paths of the interpreter: container access with ownership checks, generic operators,
//! runtime type conformance and error construction.

use catallaxy_runtime::class::{class_id_of, class_of, heap_class, ids, ObjKind};
use catallaxy_runtime::dict::Probe;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops::{self, Interop};
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::{intern, panic, RtResult, Trap, Value};

use crate::bytecode::{Op, RtType};
use crate::interp::Vm;
use crate::program;

pub fn expect_ints(x: Value, y: Value, op: &str) -> RtResult<()> {
    if ops::is_int(x) && ops::is_int(y) {
        Ok(())
    } else {
        panic(format!(
            "TypeError: `{op}` expects Int operands, got {} and {}",
            class_of(x).name,
            class_of(y).name
        ))
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
    let r = ownership::with_read(&vm.m, l, || {
        let items = unsafe { as_list(l).items() };
        norm_index(i, items.len()).map(|ix| items[ix])
    })?;
    r
}

pub fn list_set(vm: &mut Vm, l: Value, i: Value, v: Value) -> RtResult<()> {
    if kind_of(l) != ObjKind::List {
        return panic(format!("TypeError: expected List, got {}", type_name(l)));
    }
    let h = unsafe { &*l.as_ptr() };
    if ownership::check_write(&vm.m, h)? == Access::Shared {
        ownership::publish_into_shared(&vm.m, v)?;
    }
    ownership::with_write(&vm.m, l, |_| {
        let items = unsafe { as_list(l).items_mut() };
        norm_index(i, items.len()).map(|ix| items[ix] = v)
    })?
}

pub fn list_push(vm: &mut Vm, l: Value, v: Value) -> RtResult<()> {
    let h = unsafe { &*l.as_ptr() };
    if ownership::check_write(&vm.m, h)? == Access::Shared {
        ownership::publish_into_shared(&vm.m, v)?;
    }
    ownership::with_write(&vm.m, l, |_| {
        let items = unsafe { as_list(l).items_mut() };
        if items.len() == items.capacity() {
            catallaxy_runtime::note_external(items.capacity().max(4) * 8);
        }
        items.push(v);
    })
}

pub fn dict_get(vm: &mut Vm, d: Value, k: Value) -> RtResult<Option<Value>> {
    match ops::dict_lookup(vm, d, k)? {
        Some(idx) => Ok(Some(ownership::with_read(&vm.m, d, || unsafe { as_dict(d).table() }.entry(idx).value)?)),
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
        let found = ops::dict_lookup_hashed(vm, d, k, hash)?;
        let done = ownership::with_write(&vm.m, d, |_| {
            let t = unsafe { as_dict(d).table_mut() };
            match found {
                Some(idx) => {
                    // The table may have changed since the lookup (user `==`); re-validate.
                    if t.raw_entry(idx).is_some_and(|e| e.hash == hash) {
                        t.set_value_at(idx, v);
                        true
                    } else {
                        false
                    }
                }
                None => match t.probe(hash, k) {
                    Probe::Found(idx) => {
                        t.set_value_at(idx, v);
                        true
                    }
                    Probe::Absent => {
                        let before = t.heap_bytes();
                        t.insert_new(hash, k, v);
                        let after = t.heap_bytes();
                        if after > before {
                            catallaxy_runtime::note_external(after - before);
                        }
                        true
                    }
                    Probe::Check(_) => false,
                },
            }
        })?;
        if done {
            return Ok(());
        }
    }
}

fn char_at(s: &str, i: Value) -> RtResult<Option<&str>> {
    let Some(mut idx) = (if ops::is_int(i) { ops::int_as_i64(i) } else { None }) else {
        return panic(format!("TypeError: string index must be Int, got {}", type_name(i)));
    };
    if s.is_ascii() {
        if idx < 0 {
            idx += s.len() as i64;
        }
        if idx < 0 || idx as usize >= s.len() {
            return Ok(None);
        }
        return Ok(Some(&s[idx as usize..idx as usize + 1]));
    }
    let n = s.chars().count() as i64;
    if idx < 0 {
        idx += n;
    }
    if idx < 0 || idx >= n {
        return Ok(None);
    }
    let (start, ch) = s.char_indices().nth(idx as usize).unwrap();
    Ok(Some(&s[start..start + ch.len_utf8()]))
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
                let items = ownership::with_read(&vm.m, x, || unsafe { as_list(x).items() }.clone())?;
                let (a, b) = range_bounds(i, items.len())?;
                return Ok(vm.m.alloc_list(items[a..b].to_vec()));
            }
            ObjKind::Str => {
                let s = unsafe { as_str_obj(x) }.as_str();
                let n = unsafe { as_str_obj(x) }.char_count();
                let (a, b) = range_bounds(i, n)?;
                let sub: String = if s.is_ascii() { s[a..b].to_string() } else { s.chars().skip(a).take(b - a).collect() };
                return Ok(vm.m.alloc_str(&sub));
            }
            _ => {}
        }
    }
    match kind_of(x) {
        ObjKind::List => list_get(vm, x, i),
        ObjKind::Dict => Ok(dict_get(vm, x, i)?.unwrap_or(Value::NIL)),
        ObjKind::Str => {
            let s = unsafe { as_str_obj(x) }.as_str();
            match char_at(s, i)? {
                Some(c) => {
                    let c = c.to_string();
                    Ok(vm.m.alloc_str(&c))
                }
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
            let mut items = ownership::with_read(&vm.m, x, || unsafe { as_list(x).items() }.clone())?;
            let more = ownership::with_read(&vm.m, y, || unsafe { as_list(y).items() }.clone())?;
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
                v.is_ptr() && unsafe { heap_class(v.as_ptr()) }.is_subclass_of(catallaxy_runtime::class::class_by_id(*cid))
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
                ownership::with_read(&vm.m, v, || unsafe { as_list(v).items() }.clone())?
            } else {
                ownership::with_read(&vm.m, v, || unsafe { as_dict(v).table() }.iter().map(|e| e.key).collect())?
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
                ownership::with_read(&vm.m, v, || unsafe { as_dict(v).table() }.iter().map(|e| (e.key, e.value)).collect())?;
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
