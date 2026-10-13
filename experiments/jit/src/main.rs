//! JIT experiment harness (issue #7). See README.md.
//!
//! ```text
//! jit-experiment [--poll call|load|none] [--gc-stress] [-v] (interp|jit) run  <file.cx>
//! jit-experiment [--poll call|load|none] [--gc-stress] [-v] (interp|jit) call <file.cx> <fn> <int>...
//! ```
//!
//! `run` executes the program's entry functions like `catx run`, JIT-compiling those that fit the
//! subset (and their callees). `call` calls one function with Int arguments and prints the result,
//! without running the module bodies. Timings go to stderr.

mod jit;

use std::path::Path;
use std::process::exit;
use std::time::Instant;

use catallaxy_compiler::driver::{compile_path, FsLoader, Options};
use catallaxy_runtime::{Trap, Value};
use catallaxy_vm::interp::MAIN_STACK;
use catallaxy_vm::{Program, Vm};

use jit::{Analysis, Jit, Poll};

struct Args {
    jit: bool,
    poll: Poll,
    verbose: bool,
    gc_stress: bool,
    file: String,
    call: Option<(String, Vec<i64>)>,
}

fn usage() -> ! {
    eprintln!("usage: jit-experiment [--poll call|load|none] [--gc-stress] [-v] (interp|jit) (run <file.cx> | call <file.cx> <fn> <int>...)");
    exit(2)
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1).peekable();
    let mut poll = Poll::Call;
    let mut verbose = false;
    let mut gc_stress = false;
    while let Some(a) = it.peek().cloned() {
        match a.as_str() {
            "--poll" => {
                it.next();
                poll = match it.next().as_deref() {
                    Some("call") => Poll::Call,
                    Some("load") => Poll::Load,
                    Some("none") => Poll::None,
                    _ => usage(),
                };
            }
            "--gc-stress" => {
                it.next();
                gc_stress = true;
            }
            "-v" => {
                it.next();
                verbose = true;
            }
            _ => break,
        }
    }
    let jit = match it.next().as_deref() {
        Some("interp") => false,
        Some("jit") => true,
        _ => usage(),
    };
    let cmd = it.next().unwrap_or_else(|| usage());
    let file = it.next().unwrap_or_else(|| usage());
    let call = match cmd.as_str() {
        "run" => None,
        "call" => {
            let name = it.next().unwrap_or_else(|| usage());
            let ints = it.map(|s| s.parse().unwrap_or_else(|_| usage())).collect();
            Some((name, ints))
        }
        _ => usage(),
    };
    Args { jit, poll, verbose, gc_stress, file, call }
}

fn main() {
    let args = parse_args();
    let comp = compile_path(Path::new(&args.file), &FsLoader, &Options::default());
    let Some(program) = comp.program else {
        for d in &comp.diags {
            eprint!("{}", comp.sm.render(d, false));
        }
        exit(1)
    };
    // Like `catx run`: a fresh thread with a large native stack.
    let code = std::thread::Builder::new()
        .name("catx-main".into())
        .stack_size(256 << 20)
        .spawn(move || run(&args, &program))
        .unwrap()
        .join()
        .unwrap_or(101);
    exit(code)
}

fn compile(args: &Args, roots: &[u32]) -> (Analysis, Jit) {
    let t0 = Instant::now();
    let an = jit::analyse(roots);
    let analyse_time = t0.elapsed();
    if args.verbose {
        for (fid, why) in &an.rejected {
            eprintln!("not JIT-able: {} ({why})", catallaxy_vm::program::func(*fid).name);
        }
    }
    let j = Jit::compile(&an, args.poll);
    let mut fns: Vec<_> = an.fns.values().collect();
    fns.sort_by_key(|f| f.fid);
    for f in fns {
        let c = &j.compiled[&f.fid];
        eprintln!(
            "jit: {:<24} {:>4} words  {:>5} bytes  {:>7.1} us{}",
            f.name,
            catallaxy_vm::program::func(f.fid).code.len(),
            c.code_size,
            c.compile_time.as_secs_f64() * 1e6,
            if c.ret_int { "  (returns unboxed Int)" } else { "" }
        );
    }
    eprintln!(
        "jit: analysis {:.1} us, finalize {:.1} us, total {:.1} us",
        analyse_time.as_secs_f64() * 1e6,
        j.finalize_time.as_secs_f64() * 1e6,
        t0.elapsed().as_secs_f64() * 1e6
    );
    (an, j)
}

/// Runs `fid` in the JIT if compiled, falling back to the interpreter on a bail-out.
fn call(vm: &mut Vm, j: Option<&Jit>, fid: u32, argv: &[Value]) -> Result<Value, Trap> {
    if let Some(j) = j.filter(|j| j.compiled.contains_key(&fid)) {
        if let Some(v) = j.call(fid, vm, argv) {
            return Ok(v);
        }
        if let Some(t) = jit::NATIVE_TRAP.with(|c| c.borrow_mut().take()) {
            return Err(t);
        }
        eprintln!("jit: {} bailed out; rerunning it in the interpreter", catallaxy_vm::program::func(fid).name);
    }
    vm.call_func(fid, argv)
}

fn run(args: &Args, program: &Program) -> i32 {
    let mut vm = Vm::for_current_thread(MAIN_STACK);
    if let Err(e) = catallaxy_vm::load(program, &mut vm.m, &catallaxy_stdlib::lookup) {
        eprintln!("error: {}", e.0);
        return 70;
    }
    let (roots, argv): (Vec<u32>, Vec<Value>) = match &args.call {
        None => (program.entry.clone(), vec![]),
        Some((name, ints)) => {
            let Some(fid) = program.funcs.iter().position(|f| &f.name == name) else {
                eprintln!("no function `{name}`");
                return 2;
            };
            (vec![fid as u32], ints.iter().map(|&i| Value::small_int(i)).collect())
        }
    };
    let compiled = args.jit.then(|| compile(args, &roots));
    let j = compiled.as_ref().map(|(_, j)| j);
    let stress = args.gc_stress.then(gc_stress::start);
    let t0 = Instant::now();
    let mut status = 0;
    if args.call.is_some() {
        match call(&mut vm, j, roots[0], &argv) {
            Ok(v) if catallaxy_runtime::ops::is_int(v) => println!("{}", catallaxy_runtime::ops::format_int(v)),
            Ok(v) => println!("{v:?}"),
            Err(e) => status = report(e),
        }
    } else {
        for &fid in &roots {
            if let Err(e) = call(&mut vm, j, fid, &[]) {
                status = report(e);
                break;
            }
        }
    }
    catallaxy_stdlib::flush();
    eprintln!("exec: {:.3} ms", t0.elapsed().as_secs_f64() * 1e3);
    if let Some(s) = stress {
        s.finish(&vm);
    }
    status
}

/// A second mutator thread that forces a collection every millisecond and records how long each
/// one waits for the main thread to reach a safepoint. Without polls in JIT code, it would wait
/// for the whole JIT call.
mod gc_stress {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use catallaxy_vm::Vm;

    pub struct Stress {
        stop: Arc<AtomicBool>,
        handle: JoinHandle<Vec<Duration>>,
    }

    pub fn start() -> Stress {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut vm = Vm::for_current_thread(1 << 20);
            let mut lat = Vec::new();
            while !s.load(Ordering::Relaxed) {
                catallaxy_gc::enter_native(&vm.m.rec);
                std::thread::sleep(Duration::from_millis(1));
                catallaxy_gc::leave_native(&vm.m.rec);
                let t = Instant::now();
                // `Poll::Load` code watches the harness-local flag instead of the GC's.
                crate::jit::LOCAL_GC_FLAG.store(true, Ordering::Release);
                catallaxy_gc::collect_now(&vm.m.rec);
                crate::jit::LOCAL_GC_FLAG.store(false, Ordering::Release);
                lat.push(t.elapsed());
            }
            vm.shutdown();
            lat
        });
        Stress { stop, handle }
    }

    impl Stress {
        pub fn finish(self, vm: &Vm) {
            self.stop.store(true, Ordering::Relaxed);
            // Let a collection in progress stop this thread while it waits.
            catallaxy_gc::enter_native(&vm.m.rec);
            let mut lat = self.handle.join().unwrap();
            catallaxy_gc::leave_native(&vm.m.rec);
            lat.sort();
            if lat.is_empty() {
                eprintln!("gc-stress: no collections");
                return;
            }
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            eprintln!(
                "gc-stress: {} collections, collect_now latency (includes waiting for the main thread to reach a safepoint) median {:.0} us, max {:.0} us",
                lat.len(),
                us(lat[lat.len() / 2]),
                us(*lat.last().unwrap())
            );
        }
    }
}

fn report(e: Trap) -> i32 {
    catallaxy_stdlib::flush();
    match e {
        Trap::Panic(msg) => {
            eprintln!("panic: {msg}");
            101
        }
        Trap::Exit(code) => code,
        Trap::Unwind { .. } => {
            eprintln!("panic: internal error: stray non-local exit");
            101
        }
    }
}
