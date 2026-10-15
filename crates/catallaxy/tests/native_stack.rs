//! Deep native recursion raises `SystemStackError` instead of overflowing the native stack
//! (issue #154): structural `==`, hashing and `<=>` on deeply nested values, and recursion
//! through natives that call back into the VM (`Mutex#lock`, `Atomic#update`, `par_map`), on
//! the main thread, a spawned thread, a fiber and a pool worker. Each program runs in a child
//! process, so an overflow fails the test instead of killing the test runner.

use std::process::Command;

const CATX: &str = env!("CARGO_BIN_EXE_catx");

const PRELUDE: &str = r##"
fn mk(k: String, n: Int) -> Dyn {
  x: Dyn = if k == "list" { [1] } else if k == "dict" { ["k": 1] } else { (1, 2) }
  for _i in 0...n {
    x = if k == "list" { [x] } else if k == "dict" { ["k": x] } else { (x, 2) }
  }
  x
}
fn op(o: String, k: String, n: Int) -> Dyn {
  a = mk(k, n)
  if o == "==" {
    a == mk(k, n)
  } else if o == "hash" {
    d: Dict[Dyn, Int] = [:]
    d[a] = 1
    d.length
  } else {
    a <=> mk(k, n)
  }
}
fn via_lock(m: Mutex[Int], n: Int) -> Int { m.lock { |v| if n == 0 { v } else { via_lock(m, n - 1) } } }
fn via_update(a: Atomic[Int], n: Int) -> Int {
  a.update { |v| if n == 0 { v } else { via_update(a, n - 1); v } }
}
# Each level goes through `par_map` and then 30 `Mutex#lock` calls (on a mutex of its own: the
# next level may run on another thread), so the stack runs out after a few hundred parallel jobs
# (a failing job's trace grows with the depth).
fn via_par_map(n: Int) -> Int { [n].par_map { |x| lock_then_par_map(Mutex.new(x), 30) }[0] }
fn lock_then_par_map(lm: Mutex[Int], k: Int) -> Int {
  lm.lock { |v| if k == 0 { via_par_map(v + 1) } else { lock_then_par_map(lm, k - 1) } }
}
fn report[T](what: String, r: Result[T, String]) {
  match r {
    Ok(_) => puts("#{what}: no error")
    Err(e) => puts("#{what}: #{e.lines()[0]}")
  }
}
m = Mutex.new(0)
at = Atomic.new(0)
"##;

/// The structural operations and the kinds of value each is checked on (`<=>` does not order
/// dicts, so it does not recurse into them).
const OPS: [(&str, &[&str]); 3] =
    [("==", &["list", "dict", "tuple"]), ("hash", &["list", "dict", "tuple"]), ("<=>", &["list", "tuple"])];

/// Runs `body` after the prelude; returns (exit code, stdout, stderr). A child killed by a
/// signal (a stack overflow) has no exit code and reads as -1.
fn run(tag: &str, body: &str) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!("catx-native-stack-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.cx");
    std::fs::write(&path, format!("{PRELUDE}{body}")).unwrap();
    let out = Command::new(CATX).arg("run").arg(&path).env("CATX_THREADS", "4").output().expect("run catx");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs `body`, which reports each case with `report`, and checks that every case raised
/// `SystemStackError` and that the program went on to the end.
fn expect_caught(tag: &str, body: &str, cases: &[String]) {
    let (code, out, err) = run(tag, &format!("{body}puts(\"done\")\n"));
    let mut want: String = cases.iter().map(|c| format!("{c}: SystemStackError: stack level too deep\n")).collect();
    want.push_str("done\n");
    assert_eq!((code, out.as_str()), (0, want.as_str()), "stderr:\n{err}");
}

/// Builds the cases for one context: `wrap` turns an expression into one that runs it there
/// and returns a `Result` (a fiber's or a thread's `join`).
fn context_cases(name: &str, depth: u32, wrap: impl Fn(&str) -> String) -> (String, Vec<String>) {
    let mut body = String::new();
    let mut cases = Vec::new();
    for (o, kinds) in OPS {
        for k in kinds {
            let case = format!("{name} {o} {k}");
            body.push_str(&format!("report(\"{case}\", {})\n", wrap(&format!("op(\"{o}\", \"{k}\", {depth})"))));
            cases.push(case);
        }
    }
    for (what, call) in
        [("lock", "via_lock(m, 10_000_000)"), ("update", "via_update(at, 10_000_000)"), ("par_map", "via_par_map(0)")]
    {
        let case = format!("{name} {what}");
        body.push_str(&format!("report(\"{case}\", {})\n", wrap(call)));
        cases.push(case);
    }
    (body, cases)
}

#[test]
fn fiber_overflow_is_a_panic_its_join_returns() {
    // A fiber's native stack is 256 KB.
    let (body, cases) = context_cases("fiber", 20_000, |e| format!("Fiber.start {{ {e} }}.join()"));
    expect_caught("fiber", &body, &cases);
}

#[test]
fn thread_overflow_is_a_panic_its_join_returns() {
    // A thread's native stack is 16 MB.
    let (body, cases) = context_cases("thread", 400_000, |e| format!("spawn {{ {e} }}.join()"));
    expect_caught("thread", &body, &cases);
}

#[test]
fn pool_worker_overflow_is_a_panic_par_map_raises() {
    // A fiber leaves a parallel job to the pool's workers (16 MB stacks), so every element runs
    // on a worker; the panic reaches the fiber through `par_map`.
    let (body, cases) = context_cases("pool", 400_000, |e| {
        format!("Fiber.start {{ (0...2).to_list().par_map {{ |_i| {e} }}.length }}.join()")
    });
    expect_caught("pool", &body, &cases);
}

#[test]
fn main_thread_overflow_is_a_traced_panic() {
    // The main thread's native stack is 256 MB.
    let mut bodies: Vec<String> = Vec::new();
    for (o, kinds) in OPS {
        // One kind per operation: building millions of nested dicts is slow.
        let k = if o == "hash" { kinds[2] } else { kinds[0] };
        bodies.push(format!("puts(op(\"{o}\", \"{k}\", 3_000_000))\n"));
    }
    bodies.push("puts(via_lock(m, 100_000_000))\n".into());
    bodies.push("puts(via_update(at, 100_000_000))\n".into());
    bodies.push("puts(via_par_map(0))\n".into());
    for (i, body) in bodies.iter().enumerate() {
        let (code, out, err) = run(&format!("main{i}"), &format!("{body}puts(\"unreachable\")\n"));
        assert_eq!(code, 101, "{body}stdout:\n{out}\nstderr:\n{err}");
        assert!(err.starts_with("panic: SystemStackError: stack level too deep\n    at "), "{body}{err}");
    }
}
