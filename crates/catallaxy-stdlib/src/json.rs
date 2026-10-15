//! `Json.parse`, `Json.stringify` and `Json.pretty`.
//!
//! Parsing reads the text once, straight into heap values: every finished value is pushed on the
//! VM's extra roots (so allocating, which may collect, is safe), and a container takes its items
//! off them when it closes. Object keys are sorted, the last of duplicate keys winning, as
//! serde_json's default map did when it built the tree. Integers keep every digit (as big
//! integers when wider than 64 bits). The reader accepts exactly the texts serde_json accepts;
//! when it finds an error, serde_json reads the text again to word it, so messages are
//! serde_json's own.
//!
//! Printing writes into one string, without building a tree first.

use std::fmt::Write as _;

use catallaxy_runtime::class::class_of;
use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::ops;
use catallaxy_runtime::symbol::name;
use catallaxy_runtime::{intern, panic, try_vec, ObjKind, RtResult, Value};
use catallaxy_vm::slow;
use catallaxy_vm::Vm;

use crate::coll::list_read;
use crate::*;

/// Native stack that parsing or printing JSON may use beyond the stack guard's margin. Both
/// recurse once per nesting level, to at most 128 (serde_json's limit) or 200 (printing)
/// levels: about 80 KB in all on aarch64 macOS, part of which the margin covers.
const JSON_STACK: usize = 64 << 10;

/// The deepest nesting serde_json accepts: it fails on the 128th open container.
const MAX_DEPTH: usize = 127;

/// The most distinct object keys a parse remembers for sharing.
const MAX_KEYS: usize = 4096;

/// The deepest nesting `stringify` writes.
const MAX_WRITE_DEPTH: usize = 200;

/// `Json.parse(text)`: `Ok(value)`, or `Err("invalid JSON: ...")` with serde_json's message.
pub(crate) fn parse(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let text = s(a[0])?;
    catallaxy_runtime::stack::check_extra(JSON_STACK)?;
    vm.m.collect_if_refused();
    let base = vm.extra_roots().len();
    let mut r = Reader { text, b: text.as_bytes(), pos: 0, depth: 0, buf: String::new(), keys: Default::default() };
    let parsed = r.document(vm);
    let v = vm.extra_roots().get(base).copied();
    let out = match parsed {
        Ok(true) => Ok(ok(vm, v.unwrap_or(Value::NIL))),
        Ok(false) => {
            vm.extra_roots().truncate(base);
            let msg = match serde_json::from_str::<JsonCheck>(text) {
                Err(e) => format!("invalid JSON: {e}"),
                // The reader and serde_json disagree: a bug, kept visible.
                Ok(_) => format!("invalid JSON: rejected at byte {}", r.pos),
            };
            Ok(err(vm, &msg))
        }
        Err(e) => Err(e),
    };
    vm.extra_roots().truncate(base);
    out
}

/// A single-pass reader of JSON text into heap values. Each method returns `Ok(false)` at a
/// syntax error, leaving `pos` near it, and raises a `MemoryError` if an allocation is refused.
struct Reader<'a> {
    text: &'a str,
    b: &'a [u8],
    pos: usize,
    /// Open arrays and objects.
    depth: usize,
    /// The decoded text of the last string read.
    buf: String,
    /// Object keys read so far, so that records with the same keys share their key strings.
    /// Every key ends up in a dict the result (or the root stack) holds, so these stay alive.
    keys: rustc_hash::FxHashMap<Box<str>, Value>,
}

impl Reader<'_> {
    /// A whole document: one value, surrounded by nothing but whitespace.
    fn document(&mut self, vm: &mut Vm) -> RtResult<bool> {
        if !self.value(vm)? {
            return Ok(false);
        }
        self.ws();
        Ok(self.pos == self.b.len())
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn word(&mut self, w: &[u8]) -> bool {
        let ok = self.b[self.pos..].starts_with(w);
        if ok {
            self.pos += w.len();
        }
        ok
    }

    /// Reads a value and pushes it on the extra roots.
    fn value(&mut self, vm: &mut Vm) -> RtResult<bool> {
        self.ws();
        let v = match self.peek() {
            Some(b'n') if self.word(b"null") => Value::NIL,
            Some(b't') if self.word(b"true") => Value::bool(true),
            Some(b'f') if self.word(b"false") => Value::bool(false),
            Some(b'"') => {
                if !self.string() {
                    return Ok(false);
                }
                vm.m.try_alloc_str(&self.buf)?
            }
            Some(b'[') => return self.array(vm),
            Some(b'{') => return self.object(vm),
            Some(b'-' | b'0'..=b'9') => match self.number(vm)? {
                Some(v) => v,
                None => return Ok(false),
            },
            _ => return Ok(false),
        };
        vm.extra_roots().push(v);
        Ok(true)
    }

    fn open(&mut self) -> bool {
        self.pos += 1;
        self.depth += 1;
        self.depth <= MAX_DEPTH
    }

    fn array(&mut self, vm: &mut Vm) -> RtResult<bool> {
        if !self.open() {
            return Ok(false);
        }
        let base = vm.extra_roots().len();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            loop {
                if !self.value(vm)? {
                    return Ok(false);
                }
                self.ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Ok(false),
                }
            }
        }
        self.depth -= 1;
        let mut items = try_vec(vm.extra_roots().len() - base)?;
        items.extend_from_slice(&vm.extra_roots()[base..]);
        // The items stay rooted until the list holds them.
        let l = new_list(vm, items)?;
        vm.extra_roots().truncate(base);
        vm.extra_roots().push(l);
        Ok(true)
    }

    fn object(&mut self, vm: &mut Vm) -> RtResult<bool> {
        if !self.open() {
            return Ok(false);
        }
        let base = vm.extra_roots().len();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                self.ws();
                if self.peek() != Some(b'"') || !self.string() {
                    return Ok(false);
                }
                let k = match self.keys.get(self.buf.as_str()) {
                    Some(&k) => k,
                    None => {
                        let k = vm.m.try_alloc_str(&self.buf)?;
                        if self.keys.len() < MAX_KEYS {
                            self.keys.insert(self.buf.as_str().into(), k);
                        }
                        k
                    }
                };
                vm.extra_roots().push(k);
                self.ws();
                if self.peek() != Some(b':') {
                    return Ok(false);
                }
                self.pos += 1;
                if !self.value(vm)? {
                    return Ok(false);
                }
                self.ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Ok(false),
                }
            }
        }
        self.depth -= 1;
        // Sort the (key, value) pairs by key where they are rooted; the sort is stable, so
        // inserting in this order leaves the last of duplicate keys.
        let n = (vm.extra_roots().len() - base) / 2;
        sort_pairs(&mut vm.extra_roots()[base..]);
        let table = match DictTable::try_record(n) {
            Ok(t) => t,
            Err(l) => return catallaxy_runtime::refused(format!("cannot allocate a table of {} bytes", l.size())),
        };
        let d = vm.m.alloc_dict(table);
        vm.extra_roots().push(d);
        for i in 0..n {
            let (k, x) = (vm.extra_roots()[base + 2 * i], vm.extra_roots()[base + 2 * i + 1]);
            slow::dict_set(vm, d, k, x)?;
        }
        vm.extra_roots().truncate(base);
        vm.extra_roots().push(d);
        Ok(true)
    }

    /// Reads the string literal at `pos` (its opening quote) into `buf`, decoding escapes.
    fn string(&mut self) -> bool {
        self.pos += 1;
        self.buf.clear();
        loop {
            let start = self.pos;
            while let Some(&c) = self.b.get(self.pos) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // Both ends are at ASCII bytes (or the end), so this is whole characters.
            self.buf.push_str(&self.text[start..self.pos]);
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return true;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let Some(e) = self.peek() else { return false };
                    self.pos += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => match self.unicode_escape() {
                            Some(c) => c,
                            None => return false,
                        },
                        _ => return false,
                    };
                    self.buf.push(c);
                }
                // The end of the text, or a control character.
                _ => return false,
            }
        }
    }

    /// The character of a `\u` escape whose `u` was just read, joining a surrogate pair.
    fn unicode_escape(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if !(0xD800..0xE000).contains(&hi) {
            return char::from_u32(hi);
        }
        if hi >= 0xDC00 || !self.word(b"\\u") {
            return None;
        }
        let lo = self.hex4()?;
        if !(0xDC00..0xE000).contains(&lo) {
            return None;
        }
        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.b.get(self.pos..self.pos + 4)?;
        let mut n = 0;
        for &d in digits {
            n = n * 16 + (d as char).to_digit(16)?;
        }
        self.pos += 4;
        Some(n)
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        self.pos - start
    }

    /// Reads a number: an `Int` (of any size) without a fraction or exponent, else a `Float`.
    /// `None` if it is malformed or out of a float's range.
    fn number(&mut self, vm: &mut Vm) -> RtResult<Option<Value>> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Ok(None),
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Ok(None);
            }
            float = true;
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Ok(None);
            }
            float = true;
        }
        let t = &self.text[start..self.pos];
        if !float {
            if let Ok(i) = t.parse::<i64>() {
                return Ok(Some(vm.m.int(i)));
            }
        }
        // serde_json reads an integer wider than 64 bits as a float, so both must be within a
        // float's range. Near its edge, ask serde_json, whose rounding decides.
        let f: f64 = t.parse().unwrap_or(f64::INFINITY);
        if f.abs() >= 1e308 && serde_json::from_str::<JsonCheck>(t).is_err() {
            return Ok(None);
        }
        Ok(Some(if float { Value::from_f64(f) } else { vm.m.int_from_big(t.parse().unwrap_or_default()) }))
    }
}

/// Sorts `flat`, (string key, value) pairs laid out one after the other, by key. Stable, so
/// equal keys keep their order.
fn sort_pairs(flat: &mut [Value]) {
    let key = |v: Value| str_of(v).unwrap_or("");
    let n = flat.len() / 2;
    if n <= 16 {
        // Insertion sort, in place.
        for i in 1..n {
            let mut j = i;
            while j > 0 && key(flat[2 * j - 2]) > key(flat[2 * j]) {
                flat.swap(2 * j - 2, 2 * j);
                flat.swap(2 * j - 1, 2 * j + 1);
                j -= 1;
            }
        }
        return;
    }
    let mut pairs: Vec<(Value, Value)> = flat.chunks(2).map(|c| (c[0], c[1])).collect();
    pairs.sort_by(|p, q| key(p.0).cmp(key(q.0)));
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        flat[2 * i] = k;
        flat[2 * i + 1] = v;
    }
}

/// Checks JSON text as parsing it into a `serde_json::Value` would (the same errors, including
/// numbers out of range), without building anything. Used for error messages.
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

/// `Json.stringify(v)` (`pretty` false) or `Json.pretty(v)`.
pub(crate) fn stringify(vm: &mut Vm, v: Value, pretty: bool) -> RtResult<Value> {
    // Covers the recursion below.
    catallaxy_runtime::stack::check_extra(JSON_STACK)?;
    let mut out = String::new();
    let base = vm.extra_roots().len();
    let r = write(vm, v, &mut out, pretty.then_some(0), 0);
    vm.extra_roots().truncate(base);
    r?;
    vm.m.try_alloc_str(&out)
}

/// Appends what serde_json writes for `x` (a string, escaped and quoted, or a float).
fn write_serde<T: serde::Serialize + ?Sized>(out: &mut String, x: &T) {
    // SAFETY: serde_json writes JSON text, which is UTF-8, and writing to a `Vec` only appends
    // and cannot fail part way.
    let _ = serde_json::to_writer(unsafe { out.as_mut_vec() }, x);
}

fn newline(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// Before item `i` of a container: the comma and, when pretty, the line break and indent.
fn item_sep(out: &mut String, i: usize, inner: Option<usize>) {
    if i > 0 {
        out.push(',');
    }
    if let Some(l) = inner {
        newline(out, l);
    }
}

/// After the items of a non-empty container: when pretty, the break before its closing bracket.
fn close(out: &mut String, indent: Option<usize>, bracket: char) {
    if let Some(l) = indent {
        newline(out, l);
    }
    out.push(bracket);
}

/// Writes `v` as JSON: compact when `indent` is `None`, else in serde_json's pretty style
/// (two-space indent, `"key": value`) at nesting level `indent`. Object keys are sorted, the
/// last of equal keys winning. Values it takes from containers stay on the extra roots while
/// user code (`to_json`, `to_s`) may run.
fn write(vm: &mut Vm, v: Value, out: &mut String, indent: Option<usize>, depth: usize) -> RtResult<()> {
    if depth > MAX_WRITE_DEPTH {
        return panic("JSON nesting too deep");
    }
    if v.is_nil() {
        out.push_str("null");
        return Ok(());
    }
    if v.is_bool() {
        out.push_str(if v.is_true() { "true" } else { "false" });
        return Ok(());
    }
    if v.is_small_int() {
        let _ = write!(out, "{}", v.as_small_int());
        return Ok(());
    }
    if v.is_float() {
        // Non-finite floats are written as `null`.
        write_serde(out, &v.as_f64());
        return Ok(());
    }
    let inner = indent.map(|l| l + 1);
    match kind_of(v) {
        ObjKind::BigInt => out.push_str(&ops::format_int(v)),
        ObjKind::Str => write_serde(out, s(v)?),
        ObjKind::List | ObjKind::Tuple => {
            let base = vm.extra_roots().len();
            if kind_of(v) == ObjKind::Tuple {
                // SAFETY: checked to be a tuple just above.
                let elems = unsafe { as_tuple(v) }.elems();
                vm.extra_roots().extend_from_slice(elems);
            } else {
                let items = list_read(vm, v)?.try_to_vec()?;
                vm.extra_roots().extend_from_slice(&items);
            }
            let n = vm.extra_roots().len() - base;
            out.push('[');
            if n == 0 {
                out.push(']');
                return Ok(());
            }
            for i in 0..n {
                item_sep(out, i, inner);
                let x = vm.extra_roots()[base + i];
                write(vm, x, out, inner, depth + 1)?;
            }
            vm.extra_roots().truncate(base);
            close(out, indent, ']');
        }
        ObjKind::Dict => {
            let base = vm.extra_roots().len();
            let pairs: Vec<(Value, Value)> =
                catallaxy_runtime::ownership::read_dict(&vm.m, v, |t| t.iter().map(|e| (e.key, e.value)).collect())?;
            // (key text, value) pairs on the roots: keys that are not strings become their `to_s`.
            for (k, x) in pairs {
                let key = if str_of(k).is_some() {
                    k
                } else {
                    vm.extra_roots().push(x);
                    let t = to_s(vm, k)?;
                    vm.extra_roots().pop();
                    vm.m.try_alloc_str(&t)?
                };
                vm.extra_roots().push(key);
                vm.extra_roots().push(x);
            }
            sort_pairs(&mut vm.extra_roots()[base..]);
            let n = (vm.extra_roots().len() - base) / 2;
            write_object(vm, out, indent, depth, base, n)?;
            vm.extra_roots().truncate(base);
        }
        _ => {
            // Objects: what their `to_json` returns, their fields as an object, or to_s.
            let cls = class_of(v);
            if cls.kind == ObjKind::Instance && cls.lookup(intern("to_json")).is_some() {
                let r = vm.call_method_named(v, intern("to_json"), &[])?;
                vm.extra_roots().push(r);
                write(vm, r, out, indent, depth + 1)?;
                vm.extra_roots().pop();
                return Ok(());
            }
            if cls.kind == ObjKind::Instance && cls.variant_tag.is_none() {
                // SAFETY: checked to be an instance just above.
                let o = unsafe { as_instance(v) };
                let base = vm.extra_roots().len();
                let mut order: Vec<usize> = (0..cls.nfields()).collect();
                order.sort_by_key(|&i| name(cls.field_names[i]));
                for &i in &order {
                    let key = vm.m.try_alloc_str(name(cls.field_names[i]))?;
                    vm.extra_roots().push(key);
                    vm.extra_roots().push(o.get(i));
                }
                write_object(vm, out, indent, depth, base, order.len())?;
                vm.extra_roots().truncate(base);
                return Ok(());
            }
            let t = to_s(vm, v)?;
            write_serde(out, &t);
        }
    }
    Ok(())
}

/// Writes the `n` (key, value) pairs on the extra roots from `base`, sorted by key, as an object;
/// of equal keys only the last is written.
fn write_object(
    vm: &mut Vm,
    out: &mut String,
    indent: Option<usize>,
    depth: usize,
    base: usize,
    n: usize,
) -> RtResult<()> {
    let inner = indent.map(|l| l + 1);
    out.push('{');
    let mut written = 0;
    for i in 0..n {
        let (k, x) = (vm.extra_roots()[base + 2 * i], vm.extra_roots()[base + 2 * i + 1]);
        if i + 1 < n && str_of(vm.extra_roots()[base + 2 * i + 2]) == str_of(k) {
            continue;
        }
        item_sep(out, written, inner);
        write_serde(out, str_of(k).unwrap_or(""));
        out.push_str(if indent.is_some() { ": " } else { ":" });
        write(vm, x, out, inner, depth + 1)?;
        written += 1;
    }
    if written == 0 {
        out.push('}');
    } else {
        close(out, indent, '}');
    }
    Ok(())
}
