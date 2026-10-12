//! NaN-boxed values.
//!
//! A [`Value`] is one 64-bit word. Doubles are stored as themselves; the negative quiet-NaN space
//! with the top 16 bits in `0xFFF9..=0xFFFC` encodes everything else:
//!
//! | top 16 bits | payload |
//! |-------------|---------|
//! | `0xFFF9` | 48-bit heap pointer |
//! | `0xFFFA` | 48-bit signed small integer |
//! | `0xFFFB` | special: `nil` (0), `false` (2), `true` (3), and internal markers |
//!
//! Every NaN produced by arithmetic is canonicalized to `0x7FF8_0000_0000_0000`, so no float ever
//! collides with a tag.

use catallaxy_gc::Header;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Value(pub u64);

const TAG_SHIFT: u32 = 48;
const TAG_PTR: u64 = 0xFFF9;
const TAG_INT: u64 = 0xFFFA;
const TAG_SPECIAL: u64 = 0xFFFB;
const PAYLOAD_MASK: u64 = (1 << 48) - 1;
const CANONICAL_NAN: u64 = 0x7FF8_0000_0000_0000;

pub const SMALL_INT_MIN: i64 = -(1 << 47);
pub const SMALL_INT_MAX: i64 = (1 << 47) - 1;

impl Value {
    pub const NIL: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 0);
    pub const FALSE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 2);
    pub const TRUE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 3);
    /// Marks an empty slot in internal tables and an uninitialized register. Never user-visible.
    pub const EMPTY: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 4);
    /// A dict tombstone.
    pub const TOMBSTONE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | 5);

    #[inline(always)]
    pub fn from_f64(f: f64) -> Value {
        if f.is_nan() {
            Value(CANONICAL_NAN)
        } else {
            Value(f.to_bits())
        }
    }

    #[inline(always)]
    pub fn bool(b: bool) -> Value {
        if b {
            Value::TRUE
        } else {
            Value::FALSE
        }
    }

    /// Encodes a small integer. The caller guarantees `i` fits in 48 bits.
    #[inline(always)]
    pub fn small_int(i: i64) -> Value {
        debug_assert!(fits_small(i));
        Value((TAG_INT << TAG_SHIFT) | (i as u64 & PAYLOAD_MASK))
    }

    #[inline(always)]
    pub fn from_ptr(p: *const Header) -> Value {
        debug_assert!((p as u64) & !PAYLOAD_MASK == 0);
        Value((TAG_PTR << TAG_SHIFT) | p as u64)
    }

    #[inline(always)]
    fn tag(self) -> u64 {
        self.0 >> TAG_SHIFT
    }

    #[inline(always)]
    pub fn is_float(self) -> bool {
        self.tag() < TAG_PTR || self.tag() > 0xFFFC
    }
    #[inline(always)]
    pub fn is_small_int(self) -> bool {
        self.tag() == TAG_INT
    }
    #[inline(always)]
    pub fn is_ptr(self) -> bool {
        self.tag() == TAG_PTR
    }
    #[inline(always)]
    pub fn is_nil(self) -> bool {
        self.0 == Value::NIL.0
    }
    #[inline(always)]
    pub fn is_bool(self) -> bool {
        self.0 == Value::TRUE.0 || self.0 == Value::FALSE.0
    }
    #[inline(always)]
    pub fn is_true(self) -> bool {
        self.0 == Value::TRUE.0
    }
    #[inline(always)]
    pub fn is_empty_marker(self) -> bool {
        self.0 == Value::EMPTY.0
    }

    #[inline(always)]
    pub fn as_f64(self) -> f64 {
        f64::from_bits(self.0)
    }
    #[inline(always)]
    pub fn as_small_int(self) -> i64 {
        ((self.0 << 16) as i64) >> 16
    }
    #[inline(always)]
    pub fn as_ptr(self) -> *mut Header {
        (self.0 & PAYLOAD_MASK) as *mut Header
    }
    /// Both operands are small integers (one check for binary fast paths).
    #[inline(always)]
    pub fn both_small_ints(a: Value, b: Value) -> bool {
        (a.tag() == TAG_INT) & (b.tag() == TAG_INT)
    }

    /// The heap header if this is a pointer.
    #[inline(always)]
    pub fn header(self) -> Option<&'static Header> {
        if self.is_ptr() {
            // SAFETY: pointer values always refer to live heap objects while reachable.
            Some(unsafe { &*self.as_ptr() })
        } else {
            None
        }
    }
}

#[inline(always)]
pub fn fits_small(i: i64) -> bool {
    ((i << 16) >> 16) == i
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_float() {
            write!(f, "{}f", self.as_f64())
        } else if self.is_small_int() {
            write!(f, "{}i", self.as_small_int())
        } else if self.is_nil() {
            write!(f, "nil")
        } else if self.is_bool() {
            write!(f, "{}", self.is_true())
        } else if self.is_ptr() {
            let cls = crate::class::class_of(*self);
            write!(f, "<{} @{:p}>", cls.name, self.as_ptr())
        } else {
            write!(f, "<special {:x}>", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_round_trip() {
        for i in [0i64, 1, -1, SMALL_INT_MAX, SMALL_INT_MIN, 123456789] {
            let v = Value::small_int(i);
            assert!(v.is_small_int() && !v.is_float() && !v.is_ptr());
            assert_eq!(v.as_small_int(), i);
        }
        for f in [0.0, -0.0, 1.5, f64::INFINITY, f64::NEG_INFINITY, f64::MIN_POSITIVE, -1e300] {
            let v = Value::from_f64(f);
            assert!(v.is_float());
            assert_eq!(v.as_f64().to_bits(), f.to_bits());
        }
        let nan = Value::from_f64(f64::NAN);
        assert!(nan.is_float() && nan.as_f64().is_nan());
        let neg_nan = Value::from_f64(-f64::NAN);
        assert!(neg_nan.is_float());
        assert!(!Value::NIL.is_float() && Value::NIL.is_nil());
        assert!(Value::TRUE.is_bool() && Value::TRUE.is_true());
        assert!(!fits_small(SMALL_INT_MAX + 1));
        assert!(!fits_small(SMALL_INT_MIN - 1));
    }
}
