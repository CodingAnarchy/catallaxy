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

/// The error raised at a safepoint when the system refuses memory the runtime cannot report on
/// the spot (a heap segment, or an allocation inside a standard collection, which the global
/// allocator retries once with the headroom released). Any site may end up with it, when the
/// system refuses one of those allocations first.
const OOM: &str = "MemoryError: out of memory: the system refused an allocation";

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
    eprintln!("OUTCOME[{after}] {} | {}", o.stdout.lines().next().unwrap_or(""), o.stderr.lines().find(|l| l.starts_with("panic:")).unwrap_or(""));
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
    check(&o, &[OOM], "after: 999999", &[OOM], "grow");
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
    let errs = ["MemoryError: cannot allocate a string of ", OOM];
    check(&o, &errs, "after: 32768000", &errs, "grow");
    let ctx = format!("{}{}", o.stdout, o.stderr);
    let built = o.stdout.lines().nth(1).unwrap_or("");
    assert!(
        ["MemoryError: cannot grow a StringBuilder", OOM].iter().any(|e| built.starts_with(&format!("join: {e}"))),
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
    let errs = ["ThreadError: cannot start a thread", "MemoryError: cannot allocate a 8192 KB register stack", OOM];
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
    let errs = [
        "MemoryError: cannot allocate a 256 KB fiber stack",
        "MemoryError: cannot allocate a 512 KB register stack",
        OOM,
    ];
    check(&o, &errs, "after: 42", &errs, "start_all");
}

/// List and dict buffers grown until the allocator refuses.
#[test]
fn containers() {
    let src = r#"fn grow_list() -> Int {
  xs: List[Int] = []
  while true { xs.push(1) }
  xs.length
}
fn grow_dict() -> Int {
  d: Dict[Int, Int] = [:]
  i = 0
  while true {
    d[i] = i
    i += 1
  }
  d.length
}
for f in [grow_list, grow_dict] {
  t = spawn move { f() }
  match t.join() {
    Ok(n) => puts("grew to #{n}")
    Err(e) => puts("join: #{e}")
  }
}
xs: List[Int] = []
for i in 0...1_000_000 { xs.push(i) }
d: Dict[Int, Int] = [:]
for i in 0...100_000 { d[i] = i }
puts("after: #{xs.length + d.length}")
xs = []
grow_list()
"#;
    let o = run_limited("containers.cx", src);
    let errs = ["MemoryError: cannot grow a list to ", OOM];
    check(&o, &errs, "after: 1100000", &errs, "grow_list");
    let ctx = format!("{}{}", o.stdout, o.stderr);
    let dict = o.stdout.lines().nth(1).unwrap_or("");
    assert!(
        ["MemoryError: cannot grow a table to ", OOM].iter().any(|e| dict.starts_with(&format!("join: {e}"))),
        "{ctx}"
    );
}

/// What natives that build a collection raise when the allocator refuses one of its buffers:
/// the vector they collect into, the list they make of it, its strings, a file's contents
/// (`cannot allocate ...`, `cannot read ...`), or the list the program keeps them in (`cannot
/// grow a list ...`).
///
/// No list in these programs holds more than a few thousand objects. Marking a list pushes the
/// objects it holds on the collector's mark stack, whose growth is not checked: a collection
/// that runs while memory is exhausted could abort on a list of a hundred thousand strings.
/// Results are kept in a linked list whose `next` field comes first, so that marking it does
/// not stack up one entry per node.
const NATIVE: [&str; 2] = ["MemoryError: cannot ", OOM];

/// Strings split into lists: `split`, `chars` and `lines`.
#[test]
fn string_lists() {
    let src = r#"class Kept {
  next: Kept?
  items: List[String]
}
fn grow() -> Int {
  text = "ab cd\n" * 400
  kept: Kept? = nil
  n = 0
  while true {
    kept = Kept.new(kept, text.split(" "))
    kept = Kept.new(kept, text.chars())
    kept = Kept.new(kept, text.lines())
    n += 1
  }
  n
}
t = spawn { grow() }
match t.join() {
  Ok(n) => puts("grew to #{n}")
  Err(e) => puts("join: #{e}")
}
text = "ab cd\n" * 1000
puts("after: #{text.split(" ").length + text.chars().length + text.lines().length}")
grow()
"#;
    let o = run_limited("string_lists.cx", src);
    check(&o, &NATIVE, "after: 8001", &NATIVE, "grow");
}

/// Parsers: `Json.parse` (an array of a million numbers) and `Csv.parse` (2,000 rows of 200
/// fields).
#[test]
fn parsers() {
    let src = r#"fn grow() -> Int {
  json = "[" + "0," * 1_000_000 + "0]"
  csv = ("a," * 199 + "a\n") * 2_000
  docs: List[Dyn] = []
  tables: List[List[List[String]]] = []
  while true {
    docs.push(Json.parse(json).unwrap())
    tables.push(Csv.parse(csv).unwrap())
  }
  docs.length
}
t = spawn { grow() }
match t.join() {
  Ok(n) => puts("grew to #{n}")
  Err(e) => puts("join: #{e}")
}
doc = Json.parse("[" + "1," * 100_000 + "1]").unwrap()
rows = Csv.parse("a,b\n" * 2_000).unwrap()
puts("after: #{if doc == nil { 0 } else { 1 } + rows.length}")
grow()
"#;
    let o = run_limited("parsers.cx", src);
    check(&o, &NATIVE, "after: 2001", &NATIVE, "grow");
}

/// Copies of lists and dicts: `sorted`, `reverse`, `+` and `keys`.
#[test]
fn list_copies() {
    let src = r#"fn grow() -> Int {
  xs = (0...1_000_000).to_list()
  d: Dict[Int, Int] = [:]
  for i in 0...200_000 { d[i] = i }
  keep: List[List[Int]] = []
  while true {
    keep.push(xs.sorted())
    keep.push(xs.reverse())
    keep.push(xs + xs)
    keep.push(d.keys())
  }
  keep.length
}
t = spawn { grow() }
match t.join() {
  Ok(n) => puts("grew to #{n}")
  Err(e) => puts("join: #{e}")
}
xs = (0...100_000).to_list()
puts("after: #{xs.sorted().length + xs.reverse().length + (xs + xs).length}")
grow()
"#;
    let o = run_limited("list_copies.cx", src);
    check(&o, &NATIVE, "after: 400000", &NATIVE, "grow");
}

/// Files read whole: `File.read`, `File.read_bytes` and `File.read_lines` (2,000 lines of 500
/// bytes).
#[test]
fn files() {
    let dir = std::env::temp_dir().join(format!("catx-oom-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("files.txt");
    std::fs::write(&data, format!("{}\n", "x".repeat(500)).repeat(2_000)).unwrap();
    let src = r#"fn grow(path: String) -> Int {
  texts: List[String] = []
  blobs: List[Bytes] = []
  lines: List[List[String]] = []
  while true {
    texts.push(File.read(path).unwrap())
    blobs.push(File.read_bytes(path).unwrap())
    lines.push(File.read_lines(path).unwrap())
  }
  texts.length
}
path = "PATH"
t = spawn { grow(path) }
match t.join() {
  Ok(n) => puts("grew to #{n}")
  Err(e) => puts("join: #{e}")
}
puts("after: #{File.read(path).unwrap().length + File.read_lines(path).unwrap().length}")
grow(path)
"#
    .replace("PATH", data.to_str().unwrap());
    let o = run_limited("files.cx", &src);
    check(&o, &NATIVE, "after: 1004000", &NATIVE, "grow");
}
