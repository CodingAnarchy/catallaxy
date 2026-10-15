//! Collection methods that once took quadratic time, run at sizes where that would take hours:
//! prelude methods that accumulate with `concat` (#140) and in-place list edits (#172). Each
//! program finishes in well under a second when linear.

use std::process::Command;
use std::time::{Duration, Instant};

const CATX: &str = env!("CARGO_BIN_EXE_catx");

fn run_within(name: &str, src: &str, limit: Duration) -> String {
    let dir = std::env::temp_dir().join(format!("catx-linear-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.cx"));
    std::fs::write(&path, src).unwrap();
    let start = Instant::now();
    let mut child = Command::new(CATX).arg("run").arg(&path).stdout(std::process::Stdio::piped()).spawn().unwrap();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            panic!("{name} ran longer than {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{name} failed");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn accumulating_prelude_methods_are_linear() {
    let src = r#"xs = (0...500000).to_list()
puts(xs.flat_map { |x| [x, x] }.length)
puts(xs.lazy().flat_map { |x| [x, -x] }.to_list().length)
flat = xs.map { |x| [x, x + 1] }.flatten()
puts(flat.length)
flat.delete_if { |x| x % 2 == 0 }
puts(flat.length)
puts((0...1000).flat_map { |i| List.filled(1000, i) }.length)
"#;
    let out = run_within("accumulate", src, Duration::from_secs(60));
    assert_eq!(out, "1000000\n1000000\n1000000\n500000\n1000000\n");
}

#[test]
fn in_place_list_edits_are_linear_in_their_count() {
    let src = r#"xs: List[Int] = (0...200000).to_list()
for i in 0...200000 { xs.concat([i]) }
for i in 0...100000 { xs.remove_at(xs.length - 1) }
for i in 0...100000 { xs.truncate(xs.length - 1) }
puts(xs.length)
"#;
    let out = run_within("edits", src, Duration::from_secs(60));
    assert_eq!(out, "200000\n");
}
