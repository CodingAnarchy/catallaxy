//! Structural operations on values: equality, hashing, ordering, formatting, and arbitrary-precision
//! integer arithmetic.
//!
//! Operations that may need user code (`==`, `hash`, `<=>`, `to_s` defined on a class) go through
//! the [`Interop`] trait, which the VM implements. Implementations never hold a borrow of an object's
//! contents across such a call; they re-read by index instead.

use std::cmp::Ordering as CmpOrdering;
use std::fmt::Write as _;
use std::hash::Hasher;

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};
use rustc_hash::FxHasher;

use crate::class::{heap_class, ids, Class, ObjKind};
use crate::dict::Probe;
use crate::mutator::{panic, Mutator, RtResult, Trap};
use crate::object::*;
use crate::ownership::{read_dict_unchecked, read_list, with_read};
use crate::symbol::{intern, Sym};
use crate::value::{fits_small, Value};

/// Calls back into the VM for operations defined in Catallaxy code.
pub trait Interop {
    fn mutator(&mut self) -> &mut Mutator;
    /// Calls method `name` on `recv` if its class defines it (not interface defaults).
    /// Returns `Ok(None)` when the class has no such method.
    fn call_method(&mut self, recv: Value, name: Sym, args: &[Value]) -> RtResult<Option<Value>>;
}

pub struct Syms {
    pub eq: Sym,
    pub hash: Sym,
    pub cmp: Sym,
    pub to_s: Sym,
    pub inspect: Sym,
}

pub static SYMS: std::sync::LazyLock<Syms> = std::sync::LazyLock::new(|| Syms {
    eq: intern("=="),
    hash: intern("hash"),
    cmp: intern("<=>"),
    to_s: intern("to_s"),
    inspect: intern("inspect"),
});

pub fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(b);
    h.write_usize(b.len());
    mix(h.finish())
}

#[inline]
fn mix(mut x: u64) -> u64 {
    // splitmix64 finalizer: spreads Fx output across all bits for the open-addressing table.
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[inline]
pub fn hash_int(i: i64) -> u64 {
    mix(i as u64 ^ 0x5151_5151)
}

fn user_class(h: &catallaxy_gc::Header) -> &'static Class {
    unsafe { heap_class(h) }
}

fn is_user_instance(cls: &Class) -> bool {
    cls.kind == ObjKind::Instance && cls.id >= ids::FIRST_USER
}

// ---------------------------------------------------------------------------------------------
// Numbers

pub fn is_int(v: Value) -> bool {
    v.is_small_int() || (v.is_ptr() && kind_of(v) == ObjKind::BigInt)
}

/// The big integer behind an Int value (callers check `is_int`; anything else reads as 0).
fn bigint_ref(v: Value) -> Option<&'static BigInt> {
    if v.is_ptr() && kind_of(v) == ObjKind::BigInt {
        Some(&unsafe { as_bigint(v) }.value)
    } else {
        None
    }
}

pub fn to_bigint(v: Value) -> BigInt {
    if v.is_small_int() {
        BigInt::from(v.as_small_int())
    } else {
        bigint_ref(v).cloned().unwrap_or_default()
    }
}

pub fn int_to_f64(v: Value) -> f64 {
    if v.is_small_int() {
        v.as_small_int() as f64
    } else {
        bigint_ref(v).and_then(|b| b.to_f64()).unwrap_or(f64::NAN)
    }
}

/// The value as i64 if it is an Int that fits.
#[inline]
pub fn int_as_i64(v: Value) -> Option<i64> {
    if v.is_small_int() {
        Some(v.as_small_int())
    } else {
        bigint_ref(v).and_then(|b| b.to_i64())
    }
}

#[inline]
pub fn int_add(m: &mut Mutator, a: Value, b: Value) -> Value {
    if Value::both_small_ints(a, b) {
        return m.int(a.as_small_int() + b.as_small_int());
    }
    m.int_from_big(to_bigint(a) + to_bigint(b))
}

#[inline]
pub fn int_sub(m: &mut Mutator, a: Value, b: Value) -> Value {
    if Value::both_small_ints(a, b) {
        return m.int(a.as_small_int() - b.as_small_int());
    }
    m.int_from_big(to_bigint(a) - to_bigint(b))
}

#[inline]
pub fn int_mul(m: &mut Mutator, a: Value, b: Value) -> Value {
    if Value::both_small_ints(a, b) {
        if let Some(p) = a.as_small_int().checked_mul(b.as_small_int()) {
            return m.int(p);
        }
    }
    m.int_from_big(to_bigint(a) * to_bigint(b))
}

fn zero_div<T>() -> RtResult<T> {
    panic("ZeroDivisionError: divided by 0")
}

/// Floor division (rounds toward negative infinity).
pub fn int_div(m: &mut Mutator, a: Value, b: Value) -> RtResult<Value> {
    if Value::both_small_ints(a, b) {
        let (x, y) = (a.as_small_int(), b.as_small_int());
        if y == 0 {
            return zero_div();
        }
        return Ok(m.int(Integer::div_floor(&x, &y)));
    }
    let y = to_bigint(b);
    if y.is_zero() {
        return zero_div();
    }
    Ok(m.int_from_big(to_bigint(a).div_floor(&y)))
}

/// Floor modulo: the result has the sign of the divisor.
pub fn int_mod(m: &mut Mutator, a: Value, b: Value) -> RtResult<Value> {
    if Value::both_small_ints(a, b) {
        let (x, y) = (a.as_small_int(), b.as_small_int());
        if y == 0 {
            return zero_div();
        }
        return Ok(m.int(Integer::mod_floor(&x, &y)));
    }
    let y = to_bigint(b);
    if y.is_zero() {
        return zero_div();
    }
    Ok(m.int_from_big(to_bigint(a).mod_floor(&y)))
}

pub fn int_pow(m: &mut Mutator, a: Value, b: Value) -> RtResult<Value> {
    let Some(e) = int_as_i64(b) else {
        return panic(if sign_of(b) < 0 {
            "ArgumentError: negative exponent for Int ** Int; use a Float power"
        } else {
            "RangeError: exponent too large"
        });
    };
    if e < 0 {
        return panic("ArgumentError: negative exponent for Int ** Int; use a Float power");
    }
    if e > u32::MAX as i64 {
        return panic("RangeError: exponent too large");
    }
    if let Some(x) = a.is_small_int().then(|| a.as_small_int()) {
        if let Some(p) = x.checked_pow(e as u32) {
            return Ok(m.int(p));
        }
    }
    Ok(m.int_from_big(num_traits::pow(to_bigint(a), e as usize)))
}

pub fn int_neg(m: &mut Mutator, a: Value) -> Value {
    if a.is_small_int() {
        return m.int(-a.as_small_int());
    }
    m.int_from_big(-to_bigint(a))
}

pub fn int_bitop(m: &mut Mutator, op: u8, a: Value, b: Value) -> RtResult<Value> {
    if Value::both_small_ints(a, b) {
        let (x, y) = (a.as_small_int(), b.as_small_int());
        let r = match op {
            b'&' => x & y,
            b'|' => x | y,
            b'^' => x ^ y,
            b'<' => {
                if y < 0 {
                    return panic("ArgumentError: negative shift count");
                }
                if y < 16 && fits_small(x << y) && (x << y) >> y == x {
                    x << y
                } else {
                    return Ok(m.int_from_big(BigInt::from(x) << (y as usize)));
                }
            }
            b'>' => {
                if y < 0 {
                    return panic("ArgumentError: negative shift count");
                }
                x >> y.min(63)
            }
            _ => unreachable!(),
        };
        return Ok(m.int(r));
    }
    let (x, y) = (to_bigint(a), to_bigint(b));
    let r = match op {
        b'&' => x & y,
        b'|' => x | y,
        b'^' => x ^ y,
        b'<' | b'>' => {
            let Some(n) = y.to_usize() else {
                return panic(if y.is_negative() {
                    "ArgumentError: negative shift count"
                } else {
                    "RangeError: shift count too large"
                });
            };
            if op == b'<' {
                x << n
            } else {
                x >> n
            }
        }
        _ => unreachable!(),
    };
    Ok(m.int_from_big(r))
}

pub fn int_cmp(a: Value, b: Value) -> CmpOrdering {
    if Value::both_small_ints(a, b) {
        return a.as_small_int().cmp(&b.as_small_int());
    }
    to_bigint(a).cmp(&to_bigint(b))
}

/// Truncates a float toward zero into an Int (panics on NaN/infinity).
pub fn float_to_int(m: &mut Mutator, f: f64) -> RtResult<Value> {
    if !f.is_finite() {
        return panic(format!("FloatDomainError: {}", format_float(f)));
    }
    let t = f.trunc();
    if t.abs() < 1e15 {
        return Ok(m.int(t as i64));
    }
    use num_traits::FromPrimitive;
    Ok(m.int_from_big(BigInt::from_f64(t).unwrap()))
}

fn num_f64(v: Value) -> Option<f64> {
    if v.is_float() {
        Some(v.as_f64())
    } else if is_int(v) {
        Some(int_to_f64(v))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Formatting

/// Formats a float the way Catallaxy prints it: always with a decimal point or exponent.
pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e16).contains(&a) {
        // Shortest round-trip mantissa, Ruby-style exponent: 1.0e+20, 2.5e-07.
        let s = format!("{:e}", f);
        let (mant, exp) = s.split_once('e').unwrap();
        let mant = if mant.contains('.') { mant.to_string() } else { format!("{mant}.0") };
        let e: i32 = exp.parse().unwrap();
        return format!("{mant}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    let s = format!("{}", f);
    if s.contains('.') {
        s
    } else {
        format!("{s}.0")
    }
}

pub fn format_int(v: Value) -> String {
    if v.is_small_int() {
        v.as_small_int().to_string()
    } else {
        to_bigint(v).to_string()
    }
}

/// Writes `s` as a quoted string literal.
pub fn quote_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '#' => out.push('#'),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `to_s`: strings print raw, everything else as `inspect`.
pub fn to_s(ix: &mut dyn Interop, v: Value) -> RtResult<String> {
    if let Some(s) = str_of(v) {
        return Ok(s.to_string());
    }
    let mut out = String::new();
    write_value(ix, v, &mut out, false, 0)?;
    Ok(out)
}

/// `inspect`: strings print quoted.
pub fn inspect(ix: &mut dyn Interop, v: Value) -> RtResult<String> {
    let mut out = String::new();
    write_value(ix, v, &mut out, true, 0)?;
    Ok(out)
}

fn write_value(ix: &mut dyn Interop, v: Value, out: &mut String, quoted: bool, depth: usize) -> RtResult<()> {
    if depth > 64 {
        out.push_str("...");
        return Ok(());
    }
    if v.is_float() {
        out.push_str(&format_float(v.as_f64()));
        return Ok(());
    }
    if v.is_small_int() {
        let _ = write!(out, "{}", v.as_small_int());
        return Ok(());
    }
    if v.is_nil() {
        out.push_str("nil");
        return Ok(());
    }
    if v.is_bool() {
        out.push_str(if v.is_true() { "true" } else { "false" });
        return Ok(());
    }
    if !v.is_ptr() {
        out.push_str("<?>");
        return Ok(());
    }
    let h = unsafe { &*v.as_ptr() };
    let cls = user_class(h);
    // User-defined to_s / inspect take precedence.
    if cls.id >= ids::FIRST_USER || cls.kind == ObjKind::Instance && cls.id != ids::RANGE {
        let name = if quoted && cls.lookup(SYMS.inspect).is_some() { SYMS.inspect } else { SYMS.to_s };
        if cls.lookup(name).is_some() {
            if let Some(r) = ix.call_method(v, name, &[])? {
                match str_of(r) {
                    Some(s) => out.push_str(s),
                    None => return panic(format!("TypeError: {}#{} must return a String", cls.name, name)),
                }
                return Ok(());
            }
        }
    }
    crate::stack::check()?;
    let m = ix.mutator() as *mut Mutator;
    let m = unsafe { &*m };
    match cls.kind {
        ObjKind::Str => {
            let s = unsafe { as_str_obj(v) }.as_str();
            if quoted {
                quote_str(out, s)
            } else {
                out.push_str(s)
            }
        }
        ObjKind::BigInt => out.push_str(&format_int(v)),
        ObjKind::List => {
            out.push('[');
            let mut i = 0;
            loop {
                let item = read_list(m, v)?.get(i);
                let Some(item) = item else { break };
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(ix, item, out, true, depth + 1)?;
                i += 1;
            }
            out.push(']');
        }
        ObjKind::Tuple => {
            out.push('(');
            let elems = unsafe { as_tuple(v) }.elems();
            for (i, &e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(ix, e, out, true, depth + 1)?;
            }
            if elems.len() == 1 {
                out.push(',');
            }
            out.push(')');
        }
        // SAFETY (the unchecked reads): matched on the kind.
        ObjKind::Dict | ObjKind::Set => {
            let is_set = cls.kind == ObjKind::Set;
            out.push_str(if is_set { "Set[" } else { "[" });
            let n = unsafe { read_dict_unchecked(m, v, |t| t.raw_len()) }?;
            let mut first = true;
            for i in 0..n {
                let e = unsafe { read_dict_unchecked(m, v, |t| t.raw_entry(i)) }?;
                let Some(e) = e else { continue };
                if !first {
                    out.push_str(", ");
                }
                first = false;
                write_value(ix, e.key, out, true, depth + 1)?;
                if !is_set {
                    out.push_str(": ");
                    write_value(ix, e.value, out, true, depth + 1)?;
                }
            }
            if first && !is_set {
                out.push(':');
            }
            out.push(']');
        }
        ObjKind::Instance if cls.id == ids::RANGE => {
            let r = unsafe { as_instance(v) };
            write_value(ix, r.get(0), out, true, depth + 1)?;
            out.push_str(if r.get(2).is_true() { "..." } else { ".." });
            write_value(ix, r.get(1), out, true, depth + 1)?;
        }
        ObjKind::Instance => {
            let o = unsafe { as_instance(v) };
            out.push_str(&cls.name);
            let n = cls.nfields();
            if cls.variant_tag.is_some() {
                if n > 0 {
                    out.push('(');
                    for i in 0..n {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_value(ix, o.get(i), out, true, depth + 1)?;
                    }
                    out.push(')');
                }
            } else {
                out.push('(');
                for i in 0..n {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{}: ", cls.field_names[i]);
                    let fv = if h.state() == catallaxy_gc::ObjState::Shared { o.get_acquire(i) } else { o.get(i) };
                    write_value(ix, fv, out, true, depth + 1)?;
                }
                out.push(')');
            }
        }
        ObjKind::StringBuilder => {
            let s = with_read(m, v, || unsafe { (*as_sb(v).buf.get()).clone() })?;
            if quoted {
                quote_str(out, &s)
            } else {
                out.push_str(&s)
            }
        }
        ObjKind::Closure => out.push_str("#<fn>"),
        _ => {
            let _ = write!(out, "#<{}>", cls.name);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Equality and hashing

/// `==`: structural for builtins, enums and tuples; user `==` for classes defining it; identity
/// otherwise. `Int == Float` compares numerically.
pub fn values_eq(ix: &mut dyn Interop, a: Value, b: Value) -> RtResult<bool> {
    if a.0 == b.0 {
        return Ok(!(a.is_float() && a.as_f64().is_nan()));
    }
    if a.is_float() || b.is_float() || a.is_small_int() || b.is_small_int() {
        return Ok(match (num_f64(a), num_f64(b)) {
            (Some(x), Some(y)) if a.is_float() || b.is_float() => x == y,
            _ => is_int(a) && is_int(b) && int_cmp(a, b) == CmpOrdering::Equal,
        });
    }
    if !a.is_ptr() || !b.is_ptr() {
        return Ok(false);
    }
    let (ha, hb) = unsafe { (&*a.as_ptr(), &*b.as_ptr()) };
    let (ca, cb) = (user_class(ha), user_class(hb));
    if is_user_instance(ca) && ca.lookup(SYMS.eq).is_some() {
        if let Some(r) = ix.call_method(a, SYMS.eq, &[b])? {
            return Ok(r.is_true());
        }
    }
    if ca.kind != cb.kind {
        return Ok(false);
    }
    let m = unsafe { &*(ix.mutator() as *mut Mutator) };
    match ca.kind {
        ObjKind::Str => Ok(unsafe { as_str_obj(a).as_str() == as_str_obj(b).as_str() }),
        ObjKind::BigInt => Ok(unsafe { as_bigint(a).value == as_bigint(b).value }),
        ObjKind::Tuple => {
            crate::stack::check()?;
            let (ta, tb) = unsafe { (as_tuple(a), as_tuple(b)) };
            if ta.len != tb.len {
                return Ok(false);
            }
            for i in 0..ta.len {
                if !values_eq(ix, ta.elems()[i], tb.elems()[i])? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        ObjKind::List => {
            crate::stack::check()?;
            let len = |v: Value| read_list(m, v).map(|l| l.len());
            if len(a)? != len(b)? {
                return Ok(false);
            }
            let mut i = 0;
            loop {
                let x = read_list(m, a)?.get(i);
                let y = read_list(m, b)?.get(i);
                match (x, y) {
                    (Some(x), Some(y)) => {
                        if !values_eq(ix, x, y)? {
                            return Ok(false);
                        }
                    }
                    (None, None) => return Ok(true),
                    _ => return Ok(false),
                }
                i += 1;
            }
        }
        // SAFETY (the unchecked reads): matched on the kind, which `b` shares.
        ObjKind::Dict | ObjKind::Set => {
            crate::stack::check()?;
            let len = |v: Value| unsafe { read_dict_unchecked(m, v, |t| t.len()) };
            if len(a)? != len(b)? {
                return Ok(false);
            }
            let n = unsafe { read_dict_unchecked(m, a, |t| t.raw_len()) }?;
            for i in 0..n {
                let e = unsafe { read_dict_unchecked(m, a, |t| t.raw_entry(i)) }?;
                let Some(e) = e else { continue };
                match dict_lookup(ix, b, e.key)? {
                    None => return Ok(false),
                    Some(idx) => {
                        if ca.kind == ObjKind::Dict {
                            let bv = unsafe { read_dict_unchecked(m, b, |t| t.entry(idx).value) }?;
                            if !values_eq(ix, e.value, bv)? {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
            Ok(true)
        }
        ObjKind::Instance => {
            // Enum variants and ranges compare structurally; other instances by identity.
            if ca.id != cb.id || !(ca.variant_tag.is_some() || ca.id == ids::RANGE) {
                return Ok(false);
            }
            crate::stack::check()?;
            let (oa, ob) = unsafe { (as_instance(a), as_instance(b)) };
            for i in 0..ca.nfields() {
                if !values_eq(ix, oa.get(i), ob.get(i))? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Hash consistent with [`values_eq`].
pub fn hash_value(ix: &mut dyn Interop, v: Value) -> RtResult<u64> {
    if v.is_small_int() {
        return Ok(hash_int(v.as_small_int()));
    }
    if v.is_float() {
        let f = v.as_f64();
        if f.fract() == 0.0 && f.abs() < 1e15 {
            return Ok(hash_int(f as i64));
        }
        return Ok(mix(f.to_bits()));
    }
    if !v.is_ptr() {
        return Ok(mix(v.0));
    }
    let h = unsafe { &*v.as_ptr() };
    let cls = user_class(h);
    let m = unsafe { &*(ix.mutator() as *mut Mutator) };
    match cls.kind {
        ObjKind::Str => Ok(unsafe { as_str_obj(v) }.hash_code()),
        ObjKind::BigInt => {
            let b = &unsafe { as_bigint(v) }.value;
            Ok(hash_bytes(&b.to_signed_bytes_le()))
        }
        ObjKind::Tuple => {
            crate::stack::check()?;
            let mut acc = 0x7475_706c_u64;
            for &e in unsafe { as_tuple(v) }.elems() {
                acc = mix(acc ^ hash_value(ix, e)?);
            }
            Ok(acc)
        }
        ObjKind::List => {
            crate::stack::check()?;
            let mut acc = 0x6c69_7374_u64;
            let mut i = 0;
            loop {
                let x = read_list(m, v)?.get(i);
                let Some(x) = x else { break };
                acc = mix(acc ^ hash_value(ix, x)?);
                i += 1;
            }
            Ok(acc)
        }
        // SAFETY (the unchecked reads): matched on the kind.
        ObjKind::Dict | ObjKind::Set => {
            crate::stack::check()?;
            let mut acc = 0x6469_6374_u64;
            let n = unsafe { read_dict_unchecked(m, v, |t| t.raw_len()) }?;
            for i in 0..n {
                let e = unsafe { read_dict_unchecked(m, v, |t| t.raw_entry(i)) }?;
                if let Some(e) = e {
                    let hv = if cls.kind == ObjKind::Dict { hash_value(ix, e.value)? } else { 0 };
                    acc ^= mix(e.hash ^ hv.rotate_left(17));
                }
            }
            Ok(acc)
        }
        ObjKind::Instance => {
            if is_user_instance(cls) && cls.lookup(SYMS.hash).is_some() {
                if let Some(r) = ix.call_method(v, SYMS.hash, &[])? {
                    return Ok(if r.is_small_int() { hash_int(r.as_small_int()) } else { mix(r.0) });
                }
            }
            if cls.variant_tag.is_some() || cls.id == ids::RANGE {
                crate::stack::check()?;
                let o = unsafe { as_instance(v) };
                let mut acc = mix(cls.id as u64);
                for i in 0..cls.nfields() {
                    acc = mix(acc ^ hash_value(ix, o.get(i))?);
                }
                return Ok(acc);
            }
            Ok(mix(h.identity_hash() as u64))
        }
        _ => Ok(mix(h.identity_hash() as u64)),
    }
}

/// Finds `key` in a dict or set, returning its entry index.
pub fn dict_lookup(ix: &mut dyn Interop, dict: Value, key: Value) -> RtResult<Option<usize>> {
    let hash = hash_value(ix, key)?;
    dict_lookup_hashed(ix, dict, key, hash)
}

/// Raises a `TypeError` if `dict` is not a dict or set.
pub fn dict_lookup_hashed(ix: &mut dyn Interop, dict: Value, key: Value, hash: u64) -> RtResult<Option<usize>> {
    dict_of(dict)?;
    // SAFETY: checked just above.
    unsafe { dict_lookup_hashed_unchecked(ix, dict, key, hash) }
}

/// [`dict_lookup_hashed`] without the kind check.
///
/// # Safety
/// `dict` must be a dict or set, checked by the caller (static types are not enough).
pub unsafe fn dict_lookup_hashed_unchecked(
    ix: &mut dyn Interop,
    dict: Value,
    key: Value,
    hash: u64,
) -> RtResult<Option<usize>> {
    let m = unsafe { &*(ix.mutator() as *mut Mutator) };
    // SAFETY (the reads below): the caller checked that `dict` is a dict or set.
    loop {
        let (probe, version) = unsafe { read_dict_unchecked(m, dict, |t| (t.probe(hash, key), t.version())) }?;
        match probe {
            Probe::Found(i) => return Ok(Some(i)),
            Probe::Absent => return Ok(None),
            Probe::Check(cands) => {
                let mut found = None;
                for idx in cands {
                    let k = unsafe { read_dict_unchecked(m, dict, |t| t.entry(idx).key) }?;
                    if values_eq(ix, k, key)? {
                        found = Some(idx);
                        break;
                    }
                }
                let still = unsafe { read_dict_unchecked(m, dict, |t| t.version()) }?;
                if still == version {
                    return Ok(found);
                }
                // The table changed under a user `==`; probe again.
            }
        }
    }
}

/// The total order `<=>` uses for numbers: the usual one, with NaN after every other value
/// (infinity included) and equal to itself; `-0.0` and `0.0` are equal.
pub fn float_cmp(x: f64, y: f64) -> CmpOrdering {
    match x.partial_cmp(&y) {
        Some(o) => o,
        None => x.is_nan().cmp(&y.is_nan()),
    }
}

/// True if `<` / `<=` on these values compares numbers one of which is NaN: false, as IEEE 754
/// and the `Float` operators have it (only `<=>` orders NaN).
pub fn unordered_numbers(x: Value, y: Value) -> bool {
    matches!((num_f64(x), num_f64(y)), (Some(p), Some(q)) if p.is_nan() || q.is_nan())
}

/// `<=>`-style comparison. Returns None if the values are not comparable.
pub fn compare(ix: &mut dyn Interop, a: Value, b: Value) -> RtResult<Option<CmpOrdering>> {
    if is_int(a) && is_int(b) {
        return Ok(Some(int_cmp(a, b)));
    }
    if let (Some(x), Some(y)) = (num_f64(a), num_f64(b)) {
        return Ok(Some(float_cmp(x, y)));
    }
    if !a.is_ptr() || !b.is_ptr() {
        if a.is_bool() && b.is_bool() {
            return Ok(Some(a.is_true().cmp(&b.is_true())));
        }
        return Ok(None);
    }
    let (ca, cb) = unsafe { (heap_class(a.as_ptr()), heap_class(b.as_ptr())) };
    if is_user_instance(ca) && ca.lookup(SYMS.cmp).is_some() {
        if let Some(r) = ix.call_method(a, SYMS.cmp, &[b])? {
            return Ok(int_as_i64(r).map(|i| i.cmp(&0)));
        }
    }
    if ca.kind != cb.kind {
        return Ok(None);
    }
    let m = unsafe { &*(ix.mutator() as *mut Mutator) };
    match ca.kind {
        ObjKind::Str => Ok(Some(unsafe { as_str_obj(a).as_str().cmp(as_str_obj(b).as_str()) })),
        ObjKind::Tuple | ObjKind::List => {
            crate::stack::check()?;
            let get = |v: Value, i: usize| -> RtResult<Option<Value>> {
                if ca.kind == ObjKind::Tuple {
                    Ok(unsafe { as_tuple(v) }.elems().get(i).copied())
                } else {
                    read_list(m, v).map(|l| l.get(i))
                }
            };
            let mut i = 0;
            loop {
                match (get(a, i)?, get(b, i)?) {
                    (None, None) => return Ok(Some(CmpOrdering::Equal)),
                    (None, Some(_)) => return Ok(Some(CmpOrdering::Less)),
                    (Some(_), None) => return Ok(Some(CmpOrdering::Greater)),
                    (Some(x), Some(y)) => match compare(ix, x, y)? {
                        Some(CmpOrdering::Equal) => {}
                        other => return Ok(other),
                    },
                }
                i += 1;
            }
        }
        ObjKind::Instance if ca.variant_tag.is_some() && ca.enum_id == cb.enum_id => {
            let (ta, tb) = (ca.variant_tag.unwrap(), cb.variant_tag.unwrap());
            if ta != tb {
                return Ok(Some(ta.cmp(&tb)));
            }
            crate::stack::check()?;
            let (oa, ob) = unsafe { (as_instance(a), as_instance(b)) };
            for i in 0..ca.nfields() {
                match compare(ix, oa.get(i), ob.get(i))? {
                    Some(CmpOrdering::Equal) => {}
                    other => return Ok(other),
                }
            }
            Ok(Some(CmpOrdering::Equal))
        }
        _ => Ok(None),
    }
}

/// Compares or panics with a clear message.
pub fn compare_or_panic(ix: &mut dyn Interop, a: Value, b: Value) -> RtResult<CmpOrdering> {
    match compare(ix, a, b)? {
        Some(o) => Ok(o),
        None => Err(Trap::Panic(format!(
            "ArgumentError: comparison of {} with {} failed",
            crate::class::class_of(a).name,
            crate::class::class_of(b).name
        ))),
    }
}

pub fn sign_of(v: Value) -> i64 {
    if v.is_small_int() {
        v.as_small_int().signum()
    } else {
        let b = to_bigint(v);
        if b.is_negative() {
            -1
        } else if b.is_zero() {
            0
        } else {
            1
        }
    }
}
