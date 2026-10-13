//! Format specifications: the mini-language after the colon in `"#{x:>8.2f}"`.
//!
//! ```text
//! spec      := [[fill] align] [sign] ["#"] ["0"] [width] [grouping] ["." precision] [type]
//! align     := "<" | ">" | "^" | "="
//! sign      := "+" | "-" | " "
//! grouping  := "," | "_"
//! type      := "s" | "d" | "x" | "X" | "o" | "b" | "e" | "E" | "f" | "F" | "%"
//! ```
//!
//! The compiler parses a spec once, checks it against the interpolated expression's static type
//! and encodes it into two instruction data words ([`FormatSpec::encode`]); the VM decodes those
//! with a few shifts ([`FormatSpec::decode`]) and calls [`format_value`]. Nothing is reparsed at
//! run time. `String.format` parses its template at run time with the same [`parse`].

use num_bigint::Sign as BigSign;

use crate::mutator::{panic, RtResult};
use crate::ops::{self, Interop};
use crate::{class_of, Value};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Align {
    /// Numbers right, text left.
    Default,
    Left,
    Right,
    Center,
    /// Padding goes between the sign (and `0x`-style prefix) and the digits.
    AfterSign,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Sign {
    /// `-`: only negative numbers get a sign (the default).
    Minus,
    /// `+`: always write a sign.
    Plus,
    /// ` `: a space in front of non-negative numbers.
    Space,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Grouping {
    None,
    /// `,` every three decimal digits.
    Comma,
    /// `_` every three decimal digits, or every four hex/octal/binary digits.
    Underscore,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FmtType {
    /// No type letter: `d` for Int, the shortest form (or `f` with a precision) for Float, text
    /// for everything else.
    Default,
    /// `s`: the value's `to_s`.
    Str,
    Dec,
    Hex,
    HexUpper,
    Oct,
    Bin,
    Exp,
    ExpUpper,
    Fixed,
    FixedUpper,
    Percent,
}

impl FmtType {
    const ALL: [FmtType; 12] = [
        FmtType::Default,
        FmtType::Str,
        FmtType::Dec,
        FmtType::Hex,
        FmtType::HexUpper,
        FmtType::Oct,
        FmtType::Bin,
        FmtType::Exp,
        FmtType::ExpUpper,
        FmtType::Fixed,
        FmtType::FixedUpper,
        FmtType::Percent,
    ];

    fn from_char(c: char) -> Option<FmtType> {
        Some(match c {
            's' => FmtType::Str,
            'd' => FmtType::Dec,
            'x' => FmtType::Hex,
            'X' => FmtType::HexUpper,
            'o' => FmtType::Oct,
            'b' => FmtType::Bin,
            'e' => FmtType::Exp,
            'E' => FmtType::ExpUpper,
            'f' => FmtType::Fixed,
            'F' => FmtType::FixedUpper,
            '%' => FmtType::Percent,
            _ => return None,
        })
    }

    fn letter(self) -> &'static str {
        match self {
            FmtType::Default => "",
            FmtType::Str => "s",
            FmtType::Dec => "d",
            FmtType::Hex => "x",
            FmtType::HexUpper => "X",
            FmtType::Oct => "o",
            FmtType::Bin => "b",
            FmtType::Exp => "e",
            FmtType::ExpUpper => "E",
            FmtType::Fixed => "f",
            FmtType::FixedUpper => "F",
            FmtType::Percent => "%",
        }
    }

    fn is_radix(self) -> bool {
        matches!(self, FmtType::Hex | FmtType::HexUpper | FmtType::Oct | FmtType::Bin)
    }

    fn is_int_only(self) -> bool {
        self == FmtType::Dec || self.is_radix()
    }

    fn is_float(self) -> bool {
        matches!(self, FmtType::Exp | FmtType::ExpUpper | FmtType::Fixed | FmtType::FixedUpper | FmtType::Percent)
    }
}

/// The largest width; precision is limited to one less (it is stored plus one).
pub const MAX_WIDTH: u16 = 0x3fff;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FormatSpec {
    pub fill: char,
    pub align: Align,
    pub sign: Sign,
    /// `#`: `0x`/`0o`/`0b` prefix.
    pub alt: bool,
    /// `0`: pad with zeros after the sign.
    pub zero: bool,
    pub width: u16,
    pub grouping: Grouping,
    pub precision: Option<u16>,
    pub ty: FmtType,
}

impl Default for FormatSpec {
    fn default() -> Self {
        FormatSpec {
            fill: ' ',
            align: Align::Default,
            sign: Sign::Minus,
            alt: false,
            zero: false,
            width: 0,
            grouping: Grouping::None,
            precision: None,
            ty: FmtType::Default,
        }
    }
}

/// A malformed spec: the message and the byte range in the spec text it refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError {
    pub msg: String,
    pub start: usize,
    pub end: usize,
}

/// What a value formats as.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Int,
    Float,
    /// Anything else, formatted through `to_s`.
    Text,
}

fn align_of(c: char) -> Option<Align> {
    Some(match c {
        '<' => Align::Left,
        '>' => Align::Right,
        '^' => Align::Center,
        '=' => Align::AfterSign,
        _ => return None,
    })
}

/// Parses a spec (the text after the colon).
pub fn parse(spec: &str) -> Result<FormatSpec, SpecError> {
    let err = |msg: String, start: usize, end: usize| Err(SpecError { msg, start, end });
    if spec.is_empty() {
        return err("empty format spec".into(), 0, 0);
    }
    let cs: Vec<(usize, char)> = spec.char_indices().collect();
    let at = |i: usize| cs.get(i).map(|&(_, c)| c);
    let pos = |i: usize| cs.get(i).map_or(spec.len(), |&(p, _)| p);
    let mut f = FormatSpec::default();
    let mut i = 0;
    if let Some(a) = at(1).and_then(align_of) {
        f.fill = cs[0].1;
        f.align = a;
        i = 2;
    } else if let Some(a) = at(0).and_then(align_of) {
        f.align = a;
        i = 1;
    }
    if let Some(c @ ('+' | ' ' | '-')) = at(i) {
        f.sign = match c {
            '+' => Sign::Plus,
            ' ' => Sign::Space,
            _ => Sign::Minus,
        };
        i += 1;
    }
    if at(i) == Some('#') {
        f.alt = true;
        i += 1;
    }
    if at(i) == Some('0') {
        f.zero = true;
        i += 1;
    }
    let ws = i;
    while at(i).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
    }
    if i > ws {
        match spec[pos(ws)..pos(i)].parse::<u32>() {
            Ok(w) if w <= MAX_WIDTH as u32 => f.width = w as u16,
            _ => return err(format!("width is larger than {MAX_WIDTH}"), pos(ws), pos(i)),
        }
    }
    if let Some(c @ (',' | '_')) = at(i) {
        f.grouping = if c == ',' { Grouping::Comma } else { Grouping::Underscore };
        i += 1;
    }
    if at(i) == Some('.') {
        let dot = i;
        i += 1;
        let ps = i;
        while at(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        if i == ps {
            return err("expected digits after `.`".into(), pos(dot), pos(i));
        }
        match spec[pos(ps)..pos(i)].parse::<u32>() {
            Ok(p) if p < MAX_WIDTH as u32 => f.precision = Some(p as u16),
            _ => return err(format!("precision is larger than {}", MAX_WIDTH - 1), pos(ps), pos(i)),
        }
    }
    if let Some(c) = at(i) {
        if let Some(t) = FmtType::from_char(c) {
            f.ty = t;
            i += 1;
        } else if c.is_ascii_alphabetic() && i + 1 == cs.len() {
            return err(
                format!("unknown format type `{c}` (expected one of s d x X o b e E f F %)"),
                pos(i),
                pos(i + 1),
            );
        }
    }
    if i < cs.len() {
        return err(format!("unexpected `{}` in format spec", &spec[pos(i)..]), pos(i), spec.len());
    }
    Ok(f)
}

impl FormatSpec {
    /// Packs the spec into two instruction data words.
    pub fn encode(&self) -> [u32; 2] {
        let w0 = self.fill as u32
            | (self.align as u32) << 21
            | (self.sign as u32) << 24
            | (self.alt as u32) << 26
            | (self.zero as u32) << 27
            | (self.grouping as u32) << 28;
        let prec = self.precision.map_or(0, |p| p as u32 + 1);
        let w1 = self.ty as u32 | (self.width as u32 & 0x3fff) << 4 | (prec & 0x3fff) << 18;
        [w0, w1]
    }

    /// Unpacks [`FormatSpec::encode`]'s words.
    #[inline]
    pub fn decode(w: [u32; 2]) -> FormatSpec {
        const ALIGNS: [Align; 5] = [Align::Default, Align::Left, Align::Right, Align::Center, Align::AfterSign];
        const SIGNS: [Sign; 3] = [Sign::Minus, Sign::Plus, Sign::Space];
        const GROUPS: [Grouping; 3] = [Grouping::None, Grouping::Comma, Grouping::Underscore];
        let [w0, w1] = w;
        let prec = (w1 >> 18) & 0x3fff;
        FormatSpec {
            fill: char::from_u32(w0 & 0x1f_ffff).unwrap_or(' '),
            align: ALIGNS[((w0 >> 21) & 7) as usize % 5],
            sign: SIGNS[((w0 >> 24) & 3) as usize % 3],
            alt: (w0 >> 26) & 1 != 0,
            zero: (w0 >> 27) & 1 != 0,
            grouping: GROUPS[((w0 >> 28) & 3) as usize % 3],
            width: ((w1 >> 4) & 0x3fff) as u16,
            precision: if prec == 0 { None } else { Some(prec as u16 - 1) },
            ty: FmtType::ALL[(w1 & 0xf) as usize % 12],
        }
    }

    /// Whether the spec can format a value of `kind`; `type_name` names it in the error.
    pub fn check(&self, kind: ValueKind, type_name: &str) -> Result<(), String> {
        let t = self.ty;
        let numeric = match kind {
            ValueKind::Int => t != FmtType::Str,
            ValueKind::Float => {
                if t.is_int_only() {
                    return Err(format!("format type `{}` needs an Int, found {type_name}", t.letter()));
                }
                t != FmtType::Str
            }
            ValueKind::Text => {
                if t.is_int_only() || t.is_float() {
                    let what = if t.is_float() { "a number" } else { "an Int" };
                    return Err(format!("format type `{}` needs {what}, found {type_name}", t.letter()));
                }
                false
            }
        };
        if !numeric {
            let flag = if self.sign != Sign::Minus {
                Some(if self.sign == Sign::Plus { "+" } else { " " })
            } else if self.alt {
                Some("#")
            } else if self.zero {
                Some("0")
            } else if self.grouping != Grouping::None {
                Some(if self.grouping == Grouping::Comma { "," } else { "_" })
            } else if self.align == Align::AfterSign {
                Some("=")
            } else {
                None
            };
            if let Some(flag) = flag {
                return Err(format!("`{flag}` applies only to numbers, found {type_name}"));
            }
            return Ok(());
        }
        if self.alt && !t.is_radix() {
            return Err("`#` needs one of the types x X o b".into());
        }
        if self.grouping == Grouping::Comma && t.is_radix() {
            return Err(format!("`,` grouping does not apply to `{}`; use `_`", t.letter()));
        }
        if kind == ValueKind::Int && self.precision.is_some() && (t == FmtType::Default || t.is_int_only()) {
            return Err("an Int format takes no precision; use `f` or `e` to format it as a Float".into());
        }
        Ok(())
    }
}

/// The kind of a runtime value, for [`FormatSpec::check`].
pub fn kind_of_value(v: Value) -> ValueKind {
    if ops::is_int(v) {
        ValueKind::Int
    } else if v.is_float() {
        ValueKind::Float
    } else {
        ValueKind::Text
    }
}

/// Formats `v` per `spec`, panicking with a `FormatError` if the spec does not apply to it (the
/// compiler rejects that statically unless the value is `Dyn` or generic).
pub fn format_value(ix: &mut dyn Interop, v: Value, spec: &FormatSpec) -> RtResult<String> {
    let kind = kind_of_value(v);
    if let Err(msg) = spec.check(kind, &class_of(v).name) {
        return panic(format!("FormatError: {msg}"));
    }
    let mut out = String::new();
    match (kind, spec.ty) {
        (_, FmtType::Str) | (ValueKind::Text, _) => {
            let text = ops::to_s(ix, v)?;
            spec.write_text(&mut out, &text);
        }
        (ValueKind::Int, t) if !t.is_float() => spec.write_int(&mut out, v),
        (ValueKind::Int, _) => spec.write_float(&mut out, ops::int_to_f64(v)),
        (ValueKind::Float, _) => spec.write_float(&mut out, v.as_f64()),
    }
    Ok(out)
}

/// Inserts `sep` every `every` digits from the right of `digits`. With `min_len`, pads `digits`
/// with leading zeros (regrouping as it goes) until the result is at least that long.
fn group(digits: &str, sep: char, every: usize, min_len: usize) -> String {
    let mut digits = digits.to_string();
    loop {
        let n = digits.len();
        let mut out = String::with_capacity(n + n / every);
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (n - i) % every == 0 {
                out.push(sep);
            }
            out.push(c);
        }
        if out.chars().count() >= min_len {
            return out;
        }
        digits.insert(0, '0');
    }
}

impl FormatSpec {
    fn sign_str(&self, negative: bool) -> &'static str {
        match (negative, self.sign) {
            (true, _) => "-",
            (false, Sign::Plus) => "+",
            (false, Sign::Space) => " ",
            (false, Sign::Minus) => "",
        }
    }

    /// Pads `sign prefix body` to the width.
    fn pad(&self, out: &mut String, sign: &str, prefix: &str, body: &str, numeric: bool) {
        let len = sign.chars().count() + prefix.chars().count() + body.chars().count();
        let pad = (self.width as usize).saturating_sub(len);
        let (fill, align) = match self.align {
            Align::Default if self.zero => ('0', Align::AfterSign),
            Align::Default if numeric => (self.fill, Align::Right),
            Align::Default => (self.fill, Align::Left),
            a => (self.fill, a),
        };
        let fills = |out: &mut String, n: usize| out.extend(std::iter::repeat_n(fill, n));
        match align {
            Align::Left => {
                out.push_str(sign);
                out.push_str(prefix);
                out.push_str(body);
                fills(out, pad);
            }
            Align::Center => {
                fills(out, pad / 2);
                out.push_str(sign);
                out.push_str(prefix);
                out.push_str(body);
                fills(out, pad - pad / 2);
            }
            Align::AfterSign => {
                out.push_str(sign);
                out.push_str(prefix);
                fills(out, pad);
                out.push_str(body);
            }
            Align::Right | Align::Default => {
                fills(out, pad);
                out.push_str(sign);
                out.push_str(prefix);
                out.push_str(body);
            }
        }
    }

    /// With the `0` flag and grouping, the zero padding is grouped too (`0,001,234`).
    fn grouped(&self, digits: &str, every: usize, others: usize) -> String {
        let sep = match self.grouping {
            Grouping::None => return digits.to_string(),
            Grouping::Comma => ',',
            Grouping::Underscore => '_',
        };
        let min =
            if self.zero && self.align == Align::Default { (self.width as usize).saturating_sub(others) } else { 0 };
        group(digits, sep, every, min)
    }

    fn write_text(&self, out: &mut String, text: &str) {
        let text = match self.precision {
            Some(p) => match text.char_indices().nth(p as usize) {
                Some((i, _)) => &text[..i],
                None => text,
            },
            None => text,
        };
        self.pad(out, "", "", text, false);
    }

    fn write_int(&self, out: &mut String, v: Value) {
        let (radix, upper, prefix) = match self.ty {
            FmtType::Hex => (16, false, "0x"),
            FmtType::HexUpper => (16, true, "0X"),
            FmtType::Oct => (8, false, "0o"),
            FmtType::Bin => (2, false, "0b"),
            _ => (10, false, ""),
        };
        let (negative, mut digits) = if v.is_small_int() {
            let n = v.as_small_int();
            let m = n.unsigned_abs();
            let d = match radix {
                16 => format!("{m:x}"),
                8 => format!("{m:o}"),
                2 => format!("{m:b}"),
                _ => m.to_string(),
            };
            (n < 0, d)
        } else {
            let b = ops::to_bigint(v);
            (b.sign() == BigSign::Minus, b.magnitude().to_str_radix(radix))
        };
        if upper {
            digits.make_ascii_uppercase();
        }
        let sign = self.sign_str(negative);
        let prefix = if self.alt { prefix } else { "" };
        let every = if radix == 10 { 3 } else { 4 };
        let body = self.grouped(&digits, every, sign.len() + prefix.len());
        self.pad(out, sign, prefix, &body, true);
    }

    fn write_float(&self, out: &mut String, x: f64) {
        let upper = matches!(self.ty, FmtType::ExpUpper | FmtType::FixedUpper);
        let negative = x.is_sign_negative() && !x.is_nan();
        let sign = self.sign_str(negative);
        let a = x.abs();
        let mut body = if !a.is_finite() {
            if a.is_nan() { "NaN" } else { "Infinity" }.to_string()
        } else {
            let p = self.precision.unwrap_or(6) as usize;
            match self.ty {
                FmtType::Exp | FmtType::ExpUpper => {
                    let s = format!("{a:.p$e}");
                    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
                    let (esign, edigits) = match exp.strip_prefix('-') {
                        Some(d) => ('-', d),
                        None => ('+', exp),
                    };
                    format!("{mant}e{esign}{edigits:0>2}")
                }
                FmtType::Percent => format!("{:.p$}%", a * 100.0),
                FmtType::Default if self.precision.is_none() => ops::format_float(a),
                _ => format!("{a:.p$}"),
            }
        };
        if upper {
            body.make_ascii_uppercase();
        }
        if self.grouping != Grouping::None && a.is_finite() {
            let end = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
            let rest_len = body.len() - end;
            let int = self.grouped(&body[..end], 3, sign.len() + rest_len);
            body = format!("{int}{}", &body[end..]);
        }
        self.pad(out, sign, "", &body, true);
    }
}

/// Formats a runtime template: `{}` takes the next argument, `{:spec}` formats it with a spec,
/// `{{` and `}}` are literal braces. Errors (a bad spec, too few or too many arguments) panic.
pub fn format_template(ix: &mut dyn Interop, template: &str, args: &[Value]) -> RtResult<String> {
    let mut out = String::new();
    let mut next = 0;
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let c = rest.as_bytes()[i];
        rest = &rest[i + 1..];
        if rest.as_bytes().first() == Some(&c) {
            out.push(c as char);
            rest = &rest[1..];
            continue;
        }
        if c == b'}' {
            return panic("FormatError: unmatched `}` in format template; write `}}` for a literal brace");
        }
        let Some(close) = rest.find('}') else {
            return panic("FormatError: unterminated `{` in format template; write `{{` for a literal brace");
        };
        let inner = &rest[..close];
        rest = &rest[close + 1..];
        let spec = match inner.strip_prefix(':') {
            Some(s) => match parse(s) {
                Ok(f) => f,
                Err(e) => return panic(format!("FormatError: {} in `{{{inner}}}`", e.msg)),
            },
            None if inner.is_empty() => FormatSpec::default(),
            None => return panic(format!("FormatError: expected `{{}}` or `{{:spec}}`, found `{{{inner}}}`")),
        };
        let Some(&v) = args.get(next) else {
            return panic(format!("FormatError: the template needs more than {} argument(s)", args.len()));
        };
        next += 1;
        if spec == FormatSpec::default() {
            out.push_str(&ops::to_s(ix, v)?);
        } else {
            out.push_str(&format_value(ix, v, &spec)?);
        }
    }
    out.push_str(rest);
    if next < args.len() {
        return panic(format!("FormatError: the template uses {next} argument(s) but {} were given", args.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips() {
        for s in ["d", ">8.2f", "*^10", "+,d", "#x", "#010b", "_X", ".3e", "08,.1f", " 5", "=+9", "é<4s", ".0%"] {
            let f = parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"));
            assert_eq!(FormatSpec::decode(f.encode()), f, "{s}");
        }
        assert_eq!(parse(">8").unwrap().width, 8);
        assert_eq!(parse("*^10").unwrap().fill, '*');
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse("").is_err());
        assert!(parse("8.").is_err());
        assert!(parse("q").unwrap_err().msg.contains("unknown format type"));
        assert!(parse("8.2fx").is_err());
        assert!(parse("99999").is_err());
    }

    #[test]
    fn checks_kinds() {
        let f = parse(".2f").unwrap();
        assert!(f.check(ValueKind::Float, "Float").is_ok());
        assert!(f.check(ValueKind::Int, "Int").is_ok());
        assert!(f.check(ValueKind::Text, "String").is_err());
        assert!(parse("x").unwrap().check(ValueKind::Float, "Float").is_err());
        assert!(parse(".2").unwrap().check(ValueKind::Int, "Int").is_err());
        assert!(parse(".2").unwrap().check(ValueKind::Text, "String").is_ok());
        assert!(parse("+").unwrap().check(ValueKind::Text, "String").is_err());
        assert!(parse("#d").unwrap().check(ValueKind::Int, "Int").is_err());
        assert!(parse(",x").unwrap().check(ValueKind::Int, "Int").is_err());
    }

    #[test]
    fn groups_with_zero_padding() {
        assert_eq!(group("1234", ',', 3, 0), "1,234");
        assert_eq!(group("1234", ',', 3, 8), "0,001,234");
        assert_eq!(group("123", ',', 3, 0), "123");
    }

    fn fmt_f(spec: &str, x: f64) -> String {
        let mut out = String::new();
        parse(spec).unwrap().write_float(&mut out, x);
        out
    }

    fn fmt_t(spec: &str, s: &str) -> String {
        let mut out = String::new();
        parse(spec).unwrap().write_text(&mut out, s);
        out
    }

    #[test]
    fn formats_floats_and_text() {
        assert_eq!(fmt_f(".2f", 3.14159), "3.14");
        assert_eq!(fmt_f(">8.2f", -3.14159), "   -3.14");
        assert_eq!(fmt_f("08.2f", -3.14159), "-0003.14");
        assert_eq!(fmt_f(",.2f", 1234567.891), "1,234,567.89");
        assert_eq!(fmt_f(".3e", 1234.5), "1.234e+03");
        assert_eq!(fmt_f("E", 0.00012), "1.200000E-04");
        assert_eq!(fmt_f(".1%", 0.256), "25.6%");
        assert_eq!(fmt_f("+", 2.5), "+2.5");
        assert_eq!(fmt_f("f", f64::NAN), "NaN");
        assert_eq!(fmt_t("*^7", "abc"), "**abc**");
        assert_eq!(fmt_t("6", "ab"), "ab    ");
        assert_eq!(fmt_t(".2", "héllo"), "hé");
    }
}
