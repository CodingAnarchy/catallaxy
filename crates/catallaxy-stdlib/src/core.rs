//! Global functions and methods of Int, Float, Bool and String.

use catallaxy_runtime::object::*;
use catallaxy_runtime::ops::{self, Interop};
use catallaxy_runtime::{panic, RtResult, Trap, Value};
use catallaxy_vm::{NativeFn, Vm};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

use crate::*;

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "puts" => puts,
        "print" => print,
        "eputs" => eputs,
        "panic" => panic_fn,
        "assert" => assert_fn,
        "assert_eq" => assert_eq,
        "inspect" => inspect,
        "compare_values" => compare_values,
        "read_line" => read_line,
        "args" => args_fn,
        "exit" => exit_fn,
        "sleep" => sleep_fn,
        "gc" => gc_fn,

        "Int.to_s" => int_to_s,
        "Int.hash" | "Float.hash" | "Bool.hash" | "String.hash" => hash,
        "Int.to_f" => int_to_f,
        "Int.to_s_radix" => int_to_s_radix,
        "Int.chr" => int_chr,
        "Int.sqrt" => int_sqrt,
        "Int.bit_length" => int_bit_length,

        "Float.to_s" => float_to_s,
        "Float.to_i" | "Float.truncate" => float_to_i,
        "Float.floor" => float_floor,
        "Float.ceil" => float_ceil,
        "Float.round" => float_round,
        "Float.round_to" => float_round_to,
        "Float.sqrt" => |_, a| fmap(a, f64::sqrt),
        "Float.cbrt" => |_, a| fmap(a, f64::cbrt),
        "Float.sin" => |_, a| fmap(a, f64::sin),
        "Float.cos" => |_, a| fmap(a, f64::cos),
        "Float.tan" => |_, a| fmap(a, f64::tan),
        "Float.asin" => |_, a| fmap(a, f64::asin),
        "Float.acos" => |_, a| fmap(a, f64::acos),
        "Float.atan" => |_, a| fmap(a, f64::atan),
        "Float.exp" => |_, a| fmap(a, f64::exp),
        "Float.ln" => |_, a| fmap(a, f64::ln),
        "Float.log10" => |_, a| fmap(a, f64::log10),
        "Float.log2" => |_, a| fmap(a, f64::log2),
        "Float.atan2" => |_, a| Ok(Value::from_f64(float(a[0])?.atan2(float(a[1])?))),
        "Float.is_nan" => |_, a| Ok(Value::bool(float(a[0])?.is_nan())),
        "Float.is_infinite" => |_, a| Ok(Value::bool(float(a[0])?.is_infinite())),
        "Float.is_finite" => |_, a| Ok(Value::bool(float(a[0])?.is_finite())),
        "Float.format" => float_format,

        "Bool.to_s" => |vm, a| Ok(new_str(vm, if a[0].is_true() { "true" } else { "false" })),

        "String.to_s" => |_, a| Ok(a[0]),
        "String.<=>" => |_, a| Ok(Value::small_int(s(a[0])?.cmp(s(a[1])?) as i64)),
        "String.+" => |vm, a| {
            let r = format!("{}{}", s(a[0])?, s(a[1])?);
            Ok(new_str(vm, &r))
        },
        "String.*" => |vm, a| {
            let n = int(a[1])?;
            if n < 0 {
                return panic("ArgumentError: negative repeat count");
            }
            let r = s(a[0])?.repeat(n as usize);
            Ok(new_str(vm, &r))
        },
        "String.[]" => str_index,
        "String.length" => |_, a| Ok(Value::small_int(unsafe { as_str_obj(a[0]) }.char_count() as i64)),
        "String.byte_length" => |_, a| Ok(Value::small_int(s(a[0])?.len() as i64)),
        "String.upcase" => |vm, a| str_map(vm, a, |x| x.to_uppercase()),
        "String.downcase" => |vm, a| str_map(vm, a, |x| x.to_lowercase()),
        "String.capitalize" => |vm, a| {
            str_map(vm, a, |x| {
                let mut c = x.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                    None => String::new(),
                }
            })
        },
        "String.swapcase" => |vm, a| {
            str_map(vm, a, |x| {
                x.chars()
                    .map(|c| if c.is_uppercase() { c.to_lowercase().collect::<String>() } else { c.to_uppercase().collect() })
                    .collect()
            })
        },
        "String.reverse" => |vm, a| str_map(vm, a, |x| x.chars().rev().collect()),
        "String.strip" => |vm, a| str_map(vm, a, |x| x.trim().to_string()),
        "String.lstrip" => |vm, a| str_map(vm, a, |x| x.trim_start().to_string()),
        "String.rstrip" => |vm, a| str_map(vm, a, |x| x.trim_end().to_string()),
        "String.chomp" => |vm, a| {
            str_map(vm, a, |x| x.strip_suffix("\r\n").or_else(|| x.strip_suffix('\n')).unwrap_or(x).to_string())
        },
        "String.split" => str_split,
        "String.lines" => |vm, a| {
            let parts: Vec<String> = s(a[0])?.lines().map(|l| l.to_string()).collect();
            Ok(str_list(vm, parts))
        },
        "String.chars" => |vm, a| {
            let parts: Vec<String> = s(a[0])?.chars().map(|c| c.to_string()).collect();
            Ok(str_list(vm, parts))
        },
        "String.bytes" => |vm, a| {
            let items = s(a[0])?.bytes().map(|b| Value::small_int(b as i64)).collect();
            Ok(new_list(vm, items))
        },
        "String.starts_with" => |_, a| Ok(Value::bool(s(a[0])?.starts_with(s(a[1])?))),
        "String.ends_with" => |_, a| Ok(Value::bool(s(a[0])?.ends_with(s(a[1])?))),
        "String.contains" => |_, a| Ok(Value::bool(s(a[0])?.contains(s(a[1])?))),
        "String.index_of" => str_index_of,
        "String.rindex_of" => |_, a| {
            let (h, n) = (s(a[0])?, s(a[1])?);
            Ok(match h.rfind(n) {
                Some(b) => Value::small_int(h[..b].chars().count() as i64),
                None => Value::NIL,
            })
        },
        "String.replace" => |vm, a| {
            let (h, o, n) = (s(a[0])?, s(a[1])?, s(a[2])?);
            if o.is_empty() {
                return Ok(a[0]);
            }
            let r = h.replace(o, n);
            Ok(new_str(vm, &r))
        },
        "String.replace_first" => |vm, a| {
            let (h, o, n) = (s(a[0])?, s(a[1])?, s(a[2])?);
            let r = h.replacen(o, n, 1);
            Ok(new_str(vm, &r))
        },
        "String.slice" => |vm, a| {
            let x = s(a[0])?;
            let (start, len) = (int(a[1])?, int(a[2])?);
            let r = char_slice(x, start, start.saturating_add(len.max(0)));
            Ok(new_str(vm, &r))
        },
        "String.substring" => |vm, a| {
            let x = s(a[0])?;
            let r = char_slice(x, int(a[1])?, int(a[2])?);
            Ok(new_str(vm, &r))
        },
        "String.count" => |_, a| {
            let (h, n) = (s(a[0])?, s(a[1])?);
            Ok(Value::small_int(if n.is_empty() { 0 } else { h.matches(n).count() as i64 }))
        },
        "String.to_i" => |vm, a| {
            let t = s(a[0])?.trim().replace('_', "");
            Ok(match t.parse::<BigInt>() {
                Ok(b) => vm.m.int_from_big(b),
                Err(_) => Value::NIL,
            })
        },
        "String.to_f" => |_, a| {
            let t = s(a[0])?.trim();
            Ok(match t.parse::<f64>() {
                Ok(f) if !t.is_empty() => Value::from_f64(f),
                _ => Value::NIL,
            })
        },
        "String.parse_int" => |vm, a| {
            let radix = arg(a, 1).map(int).transpose()?.unwrap_or(10);
            if !(2..=36).contains(&radix) {
                return panic("ArgumentError: radix must be between 2 and 36");
            }
            let t = s(a[0])?.trim();
            Ok(match BigInt::parse_bytes(t.as_bytes(), radix as u32) {
                Some(b) => vm.m.int_from_big(b),
                None => Value::NIL,
            })
        },
        "String.ord" => |_, a| match s(a[0])?.chars().next() {
            Some(c) => Ok(Value::small_int(c as i64)),
            None => panic("ArgumentError: empty string"),
        },
        "String.center" => |vm, a| pad(vm, a, 0),
        "String.ljust" => |vm, a| pad(vm, a, 1),
        "String.rjust" => |vm, a| pad(vm, a, 2),
        "String.is_digit" => |_, a| str_all(a, |c| c.is_ascii_digit()),
        "String.is_alpha" => |_, a| str_all(a, |c| c.is_alphabetic()),
        "String.is_alnum" => |_, a| str_all(a, |c| c.is_alphanumeric()),
        "String.is_space" => |_, a| str_all(a, |c| c.is_whitespace()),
        "String.is_upper" => |_, a| str_all(a, |c| !c.is_alphabetic() || c.is_uppercase()),
        "String.is_lower" => |_, a| str_all(a, |c| !c.is_alphabetic() || c.is_lowercase()),
        "String.delete_prefix" => |vm, a| {
            let (h, p) = (s(a[0])?, s(a[1])?);
            let r = h.strip_prefix(p).unwrap_or(h).to_string();
            Ok(new_str(vm, &r))
        },
        "String.delete_suffix" => |vm, a| {
            let (h, p) = (s(a[0])?, s(a[1])?);
            let r = h.strip_suffix(p).unwrap_or(h).to_string();
            Ok(new_str(vm, &r))
        },
        "String.squeeze" => |vm, a| {
            str_map(vm, a, |x| {
                let mut out = String::new();
                let mut last = None;
                for c in x.chars() {
                    if Some(c) != last {
                        out.push(c);
                    }
                    last = Some(c);
                }
                out
            })
        },
        "String.tr" => |vm, a| {
            let from: Vec<char> = s(a[1])?.chars().collect();
            let to: Vec<char> = s(a[2])?.chars().collect();
            let r: String = s(a[0])?
                .chars()
                .map(|c| match from.iter().position(|&f| f == c) {
                    Some(i) => *to.get(i).or(to.last()).unwrap_or(&c),
                    None => c,
                })
                .collect();
            Ok(new_str(vm, &r))
        },
        _ => return None,
    })
}

fn fmap(a: &[Value], f: fn(f64) -> f64) -> RtResult<Value> {
    Ok(Value::from_f64(f(float(a[0])?)))
}

fn str_map(vm: &mut Vm, a: &[Value], f: impl Fn(&str) -> String) -> RtResult<Value> {
    let r = f(s(a[0])?);
    Ok(new_str(vm, &r))
}

fn str_all(a: &[Value], f: impl Fn(char) -> bool) -> RtResult<Value> {
    let x = s(a[0])?;
    Ok(Value::bool(!x.is_empty() && x.chars().all(f)))
}

fn str_list(vm: &mut Vm, parts: Vec<String>) -> Value {
    let items: Vec<Value> = parts.iter().map(|p| vm.m.alloc_str(p)).collect();
    new_list(vm, items)
}

/// Substring by character positions [start, end), clamped; negative positions count from the end.
fn char_slice(x: &str, start: i64, end: i64) -> String {
    let n = if x.is_ascii() { x.len() as i64 } else { x.chars().count() as i64 };
    let norm = |i: i64| if i < 0 { (i + n).max(0) } else { i.min(n) };
    let (s0, e0) = (norm(start), norm(end));
    if e0 <= s0 {
        return String::new();
    }
    if x.is_ascii() {
        return x[s0 as usize..e0 as usize].to_string();
    }
    x.chars().skip(s0 as usize).take((e0 - s0) as usize).collect()
}

fn pad(vm: &mut Vm, a: &[Value], mode: u8) -> RtResult<Value> {
    let x = s(a[0])?;
    let width = int(a[1])?.max(0) as usize;
    let p = match arg(a, 2) {
        Some(v) => s(v)?,
        None => " ",
    };
    let len = x.chars().count();
    if len >= width || p.is_empty() {
        return Ok(a[0]);
    }
    let total = width - len;
    let fill = |n: usize| p.chars().cycle().take(n).collect::<String>();
    let r = match mode {
        0 => format!("{}{}{}", fill(total / 2), x, fill(total - total / 2)),
        1 => format!("{}{}", x, fill(total)),
        _ => format!("{}{}", fill(total), x),
    };
    Ok(new_str(vm, &r))
}

fn str_index(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let x = s(a[0])?;
    let i = int(a[1])?;
    let n = unsafe { as_str_obj(a[0]) }.char_count() as i64;
    let idx = if i < 0 { i + n } else { i };
    if idx < 0 || idx >= n {
        return panic(format!("IndexError: index {i} out of range for string of length {n}"));
    }
    let c = if x.is_ascii() { x[idx as usize..idx as usize + 1].to_string() } else { x.chars().nth(idx as usize).unwrap().to_string() };
    Ok(new_str(vm, &c))
}

fn str_index_of(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let (h, n) = (s(a[0])?, s(a[1])?);
    let from = arg(a, 2).map(int).transpose()?.unwrap_or(0).max(0) as usize;
    let start_byte = h.char_indices().nth(from).map(|(b, _)| b).unwrap_or(h.len());
    Ok(match h[start_byte..].find(n) {
        Some(b) => Value::small_int(h[..start_byte + b].chars().count() as i64),
        None => Value::NIL,
    })
}

fn str_split(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let x = s(a[0])?;
    let sep = match arg(a, 1) {
        Some(v) => s(v)?,
        None => "",
    };
    let parts: Vec<String> = if sep.is_empty() {
        x.split_whitespace().map(|p| p.to_string()).collect()
    } else {
        x.split(sep).map(|p| p.to_string()).collect()
    };
    Ok(str_list(vm, parts))
}

fn hash(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let h = ops::hash_value(vm, a[0])?;
    Ok(Value::small_int((h >> 17) as i64))
}

fn int_to_s(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let r = ops::format_int(a[0]);
    Ok(new_str(vm, &r))
}

fn int_to_f(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    Ok(Value::from_f64(ops::int_to_f64(a[0])))
}

fn int_to_s_radix(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let radix = int(a[1])?;
    if !(2..=36).contains(&radix) {
        return panic("ArgumentError: radix must be between 2 and 36");
    }
    let r = ops::to_bigint(a[0]).to_str_radix(radix as u32);
    Ok(new_str(vm, &r))
}

fn int_chr(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let i = int(a[0])?;
    match u32::try_from(i).ok().and_then(char::from_u32) {
        Some(c) => Ok(new_str(vm, &c.to_string())),
        None => panic(format!("RangeError: {i} is not a valid character code")),
    }
}

fn int_sqrt(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let b = ops::to_bigint(a[0]);
    if b.is_negative() {
        return panic("Math::DomainError: square root of a negative number");
    }
    Ok(vm.m.int_from_big(b.sqrt()))
}

fn int_bit_length(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    Ok(Value::small_int(ops::to_bigint(a[0]).bits() as i64))
}

fn float_to_s(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let r = ops::format_float(float(a[0])?);
    Ok(new_str(vm, &r))
}

fn float_to_i(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    ops::float_to_int(&mut vm.m, float(a[0])?)
}
fn float_floor(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    ops::float_to_int(&mut vm.m, float(a[0])?.floor())
}
fn float_ceil(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    ops::float_to_int(&mut vm.m, float(a[0])?.ceil())
}
fn float_round(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    ops::float_to_int(&mut vm.m, float(a[0])?.round())
}
fn float_round_to(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let x = float(a[0])?;
    let d = int(a[1])?.clamp(-15, 15) as i32;
    let m = 10f64.powi(d);
    Ok(Value::from_f64((x * m).round() / m))
}
fn float_format(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let x = float(a[0])?;
    let d = int(a[1])?.clamp(0, 30) as usize;
    let r = format!("{x:.d$}");
    Ok(new_str(vm, &r))
}

// ---- global functions ------------------------------------------------------------------------

fn puts(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let mut text = match arg(a, 0) {
        Some(v) => to_s(vm, v)?,
        None => String::new(),
    };
    text.push('\n');
    write_out(&text);
    Ok(Value::NIL)
}

fn print(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let text = to_s(vm, a[0])?;
    write_out(&text);
    Ok(Value::NIL)
}

fn eputs(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let text = match arg(a, 0) {
        Some(v) => to_s(vm, v)?,
        None => String::new(),
    };
    flush();
    eprintln!("{text}");
    Ok(Value::NIL)
}

fn panic_fn(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    Err(Trap::Panic(s(a[0])?.to_string()))
}

fn assert_fn(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    if !a[0].is_true() {
        let msg = match arg(a, 1) {
            Some(v) => s(v)?.to_string(),
            None => "assertion failed".into(),
        };
        return Err(Trap::Panic(msg));
    }
    Ok(Value::NIL)
}

fn assert_eq(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    if !ops::values_eq(vm, a[0], a[1])? {
        let got = ops::inspect(vm, a[0])?;
        let exp = ops::inspect(vm, a[1])?;
        let extra = match arg(a, 2) {
            Some(v) if !s(v)?.is_empty() => format!(": {}", s(v)?),
            _ => String::new(),
        };
        return Err(Trap::Panic(format!("assertion failed{extra}: expected {exp}, got {got}")));
    }
    Ok(Value::NIL)
}

fn inspect(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let r = ops::inspect(vm, a[0])?;
    Ok(new_str(vm, &r))
}

fn compare_values(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let o = ops::compare_or_panic(vm, a[0], a[1])?;
    Ok(Value::small_int(o as i64))
}

fn read_line(vm: &mut Vm, _: &[Value]) -> RtResult<Value> {
    let line = crate::fiber::blocking_io(vm, || {
        let mut s = String::new();
        match std::io::stdin().read_line(&mut s) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(s),
        }
    });
    Ok(match line {
        Some(mut l) => {
            if l.ends_with('\n') {
                l.pop();
                if l.ends_with('\r') {
                    l.pop();
                }
            }
            new_str(vm, &l)
        }
        None => Value::NIL,
    })
}

fn args_fn(vm: &mut Vm, _: &[Value]) -> RtResult<Value> {
    let args = ARGS.get().cloned().unwrap_or_default();
    Ok(str_list(vm, args))
}

fn exit_fn(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let code = arg(a, 0).map(int).transpose()?.unwrap_or(0);
    Err(exit_trap(code as i32))
}

fn sleep_fn(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let secs = float(a[0])?.max(0.0);
    crate::fiber::sleep(vm, std::time::Duration::from_secs_f64(secs));
    Ok(Value::NIL)
}

fn gc_fn(vm: &mut Vm, _: &[Value]) -> RtResult<Value> {
    catallaxy_gc::collect_now(&vm.m.rec);
    Ok(Value::NIL)
}

#[allow(dead_code)]
fn unused(b: &BigInt) -> bool {
    b.is_zero() && b.to_i64().is_some() && false
}

#[allow(dead_code)]
fn ix(vm: &mut Vm) -> &mut dyn Interop {
    vm
}
