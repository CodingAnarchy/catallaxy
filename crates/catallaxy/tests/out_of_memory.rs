//! Real allocation failures: programs run under an address-space limit (`RLIMIT_AS`), so that
//! `mmap` and `malloc` fail as they would on a machine out of memory. Each failure must be a
//! `MemoryError` (or `ThreadError`) panic, not an abort: `join` sees it on another thread, the
//! program carries on once the memory is garbage, and on the main thread it ends the program
//! with a trace and status 101.
//!
//! Linux only: macOS does not enforce `RLIMIT_AS`, and Windows has no such limit.
#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CATX: &str = env!("CARGO_BIN_EXE_catx");

/// The address-space limit. `catx` itself takes about 330 MB of it (mostly the main thread's
/// 256 MB native stack, reserved but untouched), which leaves a few hundred MB to fill.
const LIMIT: u64 = 768 << 20;

/// The programs fill memory, so they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

struct Outcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_limited(name: &str, src: &str) -> Outcome {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("catx-oom-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    let (out_path, err_path) = (dir.join(format!("{name}.out")), dir.join(format!("{name}.err")));
    let mut cmd = Command::new(CATX);
    cmd.arg("run")
        .arg(&path)
        // One malloc arena that grows a page at a time (no 64 MB arena reservations per thread,
        // no 128 KB padding), so that what fails first is the runtime's own large requests.
        .env("GLIBC_TUNABLES", "glibc.malloc.arena_max=1:glibc.malloc.top_pad=0")
        .env("MALLOC_ARENA_MAX", "1")
        .env("MALLOC_TOP_PAD_", "0")
        .env("CATX_GC_THREADS", "1")
        .env("CATX_THREADS", "1")
        .env_remove("CATX_GC_STRESS")
        .env_remove("CATX_MAX_HEAP")
        .stdout(Stdio::from(std::fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()));
    // SAFETY: setrlimit is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit { rlim_cur: LIMIT as libc::rlim_t, rlim_max: LIMIT as libc::rlim_t };
            if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("run catx");
    let deadline = Instant::now() + Duration::from_secs(180);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Outcome {
        code: status.code(),
        stdout: std::fs::read_to_string(&out_path).unwrap_or_default(),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    }
}

/// Checks the common shape: `join` saw the error, the program carried on, and the same error on
/// the main thread ended it with a trace through `func`.
fn check(o: &Outcome, join: &[&str], after: &str, panic: &[&str], func: &str) {
    let ctx = format!("status {:?}\n--- stdout\n{}--- stderr\n{}", o.code, o.stdout, o.stderr);
    assert_eq!(o.code, Some(101), "{ctx}");
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert!(lines.len() >= 2, "{ctx}");
    assert!(join.iter().any(|j| lines[0].starts_with(&format!("join: {j}"))), "{ctx}");
    assert_eq!(lines.last().copied(), Some(after), "{ctx}");
    assert!(panic.iter().any(|p| o.stderr.contains(&format!("panic: {p}"))), "{ctx}");
    assert!(o.stderr.contains(&format!("    at {func} (")), "{ctx}");
}

/// Small objects fill the heap until the OS refuses a segment: the emergency reserve carries
/// the allocation to a safepoint, where the error is raised.
#[test]
fn heap_segments() {
    let src = r#"class Node {
  value: Int
  next: Node?
}
fn grow() -> Int {
  head: Node? = nil
  n = 0
  while true {
    head = Node.new(n, head)
    n += 1
  }
  n
}
t = spawn { grow() }
match t.join() {
  Ok(n) => puts("grew to #{n}")
  Err(e) => puts("join: #{e}")
}
head: Node? = nil
for i in 0...1_000_000 { head = Node.new(i, head) }
puts("after: #{head!.value}")
head = nil
grow()
"#;
    let o = run_limited("segments.cx", src);
    let msg = "MemoryError: out of memory: the system refused to grow the heap";
    check(&o, &[msg], "after: 999999", &[msg], "grow");
}

/// Strings: a concatenation and a `StringBuilder` grown until the allocator refuses.
#[test]
fn strings() {
    let src = r#"fn grow() -> Int {
  s = "x" * 1000
  while true { s = s + s }
  s.length
}
fn build() -> Int {
  sb = StringBuilder.new()
  chunk = "y" * 65536
  while true { sb.push(chunk) }
  sb.length
}
for f in [grow, build] {
  t = spawn move { f() }
  match t.join() {
    Ok(n) => puts("grew to #{n}")
    Err(e) => puts("join: #{e}")
  }
}
s = "z" * 1000
for i in 0...15 { s = s + s }
puts("after: #{s.length}")
s = ""
grow()
"#;
    let o = run_limited("strings.cx", src);
    check(
        &o,
        &["MemoryError: cannot allocate a string of "],
        "after: 32768000",
        &["MemoryError: cannot allocate a string of "],
        "grow",
    );
    let ctx = format!("{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout.lines().nth(1).is_some_and(|l| l.starts_with("join: MemoryError: cannot grow a StringBuilder")),
        "{ctx}"
    );
}

/// Threads started until one cannot be: no memory for its register stack (a `MemoryError`) or
/// for its native stack (the OS refuses the thread: a `ThreadError`).
#[test]
fn threads() {
    let src = r#"fn start_all(gate: Channel[Int], started: Channel[Thread[Int]]) -> Int {
  n = 0
  while true {
    started.send(spawn { gate.recv() ?? 0 })
    n += 1
  }
  n
}
gate: Channel[Int] = Channel.new()
started: Channel[Thread[Int]] = Channel.new()
t = spawn { start_all(gate, started) }
match t.join() {
  Ok(n) => puts("started #{n}")
  Err(e) => puts("join: #{e}")
}
gate.close()
started.close()
joined = 0
for w in started {
  w.join()
  joined += 1
}
puts("joined: #{joined > 0}")
puts("after: #{(spawn { 6 * 7 }).join().unwrap()}")
start_all(Channel.new(), Channel.new())
"#;
    let o = run_limited("threads.cx", src);
    let errs = ["ThreadError: cannot start a thread", "MemoryError: cannot allocate a 8192 KB register stack"];
    check(&o, &errs, "after: 42", &errs, "start_all");
}

/// Fibers started until one cannot be (its native stack or its registers).
#[test]
fn fibers() {
    let src = r#"fn start_all() -> Int {
  fs: List[Fiber[Int]] = []
  while true { fs.push(Fiber.start { 1 }) }
  fs.length
}
t = spawn { start_all() }
match t.join() {
  Ok(n) => puts("started #{n}")
  Err(e) => puts("join: #{e}")
}
puts("after: #{Fiber.start { 6 * 7 }.join().unwrap()}")
start_all()
"#;
    let o = run_limited("fibers.cx", src);
    let errs =
        ["MemoryError: cannot allocate a 256 KB fiber stack", "MemoryError: cannot allocate a 512 KB register stack"];
    check(&o, &errs, "after: 42", &errs, "start_all");
}
