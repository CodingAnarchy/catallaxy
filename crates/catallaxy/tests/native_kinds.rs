//! Every native checks the kind of its receiver and arguments (#147).
//!
//! The list of natives is generated from the prelude's `native fn` declarations, so a new native
//! is covered without touching this file. For each one, a value of the wrong type reaches it the
//! way a program can make one: pushed through a `Dyn` alias into a typed list, then read back
//! with its static type. Each call runs in its own `catx` process, so a crash fails the test
//! (naming the native) instead of taking the test runner down.
//!
//! - A receiver of the wrong type must raise a `TypeError` naming the receiver's type.
//! - An argument of the wrong type must not crash; for arguments of a concrete type (not `Dyn`,
//!   a type parameter, `Bool` or a function), it must raise a `TypeError` too.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use catallaxy_compiler::driver::PRELUDE;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

#[derive(Debug)]
struct Param {
    name: String,
    /// The declared type with type parameters replaced by `Int`.
    ty: String,
    /// The declared type accepts any value (`Dyn` or a bare type parameter).
    any: bool,
}

#[derive(Debug)]
struct Native {
    /// `Type.name` for methods and static functions, `name` for global functions.
    label: String,
    /// The type the native belongs to (`None` for global functions).
    owner: Option<String>,
    name: String,
    /// The receiver's type (with type parameters replaced by `Int`), for methods.
    recv: Option<String>,
    params: Vec<Param>,
}

/// Splits `s` at top-level occurrences of `sep` (outside brackets and parentheses).
fn split_top(s: &str, sep: char) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quoted) = (Vec::new(), String::new(), 0i32, false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' if !quoted => depth += 1,
            ']' | ')' if !quoted => depth -= 1,
            _ => {}
        }
        if c == sep && depth == 0 && !quoted {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|p| p.trim().to_string()).collect()
}

/// The index of the `)` closing the `(` at `open`.
fn matching_paren(s: &str, open: usize) -> usize {
    let mut depth = 0;
    for (i, c) in s.char_indices().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parentheses in `{s}`")
}

/// The type parameter names in a `[T, K: Comparable]` list.
fn type_params(list: &str) -> Vec<String> {
    split_top(list, ',').iter().map(|p| p.split(':').next().unwrap().trim().to_string()).collect()
}

/// Replaces each whole-word type parameter in `ty` by `Int`.
fn substitute(ty: &str, params: &[String]) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if params.iter().any(|p| p == word) {
            out.push_str("Int");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in ty.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The natives declared in the prelude.
fn natives() -> Vec<Native> {
    let mut out = Vec::new();
    for (_, src) in PRELUDE {
        // The type whose block we are in, and its type parameters.
        let mut owner: Option<(String, String, Vec<String>)> = None;
        for line in src.lines() {
            if line.starts_with('}') {
                owner = None;
                continue;
            }
            let head = line.trim_start_matches("pub ");
            let head = head.trim_start_matches("frozen ").trim_start_matches("shared ");
            if let Some(rest) = head.strip_prefix("class ").or_else(|| head.strip_prefix("extend")) {
                // `extend[T: Comparable] List[T] {`, `extend List[T] {`, `class Deque[T] {`
                let rest = rest.trim_end_matches('{').trim();
                let (mut tparams, ty) = if let Some(r) = rest.strip_prefix('[') {
                    let close = r.find(']').unwrap();
                    (type_params(&r[..close]), r[close + 1..].trim().to_string())
                } else {
                    (Vec::new(), rest.to_string())
                };
                let name = ty.split('[').next().unwrap().trim().to_string();
                if let Some(open) = ty.find('[') {
                    tparams.extend(type_params(&ty[open + 1..ty.len() - 1]));
                }
                owner = Some((name, ty, tparams));
                continue;
            }
            let trimmed = line.trim_start();
            let global = !line.starts_with(' ');
            let Some(decl) = trimmed.trim_start_matches("pub ").strip_prefix("native fn ") else { continue };
            let open = decl.find('(').unwrap();
            let mut name = decl[..open].to_string();
            let mut tparams = Vec::new();
            if let Some(b) = name.find('[').filter(|&b| b > 0) {
                tparams = type_params(&name[b + 1..name.len() - 1]);
                name.truncate(b);
            }
            if name.starts_with("__") {
                continue; // private to the prelude (and checked where it calls them)
            }
            let owner = if global { None } else { owner.clone() };
            if let Some((_, _, ps)) = &owner {
                tparams.extend(ps.iter().cloned());
            }
            let close = matching_paren(decl, open);
            let mut recv = None;
            let mut params = Vec::new();
            for p in split_top(&decl[open + 1..close], ',') {
                let p = p.trim_start_matches("mut ").trim_start_matches("move ");
                if p == "self" {
                    recv = owner.as_ref().map(|(_, ty, _)| substitute(ty, &tparams));
                    continue;
                }
                let (pname, rest) =
                    p.split_once(":").unwrap_or_else(|| panic!("cannot parse parameter `{p}` in `{line}`"));
                let ty = rest.split(" = ").next().unwrap().trim();
                let any = ty == "Dyn" || tparams.iter().any(|t| t == ty);
                params.push(Param { name: pname.trim().to_string(), ty: substitute(ty, &tparams), any });
            }
            let label = match &owner {
                Some((o, _, _)) => format!("{o}.{name}"),
                None => name.clone(),
            };
            out.push(Native { label, owner: owner.map(|o| o.0), name, recv, params });
        }
    }
    out
}

/// A well-typed expression of type `ty`, for arguments that are not under test.
fn valid_value(ty: &str) -> String {
    let ty = ty.trim();
    if ty.ends_with('?') {
        return "nil".into();
    }
    if let Some(first) = split_top(ty, '|').first().filter(|_| ty.contains('|')) {
        return valid_value(first);
    }
    if let Some(sig) = ty.strip_prefix("fn(") {
        let close = sig.find(')').unwrap();
        let n = split_top(&sig[..close], ',').len();
        let ret = sig[close + 1..].trim().strip_prefix("->").map(str::trim);
        let names: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
        let body = ret.map(valid_value).unwrap_or_else(|| "nil".into());
        return format!("{{ |{}| {body} }}", names.join(", "));
    }
    let head = ty.split('[').next().unwrap();
    match head {
        "Int" | "Dyn" => "1".into(),
        "Float" => "1.5".into(),
        "String" => "\"a\"".into(),
        "Bool" => "false".into(),
        "List" => "[]".into(),
        "Dict" => "[:]".into(),
        "Bytes" => "Bytes.new()".into(),
        _ => panic!("no well-typed value for `{ty}`: add one to `valid_value`"),
    }
}

/// Expressions of other types than `ty`: an immediate (`Int`) and two heap kinds. Ints stand in
/// for floats, so they are not wrong for `Float`.
fn wrong_values(ty: &str) -> Vec<&'static str> {
    let head = ty.trim().trim_end_matches('?').split('[').next().unwrap();
    let all = [("Int", "42"), ("String", "\"wrong\""), ("List", "[7]")];
    all.iter()
        .filter(|(n, _)| !(*n == head || head == "Float" && *n == "Int" || ty.split('|').any(|alt| alt.trim() == *n)))
        .map(|(_, v)| *v)
        .collect()
}

/// Program lines making `var` a value `wrong` that has static type `ty`.
fn poison(var: &str, ty: &str, wrong: &str) -> String {
    format!("{var}_l: List[{ty}] = []\n{var}_d: Dyn = {var}_l\n{var}_d.push({wrong})\n{var} = {var}_l[0]\n")
}

struct Case {
    native: String,
    what: String,
    src: String,
    /// Text the panic message must contain (`None`: only that it does not crash).
    expect: Option<String>,
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for n in natives() {
        // Operators compile to generic instructions that dispatch on the values' actual types
        // (`==` of different types is false, `x[i]` indexes whatever `x` is), so a wrong operand
        // need not reach the native: for them, the test only checks that nothing crashes.
        let is_op = !n.name.starts_with(|c: char| c.is_alphabetic() || c == '_');
        let callee = |args: &[String]| -> String {
            match (n.name.as_str(), args) {
                ("[]", [i]) => return format!("recv[{i}]"),
                ("[]=", [i, v]) => return format!("recv[{i}] = {v}"),
                (op, [x]) if is_op => return format!("recv {op} {x}"),
                _ => {}
            }
            let args = args.join(", ");
            match (&n.recv, &n.owner) {
                (Some(_), _) => format!("recv.{}({args})", n.name),
                (None, Some(o)) => format!("{o}.{}({args})", n.name),
                (None, None) => format!("{}({args})", n.name),
            }
        };
        let valid_args: Vec<String> = n.params.iter().map(|p| valid_value(&p.ty)).collect();
        if let Some(rt) = &n.recv {
            let head = rt.split('[').next().unwrap().to_string();
            for wrong in wrong_values(rt) {
                let src = format!("{}{}\n", poison("recv", rt, wrong), callee(&valid_args));
                cases.push(Case {
                    native: n.label.clone(),
                    what: format!("receiver {wrong}"),
                    src,
                    expect: (!is_op)
                        .then(|| format!("TypeError: expected {}", if head == "Set" { "Dict or Set" } else { &head })),
                });
            }
        }
        let recv_setup = match &n.recv {
            Some(rt) => match recv_value(rt) {
                Some(v) => format!("recv: {rt} = {v}\n"),
                None => continue,
            },
            None => String::new(),
        };
        for (i, p) in n.params.iter().enumerate() {
            if p.any {
                continue;
            }
            let strict = !(is_op || p.ty == "Bool" || p.ty.starts_with("fn(") || p.ty.contains("Dyn"));
            for wrong in wrong_values(&p.ty) {
                let mut args = valid_args.clone();
                args[i] = "arg".into();
                let src = format!("{recv_setup}{}{}\n", poison("arg", &p.ty, wrong), callee(&args));
                cases.push(Case {
                    native: n.label.clone(),
                    what: format!("argument `{}` {wrong}", p.name),
                    src,
                    expect: strict.then(|| "TypeError".to_string()),
                });
            }
        }
    }
    cases
}

/// A well-typed receiver of type `ty` (`None` for system resources: their arguments are not
/// tested, as their natives would start processes or open sockets).
fn recv_value(ty: &str) -> Option<String> {
    let head = ty.split('[').next().unwrap();
    Some(match head {
        "List" => "[1, 2]".into(),
        "Dict" => "[1: 2]".into(),
        "Set" => "Set.from([1])".into(),
        "Range" => "1..2".into(),
        "StringBuilder" => "StringBuilder.new()".into(),
        "Mutex" => "Mutex.new(1)".into(),
        "Atomic" => "Atomic.new(1)".into(),
        "Channel" => "Channel.new(4)".into(),
        "Thread" => "spawn { 1 }".into(),
        "Regex" => "Regex.new(\"a\").unwrap()".into(),
        "Subprocess" | "Socket" | "Listener" => return None,
        _ => valid_value(ty),
    })
}

/// Runs a program in a child process: (exit status, stderr). `None` for the code if the process
/// was killed by a signal (a crash).
fn run(dir: &std::path::Path, idx: usize, src: &str) -> (Option<i32>, String) {
    let path = dir.join(format!("case{idx}.cx"));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(CATX)
        .arg("run")
        .arg(&path)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run catx");
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn natives_reject_values_of_the_wrong_type() {
    let natives = natives();
    assert!(natives.len() > 200, "found only {} natives in the prelude", natives.len());
    let cases = cases();
    let dir = std::env::temp_dir().join(format!("catx-native-kinds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(c) = cases.get(i) else { break };
                let (code, err) = run(&dir, i, &c.src);
                let panic_line = err.lines().find(|l| l.starts_with("panic: ")).unwrap_or("");
                let problem = match code {
                    None => Some("crashed (killed by a signal)".to_string()),
                    Some(0) if c.expect.is_none() => None,
                    Some(101) => match &c.expect {
                        Some(want) if !panic_line.contains(want.as_str()) => {
                            Some(format!("panicked without `{want}`: {panic_line}"))
                        }
                        _ => None,
                    },
                    Some(code) => Some(format!("exit code {code}: {}", err.trim())),
                };
                if let Some(p) = problem {
                    failures.lock().unwrap().push(format!("{} ({}): {p}\n--- program\n{}", c.native, c.what, c.src));
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{} of {} cases failed:\n\n{}", failures.len(), cases.len(), failures.join("\n"));
}
