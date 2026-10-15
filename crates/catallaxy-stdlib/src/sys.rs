//! Files, time, randomness, environment, JSON, math, and dynamic member access.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Instant;

use catallaxy_runtime::class::class_of;
use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops;
use catallaxy_runtime::{intern, panic, try_push, try_vec, ObjKind, RtResult, Value};
use catallaxy_vm::slow;
use catallaxy_vm::{NativeFn, Vm};
use parking_lot::RwLock;

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
            match r {
                Ok(text) => {
                    let v = vm.m.try_alloc_str(&text)?;
                    Ok(ok(vm, v))
                }
                Err(e) => io_err(vm, &p, e),
            }
        },
        "File.read_lines" => |vm, a| {
            let p = s(a[0])?.to_string();
            let r = crate::fiber::blocking_io(vm, || std::fs::read_to_string(&p));
            match r {
                Ok(text) => {
                    vm.m.collect_if_refused();
                    let l = new_str_list(vm, text.lines())?;
                    Ok(ok(vm, l))
                }
                Err(e) => io_err(vm, &p, e),
            }
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
                    let l = new_str_list(vm, names.iter().map(|n| n.as_str()))?;
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
        "Time.millis" => |vm, _| Ok(vm.m.int(wall_nanos().div_euclid(1_000_000))),
        "__time_wall_nanos" => |vm, _| Ok(vm.m.int(wall_nanos())),
        "__time_mono_nanos" => |vm, _| Ok(vm.m.int(START.elapsed().as_nanos() as i64)),
        "__time_local_offset" => |_, a| Ok(Value::small_int(local_offset(int(a[0])?))),

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
            let mut items = list_read(vm, a[0])?.try_to_vec()?;
            for i in (1..items.len()).rev() {
                let j = (next_u64() % (i as u64 + 1)) as usize;
                items.swap(i, j);
            }
            new_list(vm, items)
        },
        "Random.choice" => |vm, a| {
            let l = list_read(vm, a[0])?;
            let n = l.len();
            if n == 0 {
                return Ok(Value::NIL);
            }
            Ok(l.get((next_u64() % n as u64) as usize).unwrap_or(Value::NIL))
        },

        "Env.get" => |vm, a| {
            Ok(match env_get(s(a[0])?) {
                Some(v) => new_str(vm, &v),
                None => Value::NIL,
            })
        },
        "Env.set" => |_, a| {
            let (name, value) = (s(a[0])?, s(a[1])?);
            check_env_name(name)?;
            if value.contains('\0') {
                return panic("ArgumentError: environment values cannot contain NUL");
            }
            ENV_OVERLAY.write().insert(name.to_string(), Some(value.to_string()));
            Ok(Value::NIL)
        },
        "Env.remove" => |_, a| {
            let name = s(a[0])?;
            check_env_name(name)?;
            ENV_OVERLAY.write().insert(name.to_string(), None);
            Ok(Value::NIL)
        },
        "Env.all" => |vm, _| {
            let mut all: BTreeMap<String, String> = std::env::vars_os()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
                .collect();
            for (k, v) in ENV_OVERLAY.read().iter() {
                match v {
                    Some(v) => all.insert(k.clone(), v.clone()),
                    None => all.remove(k),
                };
            }
            // Nothing roots the new dict: its inserts must not collect.
            let d = vm.m.alloc_dict(DictTable::new());
            for (k, v) in all {
                let (kv, vv) = (new_str(vm, &k), new_str(vm, &v));
                slow::dict_set_unrooted(vm, d, kv, vv)?;
            }
            Ok(d)
        },
        "Env.cpu_count" => {
            |_, _| Ok(Value::small_int(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as i64))
        }

        "Json.parse" => json_parse,
        "Json.stringify" => |vm, a| {
            let v = to_json(vm, a[0], 0)?;
            let mut text = String::new();
            v.write(&mut text, None);
            vm.m.try_alloc_str(&text)
        },
        "Json.pretty" => |vm, a| {
            let v = to_json(vm, a[0], 0)?;
            let mut text = String::new();
            v.write(&mut text, Some(0));
            vm.m.try_alloc_str(&text)
        },

        "__dyn_member" => dyn_member,
        _ => return None,
    })
}

/// Variables set or removed by `Env.set` / `Env.remove` (`None`: removed). The process
/// environment itself is never modified: `std::env::set_var` is unsound while other threads may
/// read the environment (program threads, the I/O pool, the network reactor, libc's resolver),
/// and a Catallaxy program cannot rule that out. So the program's environment is this overlay on
/// top of the inherited one: `Env.get` and `Env.all` read through it and `Process` applies it to
/// every child. Native code that calls `getenv` itself does not see it.
static ENV_OVERLAY: LazyLock<RwLock<BTreeMap<String, Option<String>>>> = LazyLock::new(Default::default);

/// A variable as the program sees it (see [`ENV_OVERLAY`]).
pub(crate) fn env_get(name: &str) -> Option<String> {
    if let Some(v) = ENV_OVERLAY.read().get(name) {
        return v.clone();
    }
    std::env::var(name).ok()
}

/// The program's changes to the inherited environment, to apply to a child process.
pub(crate) fn env_overlay() -> Vec<(String, Option<String>)> {
    ENV_OVERLAY.read().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn check_env_name(name: &str) -> RtResult<()> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return panic(format!("ArgumentError: invalid environment variable name {name:?}"));
    }
    Ok(())
}

/// The origin of `Time.monotonic` and `Instant`: the first use of the monotonic clock, which
/// [`init_clock`] makes the start of the program.
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Pins the monotonic clock's origin to now (called while the program loads).
pub(crate) fn init_clock() {
    LazyLock::force(&START);
}

/// Nanoseconds since the Unix epoch on the wall clock (negative before 1970).
fn wall_nanos() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
#[cfg(any(unix, windows))]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The OS's local UTC offset in seconds east at `unix` seconds since the epoch (honoring `TZ`
/// and daylight saving time), or 0 if the OS cannot say. Computed as the difference between the
/// local civil time and `unix`, so it needs no `tm_gmtoff` (absent on Windows).
#[cfg(any(unix, windows))]
fn local_offset(unix: i64) -> i64 {
    let t = unix as libc::time_t;
    // SAFETY: `tm` is plain data that the libc calls only write into. `tzset` (which reads `TZ`)
    // runs once; the runtime never modifies the environment.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(unix)]
        let ok = {
            extern "C" {
                fn tzset();
            }
            static TZSET: std::sync::Once = std::sync::Once::new();
            TZSET.call_once(|| tzset());
            !libc::localtime_r(&t, &mut tm).is_null()
        };
        #[cfg(windows)]
        let ok = libc::localtime_s(&mut tm, &t) == 0;
        if !ok {
            return 0;
        }
        tm
    };
    let days = days_from_civil(tm.tm_year as i64 + 1900, tm.tm_mon as i64 + 1, tm.tm_mday as i64);
    days * 86400 + tm.tm_hour as i64 * 3600 + tm.tm_min as i64 * 60 + tm.tm_sec as i64 - unix
}

#[cfg(not(any(unix, windows)))]
fn local_offset(_unix: i64) -> i64 {
    0
}

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

/// Native stack that parsing or printing JSON may use beyond the stack guard's margin. Both
/// recurse once per nesting level, to at most 128 (serde_json's limit) or 200 (`to_json`)
/// levels: about 80 KB in all on aarch64 macOS, part of which the margin covers.
const JSON_STACK: usize = 64 << 10;

fn json_parse(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let text = s(a[0])?;
    catallaxy_runtime::stack::check_extra(JSON_STACK)?;
    // serde_json checks the text and words the errors; `Pj` then reads the checked text
    // again so that integers keep every digit.
    if let Err(e) = serde_json::from_str::<JsonCheck>(text) {
        return Ok(err(vm, &format!("invalid JSON: {e}")));
    }
    vm.m.collect_if_refused();
    let j = Pj::parse(text)?;
    let v = from_json(vm, j)?;
    vm.extra_roots().push(v);
    let r = ok(vm, v);
    vm.extra_roots().pop();
    Ok(r)
}

/// Checks JSON text as parsing it into a `serde_json::Value` would (the same errors, including
/// numbers out of range), without building anything.
struct JsonCheck;

impl<'de> serde::Deserialize<'de> for JsonCheck {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(JsonCheck)
    }
}

impl<'de> serde::de::Visitor<'de> for JsonCheck {
    type Value = JsonCheck;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any valid JSON value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<JsonCheck, E> {
        Ok(JsonCheck)
    }
    fn visit_i64<E>(self, _: i64) -> Result<JsonCheck, E> {
        Ok(JsonCheck)
    }
    fn visit_u64<E>(self, _: u64) -> Result<JsonCheck, E> {
        Ok(JsonCheck)
    }
    fn visit_f64<E>(self, _: f64) -> Result<JsonCheck, E> {
        Ok(JsonCheck)
    }
    fn visit_str<E>(self, _: &str) -> Result<JsonCheck, E> {
        Ok(JsonCheck)
    }
    fn visit_unit<E>(self) -> Result<JsonCheck, E> {
        Ok(JsonCheck)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<JsonCheck, A::Error> {
        while seq.next_element::<JsonCheck>()?.is_some() {}
        Ok(JsonCheck)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<JsonCheck, A::Error> {
        while map.next_key::<JsonCheck>()?.is_some() {
            map.next_value::<JsonCheck>()?;
        }
        Ok(JsonCheck)
    }
}

/// A parsed JSON value whose numbers are kept as their source text. serde_json's `Value`
/// turns an integer wider than 64 bits into a float unless its `arbitrary_precision`
/// feature is on, and that feature would change how the language server's untagged LSP
/// types deserialize (see `Jv`).
enum Pj<'a> {
    Null,
    Bool(bool),
    Num(&'a str),
    Str(String),
    Arr(Vec<Pj<'a>>),
    /// Sorted, keeping the last of any duplicate keys, as serde_json's default map does.
    Obj(BTreeMap<String, Pj<'a>>),
}

impl<'a> Pj<'a> {
    /// Parses text that serde_json has already accepted, so it does no error checking
    /// (and nests no deeper than serde_json's recursion limit). Arrays raise a `MemoryError`
    /// if they cannot grow.
    fn parse(text: &'a str) -> RtResult<Pj<'a>> {
        let mut pos = 0;
        Pj::value(text, &mut pos)
    }

    fn skip_ws(b: &[u8], pos: &mut usize) {
        while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\n' | b'\r') {
            *pos += 1;
        }
    }

    fn value(text: &'a str, pos: &mut usize) -> RtResult<Pj<'a>> {
        let b = text.as_bytes();
        Pj::skip_ws(b, pos);
        Ok(match b[*pos] {
            b'n' => {
                *pos += 4;
                Pj::Null
            }
            b't' => {
                *pos += 4;
                Pj::Bool(true)
            }
            b'f' => {
                *pos += 5;
                Pj::Bool(false)
            }
            b'"' => Pj::Str(Pj::string(text, pos)),
            b'[' => {
                *pos += 1;
                let mut items = Vec::new();
                loop {
                    Pj::skip_ws(b, pos);
                    match b[*pos] {
                        b']' => {
                            *pos += 1;
                            return Ok(Pj::Arr(items));
                        }
                        b',' => *pos += 1,
                        _ => try_push(&mut items, Pj::value(text, pos)?)?,
                    }
                }
            }
            b'{' => {
                *pos += 1;
                let mut map = BTreeMap::new();
                loop {
                    Pj::skip_ws(b, pos);
                    match b[*pos] {
                        b'}' => {
                            *pos += 1;
                            return Ok(Pj::Obj(map));
                        }
                        b',' => *pos += 1,
                        _ => {
                            let k = Pj::string(text, pos);
                            Pj::skip_ws(b, pos);
                            *pos += 1; // the `:`
                            map.insert(k, Pj::value(text, pos)?);
                        }
                    }
                }
            }
            _ => {
                let start = *pos;
                while *pos < b.len() && matches!(b[*pos], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    *pos += 1;
                }
                Pj::Num(&text[start..*pos])
            }
        })
    }

    /// Reads the string literal at `pos` (its opening quote) and decodes its escapes.
    fn string(text: &str, pos: &mut usize) -> String {
        let b = text.as_bytes();
        let start = *pos;
        *pos += 1;
        while b[*pos] != b'"' {
            *pos += if b[*pos] == b'\\' { 2 } else { 1 };
        }
        *pos += 1;
        serde_json::from_str(&text[start..*pos]).unwrap_or_default()
    }
}

/// Builds the value, consuming the tree as it goes. Nothing roots the values built so far, so
/// nothing here may collect: allocations that are refused raise a `MemoryError` at once.
fn from_json(vm: &mut Vm, j: Pj) -> RtResult<Value> {
    Ok(match j {
        Pj::Null => Value::NIL,
        Pj::Bool(b) => Value::bool(b),
        Pj::Num(t) => {
            // A literal without a fraction or exponent is an `Int`, however wide.
            if t.bytes().any(|c| matches!(c, b'.' | b'e' | b'E')) {
                Value::from_f64(t.parse().unwrap_or(f64::NAN))
            } else if let Ok(i) = t.parse::<i64>() {
                vm.m.int(i)
            } else {
                vm.m.int_from_big(t.parse().unwrap_or_default())
            }
        }
        Pj::Str(s) => vm.m.try_alloc_str_nogc(&s)?,
        Pj::Arr(items) => {
            let mut out = try_vec(items.len())?;
            for x in items {
                out.push(from_json(vm, x)?);
            }
            new_list(vm, out)?
        }
        Pj::Obj(map) => {
            let table = match DictTable::try_record(map.len()) {
                Ok(t) => t,
                Err(l) => return catallaxy_runtime::refused(format!("cannot allocate a table of {} bytes", l.size())),
            };
            let d = vm.m.alloc_dict(table);
            for (k, x) in map {
                let kv = vm.m.try_alloc_str_nogc(&k)?;
                let xv = from_json(vm, x)?;
                slow::dict_set_unrooted(vm, d, kv, xv)?;
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
    if depth == 0 {
        // Covers this recursion and the printing of what it builds.
        catallaxy_runtime::stack::check_extra(JSON_STACK)?;
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
                list_read(vm, v)?.try_to_vec()?
            };
            let mut out = try_vec(items.len())?;
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
            // Objects: what their `to_json` returns, their fields as an object, or to_s.
            let cls = class_of(v);
            if cls.kind == ObjKind::Instance && cls.lookup(intern("to_json")).is_some() {
                let r = vm.call_method_named(v, intern("to_json"), &[])?;
                vm.extra_roots().push(r);
                let j = to_json(vm, r, depth + 1);
                vm.extra_roots().pop();
                return j;
            }
            if cls.kind == ObjKind::Instance && cls.variant_tag.is_none() {
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
