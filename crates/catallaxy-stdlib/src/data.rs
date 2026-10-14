//! Data formats: `Bytes`, binary files, hex and base64, digests (`Digest`) and CSV (`Csv`).
//!
//! A `Bytes` value is a frozen prelude instance whose `handle` holds a [`ByteData`]: immutable,
//! so it is shared between threads and fibers like a `String`.

use base64::Engine as _;
use catallaxy_runtime::class::class_of;
use catallaxy_runtime::object::kind_of;
use catallaxy_runtime::{check_alloc, ops, ownership, panic, try_push, try_string, try_vec, ObjKind, RtResult, Value};
use catallaxy_vm::{NativeFn, Vm};

use crate::fiber::blocking_io;
use crate::script::{handle_data, instance};
use crate::*;

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "Bytes.new" => |vm, a| {
            let mut out = Vec::new();
            if let Some(l) = arg(a, 0) {
                let items = list_items(vm, l)?;
                out = try_vec(items.len())?;
                for v in items {
                    let b = int(v)?;
                    if !(0..=255).contains(&b) {
                        return panic(format!("ArgumentError: byte value {b} is not in 0..255"));
                    }
                    out.push(b as u8);
                }
            }
            new_bytes(vm, out)
        },
        "Bytes.from_hex" => |vm, a| match hex_decode(s(a[0])?) {
            Ok(b) => {
                let v = new_bytes(vm, b)?;
                Ok(ok(vm, v))
            }
            Err(e) => Ok(err(vm, &e)),
        },
        "Bytes.from_base64" => |vm, a| match base64::engine::general_purpose::STANDARD.decode(s(a[0])?.trim()) {
            Ok(b) => {
                let v = new_bytes(vm, b)?;
                Ok(ok(vm, v))
            }
            Err(e) => Ok(err(vm, &format!("invalid base64: {e}"))),
        },
        "Bytes.length" => |vm, a| Ok(vm.m.int(bytes_of(a[0])?.len() as i64)),
        "Bytes.[]" => |_, a| {
            let b = bytes_of(a[0])?;
            let i = int(a[1])?;
            match norm_index(i, b.len()) {
                Some(ix) => Ok(Value::small_int(b[ix] as i64)),
                None => panic(format!("IndexError: index {i} out of range for length {}", b.len())),
            }
        },
        "Bytes.get" => |_, a| {
            let b = bytes_of(a[0])?;
            Ok(norm_index(int(a[1])?, b.len()).map_or(Value::NIL, |ix| Value::small_int(b[ix] as i64)))
        },
        "Bytes.slice" => |vm, a| {
            let b = bytes_of(a[0])?;
            let (start, len) = (int(a[1])?, int(a[2])?);
            let n = b.len() as i64;
            let s0 = if start < 0 { (start + n).max(0) } else { start.min(n) };
            let e0 = s0.saturating_add(len.max(0)).min(n);
            let out = bytes_from(&[&b[s0 as usize..e0 as usize]])?;
            new_bytes(vm, out)
        },
        "Bytes.+" => |vm, a| {
            let out = bytes_from(&[bytes_of(a[0])?, bytes_of(a[1])?])?;
            new_bytes(vm, out)
        },
        "Bytes.==" => |_, a| {
            let other = if is_bytes(a[1]) { Some(bytes_of(a[1])?) } else { None };
            Ok(Value::bool(other == Some(bytes_of(a[0])?)))
        },
        "Bytes.hash" => |_, a| Ok(Value::small_int((ops::hash_bytes(bytes_of(a[0])?) >> 17) as i64)),
        "Bytes.to_list" => |vm, a| {
            let b = bytes_of(a[0])?;
            let mut items = vm.m.try_vec_collecting(b.len())?;
            items.extend(b.iter().map(|&x| Value::small_int(x as i64)));
            new_list(vm, items)
        },
        "Bytes.to_hex" => |vm, a| {
            let r = hex_encode(bytes_of(a[0])?)?;
            vm.m.try_alloc_str(&r)
        },
        "Bytes.to_base64" => |vm, a| {
            let b = bytes_of(a[0])?;
            let len = base64::encoded_len(b.len(), true).unwrap_or(usize::MAX);
            let mut r = try_string(len)?;
            base64::engine::general_purpose::STANDARD.encode_string(b, &mut r);
            vm.m.try_alloc_str(&r)
        },
        "Bytes.to_s" => |vm, a| {
            let r = format!("Bytes({})", hex_encode(bytes_of(a[0])?)?);
            vm.m.try_alloc_str(&r)
        },
        "String.to_bytes" => |vm, a| {
            let b = bytes_from(&[s(a[0])?.as_bytes()])?;
            new_bytes(vm, b)
        },
        "String.from_bytes" => |vm, a| match std::str::from_utf8(bytes_of(a[0])?) {
            Ok(text) => {
                let v = vm.m.try_alloc_str(text)?;
                Ok(ok(vm, v))
            }
            Err(e) => Ok(err(vm, &format!("invalid UTF-8: {e}"))),
        },
        "String.from_bytes_lossy" => |vm, a| {
            let text = String::from_utf8_lossy(bytes_of(a[0])?);
            vm.m.try_alloc_str(&text)
        },

        "File.read_bytes" => |vm, a| {
            let p = s(a[0])?.to_string();
            match blocking_io(vm, || std::fs::read(&p)) {
                Ok(b) => {
                    let v = new_bytes(vm, b)?;
                    Ok(ok(vm, v))
                }
                Err(e) => io_err(vm, &p, e),
            }
        },
        "File.write_bytes" => |vm, a| {
            let p = s(a[0])?.to_string();
            // Blocking jobs must not touch the heap (a collection may run meanwhile): copy out.
            let data = bytes_from(&[bytes_of(a[1])?])?;
            Ok(match blocking_io(vm, || std::fs::write(&p, data)) {
                Ok(()) => ok(vm, Value::NIL),
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },

        "Stdin.read_bytes" => |vm, _| {
            let b = crate::core::read_stdin_to_end(vm)?;
            new_bytes(vm, b)
        },

        "Digest.md5" => |vm, a| digest(vm, a[0], |d| md5(d).to_vec()),
        "Digest.sha1" => |vm, a| digest(vm, a[0], |d| ring_digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, d)),
        "Digest.sha256" => |vm, a| digest(vm, a[0], |d| ring_digest(&ring::digest::SHA256, d)),
        "Digest.sha512" => |vm, a| digest(vm, a[0], |d| ring_digest(&ring::digest::SHA512, d)),

        "Csv.parse" => csv_parse,
        "Csv.format" => csv_format,
        "Csv.format_row" => |vm, a| {
            let sep = csv_sep(a, 1)?;
            let row = list_items(vm, a[0])?;
            let mut out = String::new();
            csv_write_row(&mut out, &row, sep)?;
            Ok(new_str(vm, &out))
        },
        _ => return None,
    })
}

// ---- Bytes -----------------------------------------------------------------------------------

/// The contents of a `Bytes` value.
struct ByteData(Vec<u8>);

fn new_bytes(vm: &mut Vm, data: Vec<u8>) -> RtResult<Value> {
    let h = vm.m.alloc_native_frozen(Box::new(ByteData(data)));
    let b = instance(vm, "Bytes", &[("handle", h)])?;
    ownership::freeze_deep(&vm.m, b)?;
    Ok(b)
}

fn is_bytes(v: Value) -> bool {
    kind_of(v) == ObjKind::Instance && &*class_of(v).name == "Bytes"
}

/// The bytes of a `Bytes` value (which outlives the native call that received it).
fn bytes_of(v: Value) -> RtResult<&'static [u8]> {
    Ok(&handle_data::<ByteData>(v, "Bytes")?.0)
}

/// The bytes of a `String | Bytes` argument.
fn data_of(v: Value) -> RtResult<&'static [u8]> {
    if kind_of(v) == ObjKind::Str {
        return Ok(s(v)?.as_bytes());
    }
    bytes_of(v)
}

fn norm_index(i: i64, len: usize) -> Option<usize> {
    let n = len as i64;
    let ix = if i < 0 { i + n } else { i };
    (0..n).contains(&ix).then_some(ix as usize)
}

/// The concatenation of `parts`, or a `MemoryError` panic.
fn bytes_from(parts: &[&[u8]]) -> RtResult<Vec<u8>> {
    let mut out = try_vec(parts.iter().fold(0usize, |n, p| n.saturating_add(p.len())))?;
    for p in parts {
        out.extend_from_slice(p);
    }
    Ok(out)
}

fn hex_encode(b: &[u8]) -> RtResult<String> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = try_string(check_alloc(b.len(), 2)?)?;
    for &x in b {
        out.push(DIGITS[(x >> 4) as usize] as char);
        out.push(DIGITS[(x & 15) as usize] as char);
    }
    Ok(out)
}

fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    let t = text.trim().as_bytes();
    if t.len() % 2 != 0 {
        return Err("invalid hex: odd number of digits".into());
    }
    let digit = |c: u8| -> Result<u8, String> {
        (c as char).to_digit(16).map(|d| d as u8).ok_or_else(|| format!("invalid hex digit `{}`", c as char))
    };
    t.chunks(2).map(|p| Ok(digit(p[0])? << 4 | digit(p[1])?)).collect()
}

// ---- Digests ---------------------------------------------------------------------------------

fn digest(vm: &mut Vm, v: Value, f: impl FnOnce(&[u8]) -> Vec<u8>) -> RtResult<Value> {
    let r = hex_encode(&f(data_of(v)?))?;
    Ok(new_str(vm, &r))
}

fn ring_digest(alg: &'static ring::digest::Algorithm, data: &[u8]) -> Vec<u8> {
    ring::digest::digest(alg, data).as_ref().to_vec()
}

/// MD5 (RFC 1321). `ring` leaves it out as broken for security; scripts still meet it in
/// checksums and cache keys.
fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    let k: [u32; 64] = std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut h: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: [u32; 16] = std::array::from_fn(|i| u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap()));
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        h = [h[0].wrapping_add(a), h[1].wrapping_add(b), h[2].wrapping_add(c), h[3].wrapping_add(d)];
    }
    let mut out = [0u8; 16];
    for (i, w) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

// ---- CSV -------------------------------------------------------------------------------------

/// The separator argument at `i` (default `,`): exactly one character, not a quote or line break.
fn csv_sep(a: &[Value], i: usize) -> RtResult<char> {
    let Some(v) = arg(a, i) else { return Ok(',') };
    let text = s(v)?;
    let mut cs = text.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) if !matches!(c, '"' | '\n' | '\r') => Ok(c),
        _ => panic(format!(
            "ArgumentError: CSV separator must be one character other than a quote or line break, got {text:?}"
        )),
    }
}

/// Parses RFC 4180 text (with any line ending). Blank lines are skipped.
/// The outer `Result` is a `MemoryError` panic; the inner one, invalid text.
fn parse_csv(text: &str, sep: char) -> RtResult<Result<Vec<Vec<String>>, String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut line = 1;
    // Whether the current row has anything (so a blank line is not a row of one empty field).
    let mut started = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if field.is_empty() => {
                let start_line = line;
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            field.push('"');
                        }
                        Some('"') => break,
                        Some(c) => {
                            if c == '\n' {
                                line += 1;
                            }
                            field.push(c);
                        }
                        None => return Ok(Err(format!("line {start_line}: unterminated quoted field"))),
                    }
                }
                match chars.peek() {
                    None | Some('\n' | '\r') => {}
                    Some(&c) if c == sep => {}
                    Some(_) => return Ok(Err(format!("line {line}: unexpected text after a closing quote"))),
                }
                started = true;
            }
            c if c == sep => {
                try_push(&mut row, std::mem::take(&mut field))?;
                started = true;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                if started || !field.is_empty() {
                    try_push(&mut row, std::mem::take(&mut field))?;
                    try_push(&mut rows, std::mem::take(&mut row))?;
                }
                started = false;
            }
            c => field.push(c),
        }
    }
    if started || !field.is_empty() {
        try_push(&mut row, field)?;
        try_push(&mut rows, row)?;
    }
    Ok(Ok(rows))
}

fn csv_parse(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let sep = csv_sep(a, 1)?;
    vm.m.collect_if_refused();
    match parse_csv(s(a[0])?, sep)? {
        Ok(rows) => {
            // Nothing here collects, so the partial lists need no rooting. Each parsed row is
            // freed once converted.
            let mut out = try_vec(rows.len())?;
            for r in rows {
                let l = new_str_list(vm, r.iter().map(|f| f.as_str()))?;
                out.push(l);
            }
            let l = new_list(vm, out)?;
            Ok(ok(vm, l))
        }
        Err(e) => Ok(err(vm, &e)),
    }
}

fn csv_write_row(out: &mut String, row: &[Value], sep: char) -> RtResult<()> {
    if let [only] = row {
        if s(*only)?.is_empty() {
            // A lone empty field must be quoted, or the row would read back as a blank line.
            out.push_str("\"\"");
            return Ok(());
        }
    }
    for (i, &f) in row.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        let f = s(f)?;
        if f.contains([sep, '"', '\n', '\r']) {
            out.push('"');
            out.push_str(&f.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(f);
        }
    }
    Ok(())
}

fn csv_format(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let sep = csv_sep(a, 1)?;
    let mut out = String::new();
    for r in list_items(vm, a[0])? {
        let row = list_items(vm, r)?;
        csv_write_row(&mut out, &row, sep)?;
        out.push('\n');
    }
    vm.m.try_alloc_str(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_vectors() {
        assert_eq!(hex_encode(&md5(b"")).unwrap(), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex_encode(&md5(b"abc")).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        let long = b"12345678901234567890123456789012345678901234567890123456789012345678901234567890";
        assert_eq!(hex_encode(&md5(long)).unwrap(), "57edf4a22be3c955ac49da2e2107b67a");
    }

    #[test]
    fn csv_round_trip() {
        let rows = parse_csv("a,\"b,c\",\"d\"\"e\"\r\n\n\"x\ny\",,\n\"\"\n", ',').unwrap().unwrap();
        assert_eq!(rows, vec![vec!["a", "b,c", "d\"e"], vec!["x\ny", "", ""], vec![""]]);
        assert!(parse_csv("\"abc", ',').unwrap().is_err());
        assert!(parse_csv("\"a\"b", ',').unwrap().is_err());
        assert_eq!(parse_csv("5\" pipe,x", ',').unwrap().unwrap(), vec![vec!["5\" pipe", "x"]]);
    }

    #[test]
    fn hex() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
    }
}
