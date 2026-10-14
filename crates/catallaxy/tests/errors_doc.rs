//! `docs/errors.md` has an entry for every diagnostic code the compiler can emit, and each entry's
//! example program triggers its code under `catx test` (the conformance `reject` machinery).
//!
//! A code is "emitted" if a string literal such as `"E0342"` appears in the compiler's sources.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

/// Codes whose entry has no example, and why. Every other entry needs one.
const NO_EXAMPLE: &[(&str, &str)] = &[
    ("E0801", "reports a bug in `catx fmt`; no correct formatter produces it"),
    ("E0900", "needs a function with more than 250 live values, too long to show"),
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Each code the compiler sources mention, with the first place it appears.
fn emitted_codes() -> BTreeMap<String, String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = repo();
    let mut files = Vec::new();
    walk(&root.join("crates/catallaxy-compiler/src"), &mut files);
    files.sort();
    let mut codes = BTreeMap::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = f.strip_prefix(&root).unwrap_or(&f).display().to_string().replace('\\', "/");
        for (i, line) in text.lines().enumerate() {
            for code in codes_in(line) {
                codes.entry(code).or_insert_with(|| format!("{rel}:{}", i + 1));
            }
        }
    }
    codes
}

/// The `"E1234"` / `"W1234"` string literals in `line`.
fn codes_in(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    for i in 0..b.len().saturating_sub(6) {
        if b[i] == b'"'
            && matches!(b[i + 1], b'E' | b'W')
            && b[i + 2..i + 6].iter().all(u8::is_ascii_digit)
            && b[i + 6] == b'"'
        {
            out.push(line[i + 1..i + 6].to_string());
        }
    }
    out
}

/// One `## CODE` section of the index.
struct Entry {
    code: String,
    line: usize,
    /// The ```` ```cx ```` block: the program that should trigger the code.
    example: Option<String>,
    /// ```` ```cx lib/name.cx ```` blocks: modules the example imports, by relative path.
    modules: Vec<(String, String)>,
}

fn parse_index(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        if let Some(code) = line.strip_prefix("## ") {
            entries.push(Entry { code: code.trim().to_string(), line: i + 1, example: None, modules: Vec::new() });
        } else if let Some(info) = line.strip_prefix("```cx") {
            let mut body = String::new();
            for (_, l) in lines.by_ref() {
                if l.starts_with("```") {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            let entry = entries.last_mut().expect("a ```cx block before the first `## CODE` heading");
            match info.trim() {
                "" => {
                    assert!(entry.example.is_none(), "the {} entry has two ```cx examples", entry.code);
                    entry.example = Some(body);
                }
                path => entry.modules.push((path.to_string(), body)),
            }
        }
    }
    entries
}

fn template(code: &str, at: &str, place: &str) -> String {
    format!(
        "docs/errors.md has no entry for {code} (emitted at {at}). Add this section {place}:\n\n\
         ## {code}\n\n\
         <Short name>. <What the error means, in a sentence or two.>\n\n\
         ```cx\n\
         <a minimal program that makes `catx check` report {code}>\n\
         ```\n\n\
         <How to fix it.>\n\n\
         If the example imports a module, add it as a ```cx lib/name.cx block after the example.\n"
    )
}

#[test]
fn every_error_code_has_an_entry_whose_example_triggers_it() {
    let root = repo();
    let text = std::fs::read_to_string(root.join("docs/errors.md")).unwrap();
    let entries = parse_index(&text);
    let emitted = emitted_codes();
    assert!(emitted.len() > 100, "found only {} codes in the compiler sources", emitted.len());
    let documented: Vec<&str> = entries.iter().map(|e| e.code.as_str()).collect();

    let mut problems = Vec::new();
    for (code, at) in &emitted {
        if documented.contains(&code.as_str()) {
            continue;
        }
        let before = documented.iter().rev().find(|c| **c < code.as_str());
        let after = documented.iter().find(|c| **c > code.as_str());
        let place = match (before, after) {
            (Some(b), Some(a)) => format!("between `## {b}` and `## {a}`"),
            (Some(b), None) => format!("after `## {b}`"),
            (None, Some(a)) => format!("before `## {a}`"),
            (None, None) => "to the end of the file".to_string(),
        };
        problems.push(template(code, at, &place));
    }
    for e in &entries {
        if !emitted.contains_key(&e.code) {
            problems.push(format!(
                "docs/errors.md:{}: `## {}` is not a code the compiler emits (no \"{}\" literal in \
                 crates/catallaxy-compiler/src); remove the entry or fix the heading",
                e.line, e.code, e.code
            ));
        }
    }
    for w in documented.windows(2) {
        if w[0] >= w[1] {
            problems.push(format!(
                "docs/errors.md: `## {}` must come after `## {}` (entries are sorted by code)",
                w[0], w[1]
            ));
        }
    }

    // Write each example as a conformance `reject` test in its own directory and run `catx test`.
    let dir = std::env::temp_dir().join(format!("catx-errors-doc-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for e in &entries {
        let exempt = NO_EXAMPLE.iter().any(|(c, _)| *c == e.code);
        match (&e.example, exempt) {
            (Some(_), true) => problems.push(format!(
                "docs/errors.md:{}: the {} entry has an example; remove {} from NO_EXAMPLE in {}",
                e.line,
                e.code,
                e.code,
                file!()
            )),
            (None, false) => problems.push(format!(
                "docs/errors.md:{}: the {} entry needs a ```cx block with a program that triggers {}",
                e.line, e.code, e.code
            )),
            (None, true) => {}
            (Some(src), false) => {
                let d = dir.join(&e.code);
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join("example.cx"), format!("# expect: {}\n{src}", e.code)).unwrap();
                for (path, body) in &e.modules {
                    let p = d.join(path);
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(p, body).unwrap();
                }
            }
        }
    }
    let out = Command::new(CATX).arg("test").arg(&dir).output().expect("run catx test");
    let stdout = String::from_utf8_lossy(&out.stdout).replace('\\', "/");
    for block in stdout.split("FAIL ").skip(1) {
        let code = block.split("/example.cx").next().unwrap_or("").rsplit('/').next().unwrap_or("?");
        let line = entries.iter().find(|e| e.code == code).map_or(0, |e| e.line);
        let got: Vec<&str> = block.lines().skip(1).filter(|l| !l.contains(" passed, ")).collect();
        let got = got.join("\n");
        let got = got.trim();
        problems.push(format!(
            "docs/errors.md:{line}: the {code} example does not make `catx check` report {code}:\n{got}"
        ));
    }
    if problems.is_empty() {
        assert!(out.status.success(), "catx test failed:\n{stdout}{}", String::from_utf8_lossy(&out.stderr));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n\n"));
}
