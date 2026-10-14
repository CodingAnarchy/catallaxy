//! String character positions. Kept in its own test binary: under Miri every reference made from
//! a `Value` (an integer address) leaves a borrow tag that is never collected, so heap segments
//! reused after other tests carry thousands of tags and each access to them gets slower. Here the
//! test starts with a fresh heap.

mod common;

use catallaxy_runtime::object::*;
use common::*;

#[test]
fn string_char_positions_use_the_index() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Host::new();
    let texts = [
        "a".repeat(300),
        "aé€😀".repeat(90),
        "日本語".repeat(100) + "tail",
        "héllo".to_string(),
        String::new(),
        "x".repeat(StrObj::INDEX_MIN - 1) + "é",
    ];
    for text in &texts {
        let v = h.m.alloc_str(text);
        let hv = h.m.root(v);
        // Strings with an index carry it through a collection.
        h.gc();
        let v = h.m.get(hv);
        let o = unsafe { as_str_obj(v) };
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        assert_eq!(o.char_count(), chars.len());
        assert_eq!(o.is_ascii(), text.is_ascii());
        let step = if cfg!(miri) { 7 } else { 1 };
        for (i, &(b, c)) in chars.iter().enumerate().step_by(step) {
            assert_eq!(o.byte_offset(i), b);
            assert_eq!(o.char_offset(b), i);
            assert_eq!(o.char_at(i), c.to_string());
        }
        assert_eq!(o.byte_offset(chars.len()), text.len());
        assert_eq!(o.char_offset(text.len()), chars.len());
        let (a, z) = (chars.len() / 3, chars.len() * 2 / 3 + 1);
        let want: String = text.chars().skip(a).take(z.min(chars.len()).saturating_sub(a)).collect();
        assert_eq!(o.char_slice(a, z), want);
        assert_eq!(o.char_slice(z, a), "");
        assert!(o.alloc_size() >= StrObj::BASE + text.len());
    }
    // Concatenated parts with a known character count.
    let v = h.m.alloc_str_parts(&[&"é".repeat(200), &"z".repeat(100)]);
    let o = unsafe { as_str_obj(v) };
    assert_eq!(o.char_count(), 300);
    assert_eq!(o.char_at(250), "z");
    assert_eq!(o.char_at(199), "é");
    h.finish();
}
