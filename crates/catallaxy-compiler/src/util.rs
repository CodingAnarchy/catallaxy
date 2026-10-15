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
///
/// Each calling thread hands its work to a compiler thread of its own, started on first use and
/// kept until the caller exits: a new thread per compilation would cost more than checking a
/// small file (its allocator caches start cold). Calls from a compiler thread run in place.
pub fn on_compiler_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    use std::cell::{Cell, OnceCell};
    use std::sync::mpsc;

    type Job = Box<dyn FnOnce() + Send + 'static>;
    thread_local! {
        static ON_COMPILER_STACK: Cell<bool> = const { Cell::new(false) };
        static WORKER: OnceCell<Option<mpsc::Sender<Job>>> = const { OnceCell::new() };
    }
    if ON_COMPILER_STACK.with(Cell::get) {
        return f();
    }
    let worker = WORKER.with(|w| {
        w.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            let spawned =
                std::thread::Builder::new().name("catx-compile".into()).stack_size(COMPILER_STACK).spawn(move || {
                    ON_COMPILER_STACK.with(|c| c.set(true));
                    // Ends when the calling thread exits and drops the sender.
                    for job in rx {
                        job();
                    }
                });
            spawned.ok().map(|_| tx)
        })
        .clone()
    });
    let Some(worker) = worker else {
        // No thread could be started: compile here, on whatever stack there is.
        return f();
    };
    let mut result: Option<std::thread::Result<R>> = None;
    {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let slot = &mut result;
        let job: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
            *slot = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
            let _ = done_tx.send(());
        });
        // SAFETY: the job borrows from this frame. It is either sent back unrun (dropped here), or
        // this frame waits until it has run (it catches panics, so it always reports back) or has
        // been dropped unrun with the compiler thread (which drops `done_tx` and ends the wait).
        // Either way nothing of it is used after this block.
        let job: Job = unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Job>(job) };
        if worker.send(job).is_ok() {
            let _ = done_rx.recv();
        }
    }
    match result {
        Some(Ok(r)) => r,
        Some(Err(payload)) => std::panic::resume_unwind(payload),
        None => panic!("the compiler thread is gone"),
    }
}
