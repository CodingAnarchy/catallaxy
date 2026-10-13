//! Scripting: regular expressions, subprocesses, paths, directories and file metadata.
//!
//! Prelude classes built here (`Regex`, `Match`, `Output`, `Subprocess`, `Metadata`) are found by
//! name with [`program::class_named`]; their fields are set by name, so the prelude declaration
//! is the single source of their layout.

use std::io::{BufRead, Read, Write as _};
use std::sync::Arc;

use catallaxy_runtime::class::{class_by_id, class_of};
use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::*;
use catallaxy_runtime::{intern, ownership, panic, ObjKind, RtResult, Value};
use catallaxy_vm::{program, slow, NativeFn, Vm};
use parking_lot::Mutex;

use crate::fiber::blocking_io;
use crate::*;

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "Regex.new" => regex_new,
        "Regex.escape" => |vm, a| {
            let r = regex::escape(s(a[0])?);
            Ok(new_str(vm, &r))
        },
        "Regex.matches" => |_, a| Ok(Value::bool(regex_of(a[0])?.is_match(s(a[1])?))),
        "Regex.find" => regex_find,
        "Regex.find_all" => regex_find_all,
        "Regex.replace" => |vm, a| {
            let r = regex_of(a[0])?.replace(s(a[1])?, s(a[2])?).into_owned();
            Ok(new_str(vm, &r))
        },
        "Regex.replace_all" => |vm, a| {
            let r = regex_of(a[0])?.replace_all(s(a[1])?, s(a[2])?).into_owned();
            Ok(new_str(vm, &r))
        },
        "Regex.split" => |vm, a| {
            let re = regex_of(a[0])?;
            let items: Vec<Value> = re.split(s(a[1])?).map(|p| vm.m.alloc_str(p)).collect();
            Ok(new_list(vm, items))
        },
        "Regex.capture_names" => |vm, a| {
            let re = regex_of(a[0])?;
            let items: Vec<Value> = re.capture_names().flatten().map(|n| vm.m.alloc_str(n)).collect();
            Ok(new_list(vm, items))
        },

        "Process.run" => process_run,
        "Process.start" => process_start,
        "Subprocess.write" => child_write,
        "Subprocess.close_stdin" => |_, a| {
            let c = child_of(a[0])?;
            // Dropping the pipe sends end-of-file; no system call blocks here.
            c.stdin.lock().take();
            Ok(Value::NIL)
        },
        "Subprocess.read_line" => child_read_line,
        "Subprocess.read_all" => child_read_all,
        "Subprocess.wait" => child_wait,
        "Subprocess.kill" => |vm, a| {
            let c = child_of(a[0])?;
            let r = blocking_io(vm, move || c.child.lock().kill());
            Ok(match r {
                Ok(()) => ok(vm, Value::NIL),
                Err(e) => err(vm, &e.to_string()),
            })
        },

        "Path.join" => |vm, a| {
            let r = path_join(s(a[0])?, s(a[1])?);
            Ok(new_str(vm, &r))
        },
        "Path.join_all" => |vm, a| {
            let parts = list_items(vm, a[0])?;
            let mut out = String::new();
            for p in parts {
                out = path_join(&out, s(p)?);
            }
            Ok(new_str(vm, &out))
        },
        "Path.basename" => |vm, a| {
            let p = std::path::Path::new(s(a[0])?);
            let r = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(new_str(vm, &r))
        },
        "Path.dirname" => |vm, a| {
            let x = s(a[0])?;
            let r = match std::path::Path::new(x).parent() {
                Some(p) if p.as_os_str().is_empty() => ".".to_string(),
                Some(p) => p.to_string_lossy().into_owned(),
                None if x.is_empty() => ".".to_string(),
                None => x.to_string(),
            };
            Ok(new_str(vm, &r))
        },
        "Path.extension" => |vm, a| {
            Ok(match std::path::Path::new(s(a[0])?).extension() {
                Some(e) => {
                    let e = e.to_string_lossy().into_owned();
                    new_str(vm, &e)
                }
                None => Value::NIL,
            })
        },
        "Path.stem" => |vm, a| {
            let p = std::path::Path::new(s(a[0])?);
            let r = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(new_str(vm, &r))
        },
        "Path.is_absolute" => |_, a| Ok(Value::bool(std::path::Path::new(s(a[0])?).is_absolute())),

        "Dir.list" => |vm, a| {
            let p = s(a[0])?.to_string();
            let r = blocking_io(vm, || -> std::io::Result<Vec<String>> {
                let mut names: Vec<String> = std::fs::read_dir(&p)?
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                Ok(names)
            });
            Ok(match r {
                Ok(names) => {
                    let items: Vec<Value> = names.iter().map(|n| vm.m.alloc_str(n)).collect();
                    let l = new_list(vm, items);
                    ok(vm, l)
                }
                Err(e) => err(vm, &format!("{p}: {e}")),
            })
        },
        "Dir.create" => |vm, a| {
            let p = s(a[0])?.to_string();
            let recursive = arg(a, 1).is_some_and(|v| v.is_true());
            let r = blocking_io(vm, || if recursive { std::fs::create_dir_all(&p) } else { std::fs::create_dir(&p) });
            Ok(unit_result(vm, &p, r))
        },
        "Dir.remove" => |vm, a| {
            let p = s(a[0])?.to_string();
            let recursive = arg(a, 1).is_some_and(|v| v.is_true());
            let r = blocking_io(vm, || if recursive { std::fs::remove_dir_all(&p) } else { std::fs::remove_dir(&p) });
            Ok(unit_result(vm, &p, r))
        },
        "Dir.exists" => |vm, a| {
            let p = s(a[0])?.to_string();
            Ok(Value::bool(blocking_io(vm, move || std::path::Path::new(&p).is_dir())))
        },
        "Dir.current" => |vm, _| {
            let r = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(new_str(vm, &r))
        },

        "File.exists" => |vm, a| {
            let p = s(a[0])?.to_string();
            Ok(Value::bool(blocking_io(vm, move || std::path::Path::new(&p).exists())))
        },
        "File.metadata" => file_metadata,
        _ => return None,
    })
}

// ---- prelude instances -----------------------------------------------------------------------

/// Allocates an instance of the prelude class `class` with the given fields (others are nil).
/// Allocation never collects, so the field values need no rooting.
pub(crate) fn instance(vm: &mut Vm, class: &str, fields: &[(&str, Value)]) -> RtResult<Value> {
    let Some(id) = program::class_named(class) else {
        return panic(format!("internal error: prelude class {class} is not loaded"));
    };
    let cls = class_by_id(id);
    let v = vm.m.alloc_instance(cls);
    let o = unsafe { as_instance(v) };
    for &(name, fv) in fields {
        match cls.field_index(intern(name)) {
            Some(i) => o.set(i, fv),
            None => return panic(format!("internal error: {class} has no field {name}")),
        }
    }
    Ok(v)
}

/// The native data behind the `handle` field of a prelude instance.
pub(crate) fn handle_data<T: 'static>(v: Value, what: &str) -> RtResult<&'static T> {
    if kind_of(v) == ObjKind::Instance {
        let cls = class_of(v);
        if let Some(i) = cls.field_index(intern("handle")) {
            let h = unsafe { as_instance(v) }.get(i);
            if kind_of(h) == ObjKind::Native {
                // The handle is reachable from `v` (an argument), so it outlives this native call.
                let data: &'static NativeObj = unsafe { as_native(h) };
                if let Some(t) = (*data.data).as_any().downcast_ref::<T>() {
                    return Ok(t);
                }
            }
        }
    }
    panic(format!("TypeError: expected {what}, got {}", class_of(v).name))
}

fn unit_result(vm: &mut Vm, path: &str, r: std::io::Result<()>) -> Value {
    match r {
        Ok(()) => ok(vm, Value::NIL),
        Err(e) => err(vm, &format!("{path}: {e}")),
    }
}

// ---- Regex -----------------------------------------------------------------------------------

fn regex_of(v: Value) -> RtResult<&'static regex::Regex> {
    handle_data::<regex::Regex>(v, "Regex")
}

fn regex_new(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let pattern = s(a[0])?;
    match regex::Regex::new(pattern) {
        Ok(re) => {
            // The compiled regex is immutable and `Sync`: a frozen handle in a frozen instance.
            let h = vm.m.alloc_native_frozen(Box::new(re));
            let r = instance(vm, "Regex", &[("pattern", a[0]), ("handle", h)])?;
            ownership::freeze_deep(&vm.m, r)?;
            Ok(ok(vm, r))
        }
        Err(e) => Ok(err(vm, &e.to_string())),
    }
}

/// Converts byte offsets into character offsets, walking forward through ascending offsets.
struct CharPos<'a> {
    text: &'a str,
    byte: usize,
    chars: usize,
}

impl CharPos<'_> {
    fn at(&mut self, byte: usize) -> i64 {
        if byte < self.byte {
            self.byte = 0;
            self.chars = 0;
        }
        self.chars += self.text[self.byte..byte].chars().count();
        self.byte = byte;
        self.chars as i64
    }
}

fn make_match(vm: &mut Vm, re: &regex::Regex, caps: &regex::Captures, pos: &mut CharPos) -> RtResult<Value> {
    let whole = caps.get(0).expect("group 0");
    let start = pos.at(whole.start());
    let end = pos.at(whole.end());
    let groups: Vec<Value> = caps.iter().map(|g| g.map_or(Value::NIL, |g| vm.m.alloc_str(g.as_str()))).collect();
    let text = groups[0];
    let groups = new_list(vm, groups);
    let named = vm.m.alloc_dict(DictTable::new());
    for name in re.capture_names().flatten() {
        if let Some(g) = caps.name(name) {
            let k = new_str(vm, name);
            let v = new_str(vm, g.as_str());
            slow::dict_set(vm, named, k, v)?;
        }
    }
    instance(
        vm,
        "Match",
        &[
            ("text", text),
            ("start", Value::small_int(start)),
            ("end", Value::small_int(end)),
            ("groups", groups),
            ("named_groups", named),
        ],
    )
}

fn regex_find(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let re = regex_of(a[0])?;
    let text = s(a[1])?;
    let mut pos = CharPos { text, byte: 0, chars: 0 };
    match re.captures(text) {
        Some(c) => make_match(vm, re, &c, &mut pos),
        None => Ok(Value::NIL),
    }
}

fn regex_find_all(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let re = regex_of(a[0])?;
    let text = s(a[1])?;
    let mut pos = CharPos { text, byte: 0, chars: 0 };
    let mut out = Vec::new();
    for c in re.captures_iter(text) {
        out.push(make_match(vm, re, &c, &mut pos)?);
    }
    Ok(new_list(vm, out))
}

// ---- Process ---------------------------------------------------------------------------------

/// What `Process.run` and `Process.start` need, copied out of the heap before blocking.
struct CommandSpec {
    cmd: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    dir: Option<String>,
}

impl CommandSpec {
    fn from_args(vm: &mut Vm, a: &[Value]) -> RtResult<CommandSpec> {
        let cmd = s(a[0])?.to_string();
        let mut args = Vec::new();
        if let Some(l) = arg(a, 1) {
            for x in list_items(vm, l)? {
                args.push(s(x)?.to_string());
            }
        }
        let mut env = Vec::new();
        if let Some(d) = arg(a, 2) {
            if kind_of(d) != ObjKind::Dict {
                return panic(format!("TypeError: expected Dict, got {}", class_of(d).name));
            }
            let pairs: Vec<(Value, Value)> =
                ownership::read_dict(&vm.m, d, |t| t.iter().map(|e| (e.key, e.value)).collect())?;
            for (k, v) in pairs {
                env.push((s(k)?.to_string(), s(v)?.to_string()));
            }
        }
        let dir = match arg(a, 3) {
            Some(v) if !v.is_nil() => Some(s(v)?.to_string()),
            _ => None,
        };
        Ok(CommandSpec { cmd, args, env, dir })
    }

    fn command(&self) -> std::process::Command {
        let mut c = std::process::Command::new(&self.cmd);
        c.args(&self.args);
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(d) = &self.dir {
            c.current_dir(d);
        }
        c
    }
}

fn process_run(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    use std::process::Stdio;
    let spec = CommandSpec::from_args(vm, a)?;
    let cmd = spec.cmd.clone();
    let input = match arg(a, 4) {
        Some(v) if !v.is_nil() => Some(s(v)?.to_string()),
        _ => None,
    };
    let r = blocking_io(vm, move || -> std::io::Result<std::process::Output> {
        let mut c = spec.command();
        c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn()?;
        // Feed stdin from another thread so a child that writes before reading cannot deadlock.
        let feeder = match (input, child.stdin.take()) {
            (Some(text), Some(mut pipe)) => Some(std::thread::spawn(move || {
                let _ = pipe.write_all(text.as_bytes());
            })),
            _ => None,
        };
        let out = child.wait_with_output();
        if let Some(f) = feeder {
            let _ = f.join();
        }
        out
    });
    match r {
        Ok(out) => {
            let status = Value::small_int(out.status.code().map_or(-1, i64::from));
            let so = new_str(vm, &String::from_utf8_lossy(&out.stdout));
            let se = new_str(vm, &String::from_utf8_lossy(&out.stderr));
            let o = instance(vm, "Output", &[("status", status), ("stdout", so), ("stderr", se)])?;
            Ok(ok(vm, o))
        }
        Err(e) => {
            let msg = format!("{cmd}: {e}");
            Ok(err(vm, &msg))
        }
    }
}

/// A running child process. Each pipe has its own lock, so one fiber can write while another
/// reads; the process itself is polled (not waited on under the lock) so `kill` stays possible.
struct ChildProc {
    child: Mutex<std::process::Child>,
    stdin: Mutex<Option<std::process::ChildStdin>>,
    stdout: Mutex<Option<std::io::BufReader<std::process::ChildStdout>>>,
}

impl Drop for ChildProc {
    fn drop(&mut self) {
        // Reap the process if it has already exited (finalizers must not block).
        let _ = self.child.get_mut().try_wait();
    }
}

struct ChildHandle(Arc<ChildProc>);

fn child_of(v: Value) -> RtResult<Arc<ChildProc>> {
    Ok(handle_data::<ChildHandle>(v, "Subprocess")?.0.clone())
}

fn process_start(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    use std::process::Stdio;
    let spec = CommandSpec::from_args(vm, a)?;
    let cmd = spec.cmd.clone();
    let r = blocking_io(vm, move || {
        let mut c = spec.command();
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        c.spawn()
    });
    match r {
        Ok(mut child) => {
            let pid = Value::small_int(child.id() as i64);
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().map(std::io::BufReader::new);
            let proc = ChildProc { child: Mutex::new(child), stdin: Mutex::new(stdin), stdout: Mutex::new(stdout) };
            let h = vm.m.alloc_native_frozen(Box::new(ChildHandle(Arc::new(proc))));
            let c = instance(vm, "Subprocess", &[("pid", pid), ("handle", h)])?;
            Ok(ok(vm, c))
        }
        Err(e) => {
            let msg = format!("{cmd}: {e}");
            Ok(err(vm, &msg))
        }
    }
}

fn child_write(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = child_of(a[0])?;
    let text = s(a[1])?.to_string();
    let r = blocking_io(vm, move || match c.stdin.lock().as_mut() {
        Some(p) => p.write_all(text.as_bytes()).and_then(|_| p.flush()),
        None => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin is closed")),
    });
    Ok(match r {
        Ok(()) => ok(vm, Value::NIL),
        Err(e) => err(vm, &e.to_string()),
    })
}

fn child_read_line(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = child_of(a[0])?;
    let line = blocking_io(vm, move || {
        let mut g = c.stdout.lock();
        let r = g.as_mut()?;
        let mut buf = String::new();
        match r.read_line(&mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(buf),
        }
    });
    Ok(match line {
        Some(mut l) => {
            if l.ends_with('\n') {
                l.pop();
                if l.ends_with('\r') {
                    l.pop();
                }
            }
            new_str(vm, &l)
        }
        None => Value::NIL,
    })
}

fn child_read_all(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = child_of(a[0])?;
    let text = blocking_io(vm, move || {
        let mut buf = Vec::new();
        if let Some(r) = c.stdout.lock().as_mut() {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    Ok(new_str(vm, &text))
}

fn child_wait(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = child_of(a[0])?;
    let r = blocking_io(vm, move || -> std::io::Result<std::process::ExitStatus> {
        // The child may be waiting for input that will never come.
        c.stdin.lock().take();
        let mut delay = std::time::Duration::from_millis(1);
        loop {
            if let Some(st) = c.child.lock().try_wait()? {
                return Ok(st);
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(std::time::Duration::from_millis(20));
        }
    });
    Ok(match r {
        Ok(st) => {
            let code = Value::small_int(st.code().map_or(-1, i64::from));
            ok(vm, code)
        }
        Err(e) => err(vm, &e.to_string()),
    })
}

// ---- Paths and metadata ----------------------------------------------------------------------

/// Joins with `/` (accepted by every supported OS); an absolute `part` replaces `base`.
fn path_join(base: &str, part: &str) -> String {
    if base.is_empty() || std::path::Path::new(part).is_absolute() || part.starts_with(['/', '\\']) {
        return part.to_string();
    }
    if part.is_empty() || base.ends_with(['/', '\\']) {
        return format!("{base}{part}");
    }
    format!("{base}/{part}")
}

fn file_metadata(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let p = s(a[0])?.to_string();
    let r = blocking_io(vm, || {
        std::fs::metadata(&p).map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());
            (m.len(), m.is_dir(), m.is_file(), modified)
        })
    });
    match r {
        Ok((size, is_dir, is_file, modified)) => {
            let size = vm.m.int(size as i64);
            let md = instance(
                vm,
                "Metadata",
                &[
                    ("size", size),
                    ("is_dir", Value::bool(is_dir)),
                    ("is_file", Value::bool(is_file)),
                    ("modified", Value::from_f64(modified)),
                ],
            )?;
            Ok(ok(vm, md))
        }
        Err(e) => Ok(err(vm, &format!("{p}: {e}"))),
    }
}
