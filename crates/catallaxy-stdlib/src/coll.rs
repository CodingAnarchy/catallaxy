//! List, Dict, Set, Range and StringBuilder natives.

use std::cmp::Ordering as CmpOrdering;

use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops;
use catallaxy_runtime::ownership::{self, Access};
use catallaxy_runtime::{panic, try_push, try_vec, ObjKind, RtResult, Trap, Value};
use catallaxy_vm::slow;
use catallaxy_vm::{NativeFn, Vm};

use crate::*;

/// The most items `List.with_capacity` reserves.
const MAX_RESERVED: usize = 1 << 24;

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "List.new" => |vm, _| new_list(vm, Vec::new()),
        "List.with_capacity" => |vm, a| {
            let n = int(a[0])?;
            if n < 0 {
                return panic("ArgumentError: negative capacity");
            }
            // A hint: at most MAX_RESERVED items are reserved up front, so a large guess
            // cannot claim memory the list never uses.
            let items = vm.m.try_vec_collecting((n as usize).min(MAX_RESERVED))?;
            new_list(vm, items)
        },
        "List.filled" => |vm, a| {
            let n = int(a[0])?;
            if n < 0 {
                return panic("ArgumentError: negative size");
            }
            let mut items = vm.m.try_vec_collecting(n as usize)?;
            items.resize(n as usize, a[1]);
            new_list(vm, items)
        },
        "List.length" => |vm, a| Ok(Value::small_int(list_read(vm, a[0])?.len() as i64)),
        "List.push" => |vm, a| {
            slow::list_push(vm, a[0], a[1])?;
            Ok(Value::NIL)
        },
        "List.pop" => |vm, a| {
            list_read(vm, a[0])?;
            ownership::write_list(&vm.m, a[0], |l, _| unsafe { l.pop() }.unwrap_or(Value::NIL))
        },
        "List.shift" => |vm, a| list_write(vm, a[0], &[], |v| if v.is_empty() { Value::NIL } else { v.remove(0) }),
        "List.unshift" => |vm, a| {
            list_write(vm, a[0], &[a[1]], |v| {
                v.insert(0, a[1]);
                Value::NIL
            })
        },
        "List.insert" => |vm, a| {
            let i = int(a[1])?;
            let item = a[2];
            list_write(vm, a[0], &[item], |v| {
                let n = v.len() as i64;
                let idx = if i < 0 { i + n + 1 } else { i };
                if idx < 0 || idx > n {
                    return Err(Trap::Panic(format!("IndexError: index {i} out of range for insert into length {n}")));
                }
                v.insert(idx as usize, item);
                Ok(Value::NIL)
            })?
        },
        "List.remove_at" => |vm, a| {
            let i = int(a[1])?;
            list_write(vm, a[0], &[], |v| {
                let n = v.len() as i64;
                let idx = if i < 0 { i + n } else { i };
                if idx < 0 || idx >= n {
                    return Err(Trap::Panic(format!("IndexError: index {i} out of range for length {n}")));
                }
                Ok(v.remove(idx as usize))
            })?
        },
        "List.clear" => |vm, a| {
            list_write(vm, a[0], &[], |v| {
                v.clear();
                Value::NIL
            })
        },
        "List.concat" => |vm, a| {
            let h = &list_of(a[0])?.header;
            let other = list_items(vm, a[1])?;
            if ownership::check_write(&vm.m, h)? == Access::Shared {
                for &x in &other {
                    ownership::publish_into_shared(&vm.m, x)?;
                }
            }
            let grown = list_write(vm, a[0], &[], |v| v.try_reserve(other.len()).map(|_| v.extend_from_slice(&other)))?;
            match grown {
                Ok(()) => Ok(Value::NIL),
                Err(_) => catallaxy_runtime::refused(format!("cannot grow a list by {} items", other.len())),
            }
        },
        "List.[]" => |vm, a| slow::list_get(vm, a[0], a[1]),
        "__vacant" => |_, _| Ok(Value::NIL),
        "List.[]=" => |vm, a| {
            slow::list_set(vm, a[0], a[1], a[2])?;
            Ok(Value::NIL)
        },
        "List.get" => |vm, a| {
            let i = int(a[1])?;
            let l = list_read(vm, a[0])?;
            let n = l.len() as i64;
            let idx = if i < 0 { i + n } else { i };
            Ok(if idx < 0 { None } else { l.get(idx as usize) }.unwrap_or(Value::NIL))
        },
        "List.slice" => |vm, a| {
            let (start, len) = (int(a[1])?, int(a[2])?);
            let items = list_items(vm, a[0])?;
            let n = items.len() as i64;
            let s0 = if start < 0 { (start + n).max(0) } else { start.min(n) };
            let e0 = s0.saturating_add(len.max(0)).min(n);
            vm.m.try_alloc_list_from_slice(&items[s0 as usize..e0 as usize], (e0 - s0) as usize)
        },
        "List.dup" => |vm, a| {
            let items = list_items(vm, a[0])?;
            new_list(vm, items)
        },
        "List.reverse" => |vm, a| {
            let mut items = list_items(vm, a[0])?;
            items.reverse();
            new_list(vm, items)
        },
        "List.reverse_in_place" => |vm, a| {
            list_write(vm, a[0], &[], |v| {
                v.reverse();
                Value::NIL
            })
        },
        "List.swap" => |vm, a| {
            let (i, j) = (int(a[1])?, int(a[2])?);
            list_read(vm, a[0])?;
            ownership::write_list(&vm.m, a[0], |l, _| {
                let n = l.len() as i64;
                let fix = |x: i64| if x < 0 { x + n } else { x };
                let (i2, j2) = (fix(i), fix(j));
                if i2 < 0 || i2 >= n || j2 < 0 || j2 >= n {
                    return Err(Trap::Panic(format!("IndexError: swap({i}, {j}) out of range for length {n}")));
                }
                let (x, y) = (l.get_local(i2 as usize), l.get_local(j2 as usize));
                unsafe {
                    l.set(i2 as usize, y.unwrap_or(Value::NIL));
                    l.set(j2 as usize, x.unwrap_or(Value::NIL));
                }
                Ok(Value::NIL)
            })?
        },
        "List.truncate" => |vm, a| {
            let n = int(a[1])?.max(0) as usize;
            list_write(vm, a[0], &[], |v| {
                v.truncate(n);
                Value::NIL
            })
        },
        "List.sorted" => |vm, a| {
            let items = list_items(vm, a[0])?;
            let sorted = sort_values(vm, items)?;
            new_list(vm, sorted)
        },
        "List.sort_in_place" => |vm, a| {
            let items = list_items(vm, a[0])?;
            let sorted = sort_values(vm, items)?;
            list_write(vm, a[0], &[], |v| {
                v.clear();
                v.extend(sorted);
                Value::NIL
            })
        },
        "List.sort_by_keys" => |vm, a| {
            let items = list_items(vm, a[0])?;
            let keys = list_items(vm, a[1])?;
            if keys.len() != items.len() {
                return panic("ArgumentError: keys and items differ in length");
            }
            let order = sort_indices(vm, &keys)?;
            let mut out = try_vec(order.len())?;
            out.extend(order.into_iter().map(|i| items[i]));
            new_list(vm, out)
        },

        "Dict.new" => |vm, _| Ok(vm.m.alloc_dict(DictTable::new())),
        "Set.new" => |vm, _| Ok(vm.m.alloc_set(DictTable::new())),
        "Dict.length" | "Set.length" => |vm, a| dict_read(vm, a[0], |t| Value::small_int(t.len() as i64)),
        "Dict.[]" | "Dict.get" => |vm, a| Ok(slow::dict_get(vm, a[0], a[1])?.unwrap_or(Value::NIL)),
        "Dict.[]=" => |vm, a| {
            slow::dict_set(vm, a[0], a[1], a[2])?;
            Ok(Value::NIL)
        },
        "Dict.has_key" | "Set.contains" => |vm, a| Ok(Value::bool(ops::dict_lookup(vm, a[0], a[1])?.is_some())),
        "Dict.delete" => |vm, a| dict_delete(vm, a[0], a[1]),
        "Set.remove" => |vm, a| Ok(Value::bool(!dict_delete(vm, a[0], a[1])?.is_empty_marker())),
        "Dict.clear" | "Set.clear" => |vm, a| {
            ownership::write_dict(&vm.m, a[0], |t, access| unsafe { t.clear(access) })?;
            Ok(Value::NIL)
        },
        "Dict.keys" | "Set.to_list" => |vm, a| {
            let keys = dict_read(vm, a[0], |t| table_items(t, |e| e.key))??;
            new_list(vm, keys)
        },
        "Dict.values" => |vm, a| {
            let vals = dict_read(vm, a[0], |t| table_items(t, |e| e.value))??;
            new_list(vm, vals)
        },
        "Dict.entries" => |vm, a| {
            let pairs = dict_read(vm, a[0], |t| table_items(t, |e| (e.key, e.value)))??;
            let mut items = try_vec(pairs.len())?;
            items.extend(pairs.into_iter().map(|(k, v)| vm.m.alloc_tuple(&[k, v])));
            new_list(vm, items)
        },
        "Dict.dup" => |vm, a| {
            let t = dict_read(vm, a[0], |t| t.try_clone())?.or_else(table_refused)?;
            Ok(vm.m.alloc_dict(t))
        },
        "Set.dup" => |vm, a| {
            let t = dict_read(vm, a[0], |t| t.try_clone())?.or_else(table_refused)?;
            Ok(vm.m.alloc_set(t))
        },
        "Set.add" => |vm, a| {
            if ops::dict_lookup(vm, a[0], a[1])?.is_some() {
                return Ok(Value::FALSE);
            }
            slow::dict_set(vm, a[0], a[1], Value::TRUE)?;
            Ok(Value::TRUE)
        },
        "Set.from" => |vm, a| {
            let items = list_items(vm, a[0])?;
            let table = DictTable::try_with_capacity(items.len()).or_else(table_refused)?;
            let set = vm.m.alloc_set(table);
            vm.extra_roots().push(set);
            let mut res = Ok(());
            for x in items {
                if let Err(e) = slow::dict_set(vm, set, x, Value::TRUE) {
                    res = Err(e);
                    break;
                }
            }
            vm.extra_roots().pop();
            res?;
            Ok(set)
        },

        "Range.first" => |_, a| Ok(range_of(a[0])?.get(0)),
        "Range.last" => |_, a| Ok(range_of(a[0])?.get(1)),
        "Range.is_exclusive" => |_, a| Ok(range_of(a[0])?.get(2)),

        "StringBuilder.new" => |vm, _| Ok(vm.m.alloc_string_builder(String::new())),
        "StringBuilder.push" => |vm, a| {
            let x = s(a[1])?.to_string();
            sb_push(vm, a[0], &x)
        },
        "StringBuilder.push_value" => |vm, a| {
            let x = to_s(vm, a[1])?;
            sb_push(vm, a[0], &x)
        },
        "StringBuilder.push_char" => |vm, a| {
            let c = int(a[1])?;
            match u32::try_from(c).ok().and_then(char::from_u32) {
                Some(ch) => sb_push(vm, a[0], ch.encode_utf8(&mut [0; 4])),
                None => panic(format!("RangeError: {c} is not a valid character code")),
            }
        },
        "StringBuilder.to_s" => |vm, a| {
            // A snapshot (a shared builder may change once its lock is released), then the
            // string, each raising a MemoryError if refused.
            let sb = sb_of(a[0])?;
            let text = ownership::with_read(&vm.m, a[0], || {
                let b = unsafe { &*sb.buf.get() };
                let mut t = String::new();
                t.try_reserve_exact(b.len()).map(|_| {
                    t.push_str(b);
                    t
                })
            })?;
            match text {
                Ok(t) => vm.m.try_alloc_str(&t),
                Err(_) => sb_refused(),
            }
        },
        "StringBuilder.length" => |vm, a| {
            let sb = sb_of(a[0])?;
            let n = ownership::with_read(&vm.m, a[0], || unsafe { (*sb.buf.get()).chars().count() })?;
            Ok(Value::small_int(n as i64))
        },
        "StringBuilder.clear" => |vm, a| sb_write(vm, a[0], |b| b.clear()),
        _ => return None,
    })
}

/// Checks that `l` is a list this thread may read. List reads never lock.
pub(crate) fn list_read<'a>(vm: &Vm, l: Value) -> RtResult<&'a ListObj> {
    ownership::read_list(&vm.m, l)
}

/// Mutates a list; `incoming` values are published first when the list is shared.
pub(crate) fn list_write<R>(
    vm: &Vm,
    l: Value,
    incoming: &[Value],
    f: impl FnOnce(&mut Vec<Value>) -> R,
) -> RtResult<R> {
    let h = &list_of(l)?.header;
    if ownership::check_write(&vm.m, h)? == Access::Shared {
        for &x in incoming {
            ownership::publish_into_shared(&vm.m, x)?;
        }
    }
    match ownership::write_list(&vm.m, l, |list, access| unsafe { list.try_modify(access, f) })? {
        Ok(r) => Ok(r),
        Err(layout) => catallaxy_runtime::refused(format!("cannot grow a list to {} bytes", layout.size())),
    }
}

fn dict_read<R>(vm: &Vm, d: Value, f: impl Fn(&DictTable) -> R) -> RtResult<R> {
    ownership::read_dict(&vm.m, d, f)
}

/// `f` of each entry of `t`, or a `MemoryError` panic if the vector cannot be had.
fn table_items<T>(t: &DictTable, f: impl Fn(catallaxy_runtime::dict::Entry) -> T) -> RtResult<Vec<T>> {
    let mut out = try_vec(t.len())?;
    for e in t.iter() {
        try_push(&mut out, f(e))?;
    }
    Ok(out)
}

#[cold]
fn table_refused<T>(layout: std::alloc::Layout) -> RtResult<T> {
    catallaxy_runtime::refused(format!("cannot allocate a table of {} bytes", layout.size()))
}

/// The `Range` instance behind `v`, or a `TypeError` panic.
fn range_of<'a>(v: Value) -> RtResult<&'a InstanceObj> {
    instance_of(v, catallaxy_runtime::class::ids::RANGE, "Range")
}

/// Removes `k`; returns its value, nil (dict, absent) or the empty marker (absent, for sets).
fn dict_delete(vm: &mut Vm, d: Value, k: Value) -> RtResult<Value> {
    loop {
        let version = ownership::read_dict(&vm.m, d, |t| t.version())?;
        let Some(idx) = ops::dict_lookup(vm, d, k)? else {
            return Ok(if kind_of(d) == ObjKind::Set { Value::EMPTY } else { Value::NIL });
        };
        // Act only if no user `==` changed the table since the lookup.
        let r = ownership::write_dict(&vm.m, d, |t, access| {
            (t.version() == version).then(|| unsafe { t.remove_at(idx, access) })
        })?;
        if let Some(v) = r {
            return Ok(v);
        }
    }
}

/// Appends `x` to a builder. If its buffer cannot grow, collects (garbage may hold the memory)
/// and tries once more, then raises a `MemoryError`.
fn sb_push(vm: &Vm, b: Value, x: &str) -> RtResult<Value> {
    let sb = sb_of(b)?;
    for retry in [false, true] {
        let grown = ownership::with_write(&vm.m, b, |_| {
            let buf = unsafe { &mut *sb.buf.get() };
            buf.try_reserve(x.len()).map(|_| buf.push_str(x))
        })?;
        if grown.is_ok() {
            return Ok(Value::NIL);
        }
        if !retry {
            catallaxy_gc::collect_to_free(&vm.m.rec);
        }
    }
    sb_refused()
}

fn sb_refused() -> RtResult<Value> {
    catallaxy_runtime::refused("cannot grow a StringBuilder")
}

fn sb_write(vm: &Vm, b: Value, f: impl FnOnce(&mut String)) -> RtResult<Value> {
    let sb = sb_of(b)?;
    ownership::with_write(&vm.m, b, |_| f(unsafe { &mut *sb.buf.get() }))?;
    Ok(Value::NIL)
}

/// Sorts values by `<=>` (stable). Fast paths avoid callbacks for primitive lists.
pub(crate) fn sort_values(vm: &mut Vm, mut items: Vec<Value>) -> RtResult<Vec<Value>> {
    if items.iter().all(|v| v.is_small_int()) {
        // Equal small ints are the same value, so an unstable sort (which needs no buffer) is
        // as good as a stable one.
        items.sort_unstable_by_key(|v| v.as_small_int());
        return Ok(items);
    }
    // The stable sorts below allocate a buffer of up to `items.len()` values.
    check_sort_buffer(items.len())?;
    if items.iter().all(|v| v.is_float() || v.is_small_int()) {
        let mut err = false;
        items.sort_by(|a, b| {
            let (x, y) = (as_f(*a), as_f(*b));
            x.partial_cmp(&y).unwrap_or_else(|| {
                err = true;
                CmpOrdering::Equal
            })
        });
        if err {
            return panic("ArgumentError: comparison of Float with NaN failed");
        }
        return Ok(items);
    }
    if items.iter().all(|v| str_of(*v).is_some()) {
        items.sort_by(|a, b| str_of(*a).unwrap().cmp(str_of(*b).unwrap()));
        return Ok(items);
    }
    let order = sort_indices(vm, &items)?;
    let mut out = try_vec(order.len())?;
    out.extend(order.into_iter().map(|i| items[i]));
    Ok(out)
}

/// Raises a `MemoryError` if a stable sort of `n` values could not have its buffer now. The
/// standard library's sort allocates it without a way to report refusal: this takes and frees
/// one as large first (sorts of up to 512 values use a buffer on the stack).
fn check_sort_buffer(n: usize) -> RtResult<()> {
    if n > 512 {
        drop(try_vec::<Value>(n)?);
    }
    Ok(())
}

fn as_f(v: Value) -> f64 {
    if v.is_float() {
        v.as_f64()
    } else {
        v.as_small_int() as f64
    }
}

/// Stable sort order of `keys`, calling user `<=>` where needed. The keys stay rooted.
pub(crate) fn sort_indices(vm: &mut Vm, keys: &[Value]) -> RtResult<Vec<usize>> {
    let mut idx: Vec<usize> = try_vec(keys.len())?;
    idx.extend(0..keys.len());
    // Ties broken by index make an unstable sort (which needs no buffer) stable.
    if keys.iter().all(|v| v.is_small_int()) {
        idx.sort_unstable_by_key(|&i| (keys[i].as_small_int(), i));
        return Ok(idx);
    }
    if keys.iter().all(|v| str_of(*v).is_some()) {
        idx.sort_unstable_by(|&a, &b| str_of(keys[a]).unwrap().cmp(str_of(keys[b]).unwrap()).then(a.cmp(&b)));
        return Ok(idx);
    }
    let mut buf = try_vec(idx.len())?;
    buf.extend_from_slice(&idx);
    // Keys may be compared by user code, which can run the collector: root them.
    let base = vm.extra_roots().len();
    vm.extra_roots().extend_from_slice(keys);
    let mut error: Option<Trap> = None;
    // Merge sort on indices (no unsafe partial states visible to the collector).
    let r = merge_sort(vm, keys, &mut idx, buf, &mut error);
    vm.extra_roots().truncate(base);
    let _ = r;
    match error {
        Some(e) => Err(e),
        None => Ok(idx),
    }
}

/// Sorts `idx` by `keys`; `buf` is scratch space holding a copy of `idx`.
fn merge_sort(
    vm: &mut Vm,
    keys: &[Value],
    idx: &mut Vec<usize>,
    mut buf: Vec<usize>,
    error: &mut Option<Trap>,
) -> bool {
    let n = idx.len();
    let mut width = 1;
    while width < n {
        let mut i = 0;
        while i < n {
            let mid = (i + width).min(n);
            let hi = (i + 2 * width).min(n);
            let (mut a, mut b, mut k) = (i, mid, i);
            while a < mid && b < hi {
                let ord = if error.is_some() {
                    CmpOrdering::Equal
                } else {
                    match ops::compare_or_panic(vm, keys[idx[b]], keys[idx[a]]) {
                        Ok(o) => o,
                        Err(e) => {
                            *error = Some(e);
                            CmpOrdering::Equal
                        }
                    }
                };
                if ord == CmpOrdering::Less {
                    buf[k] = idx[b];
                    b += 1;
                } else {
                    buf[k] = idx[a];
                    a += 1;
                }
                k += 1;
            }
            while a < mid {
                buf[k] = idx[a];
                a += 1;
                k += 1;
            }
            while b < hi {
                buf[k] = idx[b];
                b += 1;
                k += 1;
            }
            i += 2 * width;
        }
        std::mem::swap(idx, &mut buf);
        width *= 2;
    }
    error.is_none()
}
