//! Internal errors: a Rust panic inside the runtime (here, a native extension that panics) on a
//! spawned thread, a pool worker, a fiber or the main thread. The thread must not end silently:
//! `join` returns `Err("internal error: ...")`, the object locks it held are released, it leaves
//! the collector (so later collections do not wait for it), and the program carries on. On the
//! main thread it ends the program with the message, a trace and status 101.
//!
//! Each program runs in a child process (this test binary, re-run as `child`) with a timeout:
//! the failure being tested is a hang.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use catallaxy::{Engine, RtResult, Value, Vm};

const CATX: &str = env!("CARGO_BIN_EXE_catx");

fn boom(_: &mut Vm, _: &[Value]) -> RtResult<Value> {
    panic!("boom from a native");
}

/// The child side: runs the program in `CATX_INTERNAL_PANIC_SRC` with `boom` registered.
#[test]
fn child() {
    let Ok(src) = std::env::var("CATX_INTERNAL_PANIC_SRC") else { return };
    let mut engine = Engine::new();
    engine.register("boom", boom);
    let code = engine.run_source("internal_panic.cx", &src).expect("compiles");
    std::process::exit(code);
}

struct Outcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn ctx(&self) -> String {
        format!("status {:?}\n--- stdout\n{}--- stderr\n{}", self.code, self.stdout, self.stderr)
    }
}

fn wait(mut cmd: Command) -> Outcome {
    let dir = std::env::temp_dir().join(format!("catx-internal-panic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (out_path, err_path) = (dir.join(format!("{n}.out")), dir.join(format!("{n}.err")));
    cmd.stdout(Stdio::from(std::fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()));
    let mut child = cmd.spawn().expect("start the child");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let o = Outcome {
        code: status.and_then(|s| s.code()),
        stdout: std::fs::read_to_string(&out_path).unwrap_or_default(),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    };
    assert!(status.is_some(), "timed out (hung)\n{}", o.ctx());
    o
}

/// Runs `src` with the panicking native, in a child process.
fn run(src: &str) -> Outcome {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["child", "--exact", "--nocapture", "--test-threads=1"])
        .env("CATX_INTERNAL_PANIC_SRC", src)
        .env("CATX_THREADS", "4");
    wait(cmd)
}

/// Runs `src` with `catx run`, in a child process.
fn run_catx(name: &str, src: &str) -> Outcome {
    let dir = std::env::temp_dir().join(format!("catx-internal-panic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    let mut cmd = Command::new(CATX);
    cmd.arg("run").arg(&path);
    wait(cmd)
}

const PRELUDE: &str = r#"native fn boom() -> Int
shared class Cell {
  n: Int = 0
}
c = Cell.new()
"#;

/// A thread that panics inside the runtime while holding an object lock: `join` returns the
/// error, the lock is free again, and collections and new threads still work.
#[test]
fn thread_join() {
    let src = format!(
        r#"{PRELUDE}
t = spawn {{
  with lock(c) {{
    c.n = 1
    boom()
  }}
}}
match t.join() {{
  Ok(_) => puts("ok")
  Err(e) => puts("join: #{{e}}")
}}
gc()
with lock(c) {{ c.n += 1 }}
puts("n: #{{c.n}}")
puts("after: #{{(spawn {{ 6 * 7 }}).join().unwrap()}}")
"#
    );
    let o = run(&src);
    assert_eq!(o.code, Some(0), "{}", o.ctx());
    assert!(o.stdout.contains("join: internal error: boom from a native\n"), "{}", o.ctx());
    assert!(o.stdout.contains("n: 2\nafter: 42\n"), "{}", o.ctx());
}

/// Nobody joins the thread; the main thread collects after it has failed (the collector must not
/// wait for it), and the failure is reported at exit.
#[test]
fn thread_gc() {
    let src = format!(
        r#"{PRELUDE}
started: Channel[Int] = Channel.new()
t = spawn {{
  with lock(c) {{
    started.send(1)
    boom()
  }}
}}
started.recv()
while !t.is_done() {{ sleep(0.01) }}
for i in 0...3 {{ gc() }}
with lock(c) {{ c.n += 1 }}
puts("n: #{{c.n}}")
"#
    );
    let o = run(&src);
    assert_eq!(o.code, Some(0), "{}", o.ctx());
    assert!(o.stdout.contains("n: 1\n"), "{}", o.ctx());
    assert!(o.stderr.contains("panicked and was never joined: internal error: boom from a native"), "{}", o.ctx());
}

/// Pool workers (and the calling thread, which helps) that panic inside the runtime fail the
/// job, and the pool keeps working.
#[test]
fn par_map() {
    let src = r#"native fn boom() -> Int
for round in 0...5 {
  t = spawn { (0...200).to_list().par_map { |x| if x % 7 == 3 { boom() } else { x } } }
  match t.join() {
    Ok(_) => puts("ok")
    Err(e) => puts("join: #{e}")
  }
  gc()
}
puts("sum: #{(0...200).to_list().par_map { |x| x * 2 }.sum()}")
"#;
    let o = run(src);
    assert_eq!(o.code, Some(0), "{}", o.ctx());
    assert_eq!(o.stdout.matches("join: internal error: boom from a native").count(), 5, "{}", o.ctx());
    assert!(o.stdout.contains("sum: 39800\n"), "{}", o.ctx());
}

/// A fiber that panics inside the runtime ends like a fiber that panics.
#[test]
fn fiber() {
    let src = format!(
        r#"{PRELUDE}
f = Fiber.start {{ with lock(c) {{ boom() }} }}
match f.join() {{
  Ok(_) => puts("ok")
  Err(e) => puts("join: #{{e}}")
}}
with lock(c) {{ c.n += 1 }}
gc()
puts("after: #{{Fiber.start {{ 6 * 7 }}.join().unwrap()}}")
"#
    );
    let o = run(&src);
    assert_eq!(o.code, Some(0), "{}", o.ctx());
    assert!(o.stdout.contains("join: internal error: boom from a native"), "{}", o.ctx());
    assert!(o.stdout.contains("after: 42\n"), "{}", o.ctx());
}

/// On the main thread, the program ends with its output flushed, the message and a trace.
#[test]
fn main_thread() {
    let src = format!(
        r#"{PRELUDE}
fn explode() -> Int {{
  boom() + 1
}}
puts("before")
explode()
puts("unreachable")
"#
    );
    let o = run(&src);
    assert_eq!(o.code, Some(101), "{}", o.ctx());
    assert!(o.stdout.contains("before\n"), "{}", o.ctx());
    assert!(!o.stdout.contains("unreachable"), "{}", o.ctx());
    assert!(o.stderr.contains("panic: internal error: boom from a native\n"), "{}", o.ctx());
    assert!(o.stderr.contains("    at explode ("), "{}", o.ctx());
}

/// The reproduction from #155: `with lock` nested deeper than the lock's recursion count used to
/// trip an assertion and wedge `join` and the collector. It is now a `LockError` panic.
const DEEP: &str = r#"shared class B {
  n: Int = 0
}
fn deep(b: B, k: Int) -> Int {
  if k == 0 {
    return 0
  }
  with lock(b) {
    b.n += 1
    deep(b, k - 1)
  }
  k
}
b = B.new()
"#;

const LOCK_ERROR: &str = "LockError: `with lock` nested more than 1000 deep on one object";

#[test]
fn deep_lock_join() {
    let src = format!(
        r#"{DEEP}
puts("ok: #{{deep(b, 1000)}}")
match (spawn {{ deep(b, 1100) }}).join() {{
  Ok(n) => puts("ok #{{n}}")
  Err(e) => puts("join: #{{e}}")
}}
with lock(b) {{ puts("relocked") }}
"#
    );
    let o = run_catx("deep_lock_join.cx", &src);
    assert_eq!(o.code, Some(0), "{}", o.ctx());
    assert_eq!(o.stdout, format!("ok: 1000\njoin: {LOCK_ERROR}\nrelocked\n"), "{}", o.ctx());
}

#[test]
fn deep_lock_gc() {
    let src = format!(
        r#"{DEEP}
t = spawn {{ deep(b, 1100) }}
while !t.is_done() {{ sleep(0.01) }}
gc()
with lock(b) {{ puts("relocked") }}
deep(b, 1100)
"#
    );
    let o = run_catx("deep_lock_gc.cx", &src);
    assert_eq!(o.code, Some(101), "{}", o.ctx());
    assert_eq!(o.stdout, "relocked\n", "{}", o.ctx());
    assert!(o.stderr.contains(&format!("panic: {LOCK_ERROR}\n")), "{}", o.ctx());
    assert!(o.stderr.contains("panicked and was never joined"), "{}", o.ctx());
}
