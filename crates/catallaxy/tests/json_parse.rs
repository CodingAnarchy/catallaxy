//! `Json.parse` reads text in one pass with its own reader (#177) and must accept exactly what
//! serde_json accepts, with serde_json's error messages. This checks both on a few thousand
//! texts: corner cases, and valid documents mutated a byte at a time.

use std::process::Command;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

/// Separates the texts in the corpus file; no text contains it.
const SEP: char = '\u{1e}';

const PROGRAM: &str = r#"text = File.read(args()[0]).unwrap()
for t in text.split("\u{1e}") {
  match Json.parse(t) {
    Ok(v) => puts("ok #{Json.stringify(v)}")
    Err(e) => puts("err #{e}")
  }
}
"#;

const VALID: [&str; 6] = [
    r#"{"a": [1, -2.5e3, true, false, null], "b": {"c": "d\n\u00e9\ud83d\ude00"}, "e": []}"#,
    r#"[0, -0, 1e2, 1E-2, 0.5, 123456789012345678901234567890, -9223372036854775809, {}]"#,
    "  {\"k\":\"v\" , \"k\" : 2}\t\r\n",
    r#"["\"\\\/\b\f\n\r\t", "x\u0041", "", "é😀"]"#,
    r#"{"z": 1, "a": {"y": [[], [[]], {"x": null}]}}"#,
    "\"top\"",
];

fn corner_cases() -> Vec<String> {
    let mut v: Vec<String> = [
        "",
        " ",
        "nul",
        "nulll",
        "tru",
        "falsey",
        "[",
        "]",
        "[1,]",
        "[,1]",
        "[1 2]",
        "{",
        "}",
        "{,}",
        "{\"a\"}",
        "{\"a\":}",
        "{\"a\" 1}",
        "{\"a\":1,}",
        "{1:2}",
        "{\"a\":1 \"b\":2}",
        "01",
        "-",
        "-01",
        "1.",
        ".5",
        "1e",
        "1e+",
        "1.e3",
        "+1",
        "0x10",
        "1e400",
        "-1e400",
        "1e308",
        "1.7976931348623157e308",
        "1.7976931348623158e308",
        "1.7976931348623159e308",
        "2e308",
        "1e-400",
        "\"abc",
        "\"\\x\"",
        "\"\\u12\"",
        "\"\\u12G4\"",
        "\"\\ud800\"",
        "\"\\udc00\"",
        "\"\\ud800\\u0041\"",
        "\"\\ud800\\udbff\"",
        "\"\\ud83d\\ude00\"",
        "\"a\u{1}b\"",
        "\"tab\there\"",
        "\"\u{7f}\"",
        "1 2",
        "[] []",
        "{}x",
        "\u{feff}1",
        "[1]\u{0}",
        "\u{a0}1",
        "NaN",
        "Infinity",
        "-Infinity",
        "[true, false, null]",
        "\"\\/\"",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // An integer too long for a float, and serde_json's nesting limit.
    v.push("9".repeat(400));
    v.push(format!("-{}", "9".repeat(309)));
    v.push("1".repeat(308));
    for depth in [126, 127, 128, 129] {
        v.push(format!("{}{}", "[".repeat(depth), "]".repeat(depth)));
        v.push(format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth)));
        v.push(format!("{}{}", "[".repeat(depth), "]".repeat(depth - 1)));
    }
    v
}

/// Valid documents with one byte deleted, replaced or inserted, or cut short.
fn mutations() -> Vec<String> {
    const BYTES: &[u8] = b"[]{}\",:\\ 0123456789.eE+-tfnulax\n\t\x01";
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut out = Vec::new();
    for doc in VALID {
        let b = doc.as_bytes();
        for _ in 0..400 {
            let mut m = b.to_vec();
            let i = (next() as usize) % m.len();
            let c = BYTES[(next() as usize) % BYTES.len()];
            match next() % 4 {
                0 => {
                    m.remove(i);
                }
                1 => m[i] = c,
                2 => m.insert(i, c),
                _ => m.truncate(i),
            }
            if let Ok(s) = String::from_utf8(m) {
                out.push(s);
            }
        }
    }
    out
}

#[test]
fn json_parse_matches_serde_json() {
    let mut cases: Vec<String> = VALID.iter().map(|s| s.to_string()).collect();
    cases.extend(corner_cases());
    cases.extend(mutations());
    assert!(cases.iter().all(|c| !c.contains(SEP)));
    let dir = std::env::temp_dir().join(format!("catx-json-parse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (prog, corpus) = (dir.join("p.cx"), dir.join("corpus.txt"));
    std::fs::write(&prog, PROGRAM).unwrap();
    std::fs::write(&corpus, cases.join(&SEP.to_string())).unwrap();
    let out = Command::new(CATX).arg("run").arg(&prog).arg(&corpus).output().expect("run catx");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), cases.len());
    let mut accepted = 0;
    for (case, got) in cases.iter().zip(&lines) {
        match serde_json::from_str::<serde_json::Value>(case) {
            Err(e) => assert_eq!(*got, format!("err invalid JSON: {e}"), "text {case:?}"),
            Ok(v) => {
                assert!(got.starts_with("ok "), "text {case:?}: {got}");
                accepted += 1;
                // Where serde_json's tree holds the same values (no floats, no integers past
                // 64 bits), the output is serde_json's (keys sorted, as its map keeps them).
                let plain =
                    !case.contains(['.', 'e', 'E']) && !case.bytes().filter(u8::is_ascii_digit).nth(18).is_some();
                if plain {
                    assert_eq!(&got[3..], serde_json::to_string(&v).unwrap(), "text {case:?}");
                }
            }
        }
    }
    assert!(accepted > 50 && accepted < cases.len() - 500, "{accepted} of {}", cases.len());
}
