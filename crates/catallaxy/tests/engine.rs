//! Embedding API: running a program with a native extension that calls back into Catallaxy.
//! (One program per process, so this file holds a single test.)

use catallaxy::{native, Engine, HandleScope, RtResult, Trap, Value, Vm};

/// `native fn apply_twice(f: fn(Int) -> Int, x: Int) -> List[Int]`: calls back into Catallaxy
/// while holding a list, so the list is rooted in a HandleScope.
fn apply_twice(vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
    let (f, x) = (args[0], args[1]);
    let scope: HandleScope = vm.handle_scope();
    let first = native::call(vm, f, &[x])?;
    // `first` must survive the next call, which may collect.
    let held = scope.root(first);
    let second = native::call(vm, f, &[first])?;
    let items = vec![scope.get(held), second];
    Ok(native::new_list(vm, items))
}

fn greeting(vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
    let name = native::string(args[0])?;
    Ok(native::new_string(vm, &format!("hello from Rust, {name}")))
}

/// A `NativeHandle` around self-synchronizing Rust data, kept in a frozen class.
fn counter_new(vm: &mut Vm, _: &[Value]) -> RtResult<Value> {
    Ok(native::new_handle(vm, std::sync::atomic::AtomicI64::new(0)))
}

fn counter_bump(vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
    let Some(c) = native::handle::<std::sync::atomic::AtomicI64>(args[0]) else {
        return Err(Trap::Panic("not a counter".into()));
    };
    let n = c.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    Ok(native::new_int(vm, n))
}

#[test]
fn engine_runs_program_with_native_extensions() {
    let mut engine = Engine::new();
    engine.register("apply_twice", apply_twice).register("Greeter.greet", greeting);
    engine.register("counter_new", counter_new).register("counter_bump", counter_bump);
    let src = r#"
native fn apply_twice(f: fn(Int) -> Int, x: Int) -> List[Int]
class Greeter {
  native fn greet(name: String) -> String
}
r = apply_twice({ |v| v * 10 }, 3)
assert_eq(r, [30, 300])
assert_eq(Greeter.greet("Ada"), "hello from Rust, Ada")
native fn counter_new() -> NativeHandle
native fn counter_bump(h: NativeHandle) -> Int
frozen class Counter { h: NativeHandle }
c = Counter.new(counter_new())
ts = (0...4).map { |_i| spawn { counter_bump(c.h) } }
for t in ts { t.join().unwrap() }
assert_eq(counter_bump(c.h), 5)
gc()
puts("ok")
"#;
    assert_eq!(engine.run_source("embed.cx", src), Ok(0));
    let bad = engine.check_file(std::path::Path::new("does/not/exist.cx"));
    assert!(!bad.is_empty());
}
