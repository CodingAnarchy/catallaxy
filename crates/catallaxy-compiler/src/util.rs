//! Small helpers.

/// Levenshtein distance (for "did you mean" suggestions).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest candidate within distance 2, if any.
pub fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        if c == name {
            continue;
        }
        let d = edit_distance(name, c);
        let limit = if name.len() <= 3 { 1 } else { 2 };
        if d <= limit && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, s)| s.to_string())
}

/// The native stack the compiler runs on. The parser, checker and code generator recurse over
/// the syntax tree, whose depth [`crate::parser::MAX_NESTING`] and [`crate::parser::MAX_DEPTH`]
/// bound; this leaves room for that at the deepest even in a debug build. Like the main VM
/// thread's stack, it is reserved address space, touched only as deep as it is used.
pub const COMPILER_STACK: usize = 256 << 20;

/// Runs `f` on a thread with [`COMPILER_STACK`] of native stack and returns its result. A panic
/// in `f` continues in the caller.
pub fn on_compiler_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        let h = std::thread::Builder::new()
            .name("catx-compile".into())
            .stack_size(COMPILER_STACK)
            .spawn_scoped(s, f)
            .expect("spawn the compiler thread");
        h.join().unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}
