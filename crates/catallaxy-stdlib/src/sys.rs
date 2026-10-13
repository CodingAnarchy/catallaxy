//! Files, time, randomness, environment, JSON, math, and dynamic member access.

use std::cell::Cell;
use std::sync::LazyLock;
use std::time::Instant;

use catallaxy_runtime::class::class_of;
use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops;
use catallaxy_runtime::{intern, panic, ObjKind, RtResult, Value};
use catallaxy_vm::slow;
use catallaxy_vm::{NativeFn, Vm};

use crate::coll::list_read;
use crate::*;

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "Math.sqrt" => |_, a| Ok(Value::from_f64(float(a[0])?.sqrt())),
        "Math.pow" => |_, a| Ok(Value::from_f64(float(a[0])?.powf(float(a[1])?))),
        "Math.sin" => |_, a| Ok(Value::from_f64(float(a[0])?.sin())),
        "Math.cos" => |_, a| Ok(Value::from_f64(float(a[0])?.cos())),
        "Math.tan" => |_, a| Ok(Value::from_f64(float(a[0])?.tan())),
        "Math.atan2" => |_, a| Ok(Value::from_f64(float(a[0])?.atan2(float(a[1])?))),
        "Math.exp" => |_, a| Ok(Value::from_f64(float(a[0])?.exp())),
        "Math.log" => |_, a| Ok(Value::from_f64(float(a[0])?.ln())),
        "Math.log10" => |_, a| Ok(Value::from_f64(float(a[0])?.log10())),
        "Math.hypot" => |_, a| Ok(Value::from_f64(float(a[0])?.hypot(float(a[1])?))),

        "File.read" => |vm, a| {
            let p = s(a[0])?.to_string();
            let r = crate::fiber::blocking_io(vm, || std::fs::read_to_string(&p));
            Ok(match r {
                Ok(text) => {
                    let v = new_str(vm, &text);
                    ok(vm, v)
                }
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },
        "File.read_lines" => |vm, a| {
            let p = s(a[0])?.to_string();
            let r = crate::fiber::blocking_io(vm, || std::fs::read_to_string(&p));
            Ok(match r {
                Ok(text) => {
                    let items: Vec<Value> = text.lines().map(|l| vm.m.alloc_str(l)).collect();
                    let l = new_list(vm, items);
                    ok(vm, l)
                }
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },
        "File.write" => |vm, a| {
            let p = s(a[0])?.to_string();
            let c = s(a[1])?.to_string();
            let r = crate::fiber::blocking_io(vm, || std::fs::write(&p, c));
            Ok(match r {
                Ok(()) => ok(vm, Value::NIL),
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },
        "File.append" => |vm, a| {
            let p = s(a[0])?.to_string();
            let c = s(a[1])?.to_string();
            let r = crate::fiber::blocking_io(vm, || {
                use std::io::Write as _;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .and_then(|mut f| f.write_all(c.as_bytes()))
            });
            Ok(match r {
                Ok(()) => ok(vm, Value::NIL),
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },
        "File.delete" => |vm, a| {
            let p = s(a[0])?.to_string();
            let r = crate::fiber::blocking_io(vm, || std::fs::remove_file(&p));
            Ok(match r {
                Ok(()) => ok(vm, Value::NIL),
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },
        "File.list_dir" => |vm, a| {
            let p = s(a[0])?.to_string();
            let r = crate::fiber::blocking_io(vm, || -> std::io::Result<Vec<String>> {
                let mut names: Vec<String> = std::fs::read_dir(&p)?
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();
                names.sort();
                Ok(names)
            });
            Ok(match r {
                Ok(names) => {
                    let items: Vec<Value> = names.iter().map(|n| vm.m.alloc_str(n)).collect();
                    let l = new_list(vm, items);
                    ok(vm, l)
                }
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },

        "Time.now" => |_, _| {
            let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            Ok(Value::from_f64(d.as_secs_f64()))
        },
        "Time.monotonic" => |_, _| Ok(Value::from_f64(START.elapsed().as_secs_f64())),
        "Time.millis" => |_, _| Ok(Value::small_int(START.elapsed().as_millis() as i64)),

        "Random.seed" => |_, a| {
            let n = int(a[0])?;
            RNG.with(|r| r.set(splitmix(n as u64)));
            Ok(Value::NIL)
        },
        "Random.int" => |_, a| {
            let (lo, hi) = (int(a[0])?, int(a[1])?);
            if hi < lo {
                return panic("ArgumentError: Random.int(lo, hi) needs lo <= hi");
            }
            let span = (hi - lo) as u64 + 1;
            Ok(Value::small_int(lo + (next_u64() % span) as i64))
        },
        "Random.float" => |_, _| Ok(Value::from_f64((next_u64() >> 11) as f64 / (1u64 << 53) as f64)),
        "Random.shuffle" => |vm, a| {
            let mut items = list_read(vm, a[0])?.to_vec();
            for i in (1..items.len()).rev() {
                let j = (next_u64() % (i as u64 + 1)) as usize;
                items.swap(i, j);
            }
            Ok(new_list(vm, items))
        },
        "Random.choice" => |vm, a| {
            let items = list_read(vm, a[0])?.to_vec();
            if items.is_empty() {
                return Ok(Value::NIL);
            }
            Ok(items[(next_u64() % items.len() as u64) as usize])
        },

        "Env.get" => |vm, a| {
            Ok(match std::env::var(s(a[0])?) {
                Ok(v) => new_str(vm, &v),
                Err(_) => Value::NIL,
            })
        },
        "Env.cpu_count" => {
            |_, _| Ok(Value::small_int(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as i64))
        }

        "Json.parse" => json_parse,
        "Json.stringify" => |vm, a| {
            let v = to_json(vm, a[0], 0)?;
            let mut text = String::new();
            v.write(&mut text, None);
            Ok(new_str(vm, &text))
        },
        "Json.pretty" => |vm, a| {
            let v = to_json(vm, a[0], 0)?;
            let mut text = String::new();
            v.write(&mut text, Some(0));
            Ok(new_str(vm, &text))
        },

        "__dyn_member" => dyn_member,
        _ => return None,
    })
}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

thread_local! {
    static RNG: Cell<u64> = Cell::new({
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        splitmix(t ^ (std::thread::current().id().as_u64_compat() << 32))
    });
}

trait ThreadIdCompat {
    fn as_u64_compat(&self) -> u64;
}
impl ThreadIdCompat for std::thread::ThreadId {
    fn as_u64_compat(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn next_u64() -> u64 {
    RNG.with(|r| {
        // xorshift64*
        let mut x = r.get().max(1);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        r.set(x);
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    })
}

fn json_parse(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let text = s(a[0])?;
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(j) => {
            let v = from_json(vm, &j)?;
            vm.extra_roots().push(v);
            let r = ok(vm, v);
            vm.extra_roots().pop();
            Ok(r)
        }
        Err(e) => Ok(err(vm, &format!("invalid JSON: {e}"))),
    }
}

fn from_json(vm: &mut Vm, j: &serde_json::Value) -> RtResult<Value> {
    use serde_json::Value as J;
    Ok(match j {
        J::Null => Value::NIL,
        J::Bool(b) => Value::bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                vm.m.int(i)
            } else if let Some(u) = n.as_u64() {
                vm.m.int_from_big(num_bigint::BigInt::from(u))
            } else {
                Value::from_f64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        J::String(s) => new_str(vm, s),
        J::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for x in items {
                out.push(from_json(vm, x)?);
            }
            // No user code runs while building, so the elements need no rooting.
            new_list(vm, out)
        }
        J::Object(map) => {
            let d = vm.m.alloc_dict(DictTable::record(map.len()));
            for (k, x) in map {
                let kv = new_str(vm, k);
                let xv = from_json(vm, x)?;
                slow::dict_set(vm, d, kv, xv)?;
            }
            d
        }
    })
}

/// A JSON tree ready to print. Scalars are pre-rendered, so integers of any
/// size are written as JSON numbers (serde_json's `Value` cannot hold a number
/// wider than 64 bits without its `arbitrary_precision` feature, which would
/// also change how the language server's untagged LSP types deserialize).
/// Object keys are sorted, as serde_json's default map does.
enum Jv {
    Scalar(String),
    Arr(Vec<Jv>),
    Obj(std::collections::BTreeMap<String, Jv>),
}

impl Jv {
    fn string(s: &str) -> Jv {
        Jv::Scalar(serde_json::to_string(s).unwrap_or_default())
    }

    /// Writes compact JSON when `indent` is `None`, else pretty JSON in
    /// serde_json's style (two-space indent, `"key": value`).
    fn write(&self, out: &mut String, indent: Option<usize>) {
        fn newline(out: &mut String, level: usize) {
            out.push('\n');
            for _ in 0..level {
                out.push_str("  ");
            }
        }
        let (open, close, len) = match self {
            Jv::Scalar(t) => {
                out.push_str(t);
                return;
            }
            Jv::Arr(items) => ('[', ']', items.len()),
            Jv::Obj(map) => ('{', '}', map.len()),
        };
        out.push(open);
        if len == 0 {
            out.push(close);
            return;
        }
        let inner = indent.map(|l| l + 1);
        let item = |out: &mut String, i: usize| {
            if i > 0 {
                out.push(',');
            }
            if let Some(l) = inner {
                newline(out, l);
            }
        };
        match self {
            Jv::Arr(items) => {
                for (i, x) in items.iter().enumerate() {
                    item(out, i);
                    x.write(out, inner);
                }
            }
            Jv::Obj(map) => {
                for (i, (k, x)) in map.iter().enumerate() {
                    item(out, i);
                    out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    out.push_str(if indent.is_some() { ": " } else { ":" });
                    x.write(out, inner);
                }
            }
            Jv::Scalar(_) => unreachable!(),
        }
        if let Some(l) = indent {
            newline(out, l);
        }
        out.push(close);
    }
}

fn to_json(vm: &mut Vm, v: Value, depth: usize) -> RtResult<Jv> {
    if depth > 200 {
        return panic("JSON nesting too deep");
    }
    if v.is_nil() {
        return Ok(Jv::Scalar("null".into()));
    }
    if v.is_bool() {
        return Ok(Jv::Scalar(if v.is_true() { "true" } else { "false" }.into()));
    }
    if v.is_small_int() {
        return Ok(Jv::Scalar(v.as_small_int().to_string()));
    }
    if v.is_float() {
        let text = serde_json::Number::from_f64(v.as_f64()).map_or_else(|| "null".into(), |n| n.to_string());
        return Ok(Jv::Scalar(text));
    }
    match kind_of(v) {
        ObjKind::BigInt => Ok(Jv::Scalar(ops::format_int(v))),
        ObjKind::Str => Ok(Jv::string(s(v)?)),
        ObjKind::List | ObjKind::Tuple => {
            let items: Vec<Value> = if kind_of(v) == ObjKind::Tuple {
                unsafe { as_tuple(v) }.elems().to_vec()
            } else {
                list_read(vm, v)?.to_vec()
            };
            let mut out = Vec::with_capacity(items.len());
            for x in items {
                out.push(to_json(vm, x, depth + 1)?);
            }
            Ok(Jv::Arr(out))
        }
        ObjKind::Dict => {
            let pairs: Vec<(Value, Value)> =
                catallaxy_runtime::ownership::read_dict(&vm.m, v, |t| t.iter().map(|e| (e.key, e.value)).collect())?;
            let mut map = std::collections::BTreeMap::new();
            for (k, x) in pairs {
                let key = to_s(vm, k)?;
                map.insert(key, to_json(vm, x, depth + 1)?);
            }
            Ok(Jv::Obj(map))
        }
        _ => {
            // Objects: their fields as an object, or to_s.
            let cls = class_of(v);
            if cls.kind == ObjKind::Instance && cls.lookup(intern("to_json")).is_none() && cls.variant_tag.is_none() {
                let o = unsafe { as_instance(v) };
                let mut map = std::collections::BTreeMap::new();
                for i in 0..cls.nfields() {
                    let fv = o.get(i);
                    map.insert(cls.field_names[i].to_string(), to_json(vm, fv, depth + 1)?);
                }
                return Ok(Jv::Obj(map));
            }
            Ok(Jv::string(&to_s(vm, v)?))
        }
    }
}

/// `d.name` on a `Dyn` value: a dict key, a field, or a zero-argument method.
fn dyn_member(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let (recv, name) = (a[0], a[1]);
    if kind_of(recv) == ObjKind::Dict {
        return Ok(slow::dict_get(vm, recv, name)?.unwrap_or(Value::NIL));
    }
    slow::dyn_member(vm, recv, s(name)?)
}
