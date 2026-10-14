//! Catallaxy: a statically typed, Ruby-flavored language whose threads run truly in parallel.
//!
//! This crate is the `catx` command-line tool and the embedding API. To run Catallaxy from Rust,
//! or to extend it with native functions written in Rust:
//!
//! ```no_run
//! use catallaxy::{Engine, Value, Vm, RtResult};
//!
//! fn double(_vm: &mut Vm, args: &[Value]) -> RtResult<Value> {
//!     let n = catallaxy::native::int(args[0])?;
//!     Ok(Value::small_int(n * 2))
//! }
//!
//! let mut engine = Engine::new();
//! engine.register("double", double);
//! // In Catallaxy: `native fn double(n: Int) -> Int` declares it.
//! let code = engine.run_source("demo.cx", "native fn double(n: Int) -> Int\nputs(double(21))\n");
//! assert_eq!(code, Ok(0));
//! ```
//!
//! A process runs one program: the VM's code and class tables are process-wide.

use std::path::Path;

pub use catallaxy_compiler::diag::Diagnostic;
pub use catallaxy_runtime::{HandleScope, Local, Mutator, RtResult, Trap, Value};
pub use catallaxy_vm::{NativeFn, Program, Vm};

use catallaxy_compiler::driver::{compile_path, compile_source, FsLoader, Options};
use catallaxy_runtime::Trap as RtTrap;
use catallaxy_vm::interp::MAIN_STACK;

/// Helpers for writing native functions.
pub mod native {
    use catallaxy_runtime::object::str_of;
    use catallaxy_runtime::{ops, panic, RtResult, Value};
    use catallaxy_vm::Vm;

    /// An `Int` argument (panics in Catallaxy if it is not one or does not fit in i64).
    pub fn int(v: Value) -> RtResult<i64> {
        match ops::int_as_i64(v) {
            Some(i) if ops::is_int(v) => Ok(i),
            _ => panic("TypeError: expected Int"),
        }
    }
    /// A `Float` argument (an `Int` is converted).
    pub fn float(v: Value) -> RtResult<f64> {
        if v.is_float() {
            Ok(v.as_f64())
        } else if ops::is_int(v) {
            Ok(ops::int_to_f64(v))
        } else {
            panic("TypeError: expected Float")
        }
    }
    /// A `String` argument.
    pub fn string<'a>(v: Value) -> RtResult<&'a str> {
        match str_of(v) {
            Some(s) => Ok(s),
            None => panic("TypeError: expected String"),
        }
    }
    /// Allocates a string.
    pub fn new_string(vm: &mut Vm, s: &str) -> Value {
        vm.m.alloc_str(s)
    }
    /// Allocates an Int (big integers as needed).
    pub fn new_int(vm: &mut Vm, i: i64) -> Value {
        vm.m.int(i)
    }
    /// Allocates a list.
    pub fn new_list(vm: &mut Vm, items: Vec<Value>) -> Value {
        vm.m.alloc_list(items)
    }
    /// Wraps Rust data in a `NativeHandle`: an opaque, frozen value that may be stored in frozen
    /// classes and used from any thread, so `data` must be immutable or synchronize itself.
    /// It is dropped when the collector frees the handle.
    pub fn new_handle<T: Send + Sync + 'static>(vm: &mut Vm, data: T) -> Value {
        vm.m.alloc_native_frozen(Box::new(data))
    }
    /// The data of a `NativeHandle` argument made by [`new_handle`] with the same `T`, or `None`.
    pub fn handle<'a, T: 'static>(v: Value) -> Option<&'a T> {
        use catallaxy_runtime::object::{as_native, kind_of};
        if kind_of(v) != catallaxy_runtime::ObjKind::Native {
            return None;
        }
        // SAFETY: `v` is a native object; an argument stays alive for the native call.
        let obj = unsafe { as_native(v) };
        (*obj.data).as_any().downcast_ref::<T>()
    }
    /// Calls a Catallaxy closure. Values the native holds across this call must be rooted in a
    /// [`crate::HandleScope`], because the call may run the collector.
    pub fn call(vm: &mut Vm, closure: Value, args: &[Value]) -> RtResult<Value> {
        vm.call_value(closure, args)
    }
}

/// Compiles and runs Catallaxy programs, with optional native extensions.
#[derive(Default)]
pub struct Engine {
    _private: (),
}

impl Engine {
    pub fn new() -> Self {
        Engine { _private: () }
    }

    /// Registers a native function, declared in Catallaxy as `native fn name(...)`.
    pub fn register(&mut self, name: &str, f: NativeFn) -> &mut Self {
        catallaxy_stdlib::register_native(name, f);
        self
    }

    /// Sets the program's command-line arguments (`args()` in Catallaxy). Call before running.
    pub fn set_args(&mut self, args: Vec<String>) -> &mut Self {
        let _ = catallaxy_stdlib::ARGS.set(args);
        self
    }

    /// Type-checks a file and everything it imports.
    pub fn check_file(&self, path: &Path) -> Vec<Diagnostic> {
        compile_path(path, &FsLoader, &Options { check_only: true }).diags
    }

    /// Compiles and runs a file. Returns the exit code, or the rendered compile errors.
    pub fn run_file(&self, path: &Path) -> Result<i32, String> {
        let comp = compile_path(path, &FsLoader, &Options::default());
        match comp.program {
            Some(p) => Ok(execute(p)),
            None => Err(comp.diags.iter().map(|d| comp.sm.render(d, false)).collect()),
        }
    }

    /// Compiles and runs source text (`name` is used for diagnostics and relative imports).
    pub fn run_source(&self, name: &str, src: &str) -> Result<i32, String> {
        let comp = compile_source(Path::new(name), src, &Options::default());
        match comp.program {
            Some(p) => Ok(execute(p)),
            None => Err(comp.diags.iter().map(|d| comp.sm.render(d, false)).collect()),
        }
    }
}

/// Runs a program on a fresh main VM thread (with a large native stack). Returns the exit code.
pub fn execute(program: Program) -> i32 {
    let h = std::thread::Builder::new()
        .name("catx-main".into())
        .stack_size(256 << 20)
        .spawn(move || run_program(&program))
        .expect("spawn main thread");
    h.join().unwrap_or(101)
}

fn run_program(program: &Program) -> i32 {
    let mut vm = Vm::for_current_thread(MAIN_STACK);
    catallaxy_runtime::ownership::set_promotion_hook(|cls| {
        eprintln!("warning: promoted a {cls} to shared on access from another thread");
    });
    if let Err(e) = catallaxy_vm::load(program, &mut vm.m, &catallaxy_stdlib::lookup) {
        eprintln!("error: {}", e.0);
        return 70;
    }
    for &fid in &program.entry {
        match vm.call_func(fid, &[]) {
            Ok(_) => {}
            Err(RtTrap::Panic(msg)) => {
                catallaxy_stdlib::flush();
                eprintln!("panic: {msg}");
                if let Some(trace) = vm.panic_trace.take() {
                    for line in trace.iter().take(30) {
                        eprintln!("    at {line}");
                    }
                    if trace.len() > 30 {
                        eprintln!("    ... {} more", trace.len() - 30);
                    }
                }
                catallaxy_stdlib::report_unjoined_panics();
                return 101;
            }
            Err(RtTrap::Exit(code)) => {
                catallaxy_stdlib::flush();
                catallaxy_stdlib::report_unjoined_panics();
                return code;
            }
            Err(RtTrap::Unwind { .. }) => {
                catallaxy_stdlib::flush();
                eprintln!("panic: internal error: stray non-local exit");
                return 101;
            }
        }
    }
    // The program ends when its fibers do.
    catallaxy_stdlib::fiber::drain(&mut vm);
    catallaxy_stdlib::flush();
    catallaxy_stdlib::report_unjoined_panics();
    if std::env::var_os("CATX_GC_STATS").is_some() {
        let (total, max) = catallaxy_gc::stats::pauses();
        eprintln!(
            "gc: {} collections, {:.1} MB heap ({:.1} MB returned to the OS), {:.1} MB live after last, pauses {:.1} ms total / {:.2} ms max",
            catallaxy_gc::stats::collections(),
            catallaxy_gc::total_heap_bytes() as f64 / 1e6,
            catallaxy_gc::stats::released_bytes() as f64 / 1e6,
            catallaxy_gc::stats::live_bytes() as f64 / 1e6,
            total as f64 / 1e6,
            max as f64 / 1e6
        );
        let (n, ns, max_ns, freed, promoted) = catallaxy_gc::stats::minors();
        eprintln!(
            "gc: {n} minor collections, {:.1} ms total / {:.2} ms max, {:.1} MB freed, {:.1} MB promoted",
            ns as f64 / 1e6,
            max_ns as f64 / 1e6,
            freed as f64 / 1e6,
            promoted as f64 / 1e6
        );
    }
    0
}
