//! Registers left behind by returned calls must not keep objects alive (issue #47).
//! (One program per process, so this file holds a single test.)

use catallaxy::{native, Engine, RtResult, Value, Vm};

/// `native fn live_bytes() -> Int`: live bytes found by the last collection.
fn live_bytes(vm: &mut Vm, _: &[Value]) -> RtResult<Value> {
    Ok(native::new_int(vm, catallaxy_gc::stats::live_bytes() as i64))
}

#[test]
fn returned_frames_do_not_root_dead_objects() {
    let mut engine = Engine::new();
    engine.register("live_bytes", live_bytes);
    // `build` runs at the bottom of `work`'s register window, so its frames (and their
    // callees') leave subtrees in registers `work` only overwrites later, or never.
    let src = r##"
native fn live_bytes() -> Int
class Node {
  left: Node?
  right: Node?
  id: Int
}
fn build(d: Int) -> Node {
  if d == 0 { Node.new(nil, nil, 0) } else { Node.new(build(d - 1), build(d - 1), d) }
}
fn work() -> Int {
  keep: Node? = build(16)
  keep = nil
  t = 0
  for i in 0...100 { x = [i, i + 1, i + 2]; y = x.map { |v| v * 2 }; t += y.sum() + x.length }
  gc()
  gc()
  live = live_bytes()
  assert(live < 500_000, "#{live} bytes live after dropping the tree")
  t
}
fn find(d: Int) -> Int {
  [1, 2].each { |_v|
    t = build(d)
    return t.id
  }
  0
}
fn after_unwind() -> Int {
  n = find(16)
  gc()
  gc()
  live = live_bytes()
  assert(live < 500_000, "#{live} bytes live after a non-local return dropped the tree")
  # Widens this frame's register window over the slots the block's frames used.
  wide = [n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n]
  wide.length
}
puts(work())
puts(after_unwind())
"##;
    assert_eq!(engine.run_source("gc_roots.cx", src), Ok(0));
}
