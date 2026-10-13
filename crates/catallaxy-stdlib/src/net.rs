//! Networking: TCP sockets and listeners, and an HTTP client.
//!
//! Sockets are non-blocking and registered with one process-wide reactor (a `mio` poller on its
//! own thread). An operation that would block registers a waker for its socket and waits: a
//! fiber parks (so one OS thread can serve thousands of connections), any other context blocks
//! its thread in the native state. The reactor wakes every waiter of a socket when it becomes
//! ready; waiters then retry. Registration happens before a final retry, so readiness that
//! arrives in between is never lost (the reactor is edge-triggered).
//!
//! The HTTP client (`ureq`, with TLS) runs on the fibers' I/O pool: concurrent requests are
//! bounded by the pool size, which suits a client.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use catallaxy_runtime::dict::DictTable;
use catallaxy_runtime::object::Wake;
use catallaxy_runtime::{RtResult, Value};
use catallaxy_vm::{NativeFn, Vm};
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Registry, Token};
use parking_lot::{Condvar, Mutex};

use crate::fiber::{blocking_io, wait_event_until};
use crate::script::{handle_data, instance};
use crate::*;

// ---- the reactor -------------------------------------------------------------------------------

struct Reactor {
    registry: Registry,
    waiters: Mutex<HashMap<usize, Vec<Wake>>>,
    /// Sockets with a connect in flight, and whether the reactor has seen an event for them.
    connecting: Mutex<HashMap<usize, bool>>,
    next: AtomicUsize,
}

static REACTOR: LazyLock<Reactor> = LazyLock::new(|| {
    let poll = Poll::new().expect("create the network poller");
    let registry = poll.registry().try_clone().expect("clone the poller registry");
    std::thread::Builder::new()
        .name("catx-net".into())
        .spawn(move || reactor_main(poll))
        .expect("spawn the network reactor");
    Reactor {
        registry,
        waiters: Mutex::new(HashMap::new()),
        connecting: Mutex::new(HashMap::new()),
        next: AtomicUsize::new(1),
    }
});

fn reactor_main(mut poll: Poll) {
    let mut events = Events::with_capacity(1024);
    loop {
        if poll.poll(&mut events, None).is_err() {
            continue;
        }
        for ev in events.iter() {
            // Recorded before the waiters are taken: see `wait_connected`.
            if let Some(fired) = REACTOR.connecting.lock().get_mut(&ev.token().0) {
                *fired = true;
            }
            let ws = REACTOR.waiters.lock().remove(&ev.token().0);
            for w in ws.into_iter().flatten() {
                w();
            }
        }
    }
}

fn new_token() -> Token {
    Token(REACTOR.next.fetch_add(1, Ordering::Relaxed))
}

/// Runs `op` until it stops returning `WouldBlock`, waiting for readiness of `token` in between.
fn io_wait<T>(
    vm: &mut Vm,
    token: Token,
    deadline: Option<Instant>,
    mut op: impl FnMut() -> std::io::Result<T>,
) -> Result<T, String> {
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err("timed out".into());
        }
        // Register, then retry once: readiness that arrived after the failed attempt but before
        // registration would otherwise be missed.
        let mut early: Option<std::io::Result<T>> = None;
        let mut arm = |wake: Wake| {
            REACTOR.waiters.lock().entry(token.0).or_default().push(wake);
            match op() {
                Err(e) if e.kind() == ErrorKind::WouldBlock => true,
                r => {
                    early = Some(r);
                    false
                }
            }
        };
        match wait_event_until(vm, deadline, &mut arm) {
            Some(_) => {}
            None => {
                // No fibers to run: block this thread until the reactor (or the deadline) wakes it.
                let pair = Arc::new((Mutex::new(false), Condvar::new()));
                let p2 = pair.clone();
                let wake: Wake = Arc::new(move || {
                    *p2.0.lock() = true;
                    p2.1.notify_all();
                });
                if arm(wake) {
                    blocking(vm, || {
                        let mut fired = pair.0.lock();
                        while !*fired {
                            match deadline {
                                Some(d) => {
                                    let now = Instant::now();
                                    if now >= d || pair.1.wait_for(&mut fired, d - now).timed_out() {
                                        break;
                                    }
                                }
                                None => pair.1.wait(&mut fired),
                            }
                        }
                    });
                }
            }
        }
        if let Some(r) = early {
            match r {
                Ok(v) => return Ok(v),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

// ---- sockets -----------------------------------------------------------------------------------

struct Conn {
    stream: Mutex<Option<TcpStream>>,
    token: Token,
    timeout: Mutex<Option<Duration>>,
    /// Bytes read past the last line returned by `read_line`.
    pending: Mutex<Vec<u8>>,
}

struct Listen {
    listener: Mutex<Option<TcpListener>>,
    token: Token,
    port: u16,
}

fn conn_of(v: Value) -> RtResult<&'static Conn> {
    handle_data::<Conn>(v, "Socket")
}
fn listen_of(v: Value) -> RtResult<&'static Listen> {
    handle_data::<Listen>(v, "Listener")
}

fn deadline(c: &Conn) -> Option<Instant> {
    c.timeout.lock().map(|d| Instant::now() + d)
}

fn new_socket(vm: &mut Vm, mut stream: TcpStream) -> RtResult<Value> {
    let token = new_token();
    if let Err(e) = REACTOR.registry.register(&mut stream, token, Interest::READABLE | Interest::WRITABLE) {
        return Ok(err(vm, &format!("cannot watch socket: {e}")));
    }
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let conn =
        Conn { stream: Mutex::new(Some(stream)), token, timeout: Mutex::new(None), pending: Mutex::new(Vec::new()) };
    let h = vm.m.alloc_native(Box::new(conn));
    let p = new_str(vm, &peer);
    let sock = instance(vm, "Socket", &[("handle", h), ("peer", p)])?;
    Ok(ok(vm, sock))
}

fn with_stream<R>(c: &Conn, f: impl FnOnce(&mut TcpStream) -> std::io::Result<R>) -> std::io::Result<R> {
    match c.stream.lock().as_mut() {
        Some(s) => f(s),
        None => Err(std::io::Error::new(ErrorKind::NotConnected, "socket is closed")),
    }
}

fn connect(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let (host, port) = (s(a[0])?.to_string(), int(a[1])?);
    let timeout = arg(a, 2).map(float).transpose()?.map(Duration::from_secs_f64);
    if !(0..=65535).contains(&port) {
        return Ok(err(vm, &format!("invalid port {port}")));
    }
    // Name resolution blocks: run it on the I/O pool.
    let addrs = blocking_io(vm, move || (host.as_str(), port as u16).to_socket_addrs().map(|a| a.collect::<Vec<_>>()));
    let addrs = match addrs {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => return Ok(err(vm, "no address found")),
        Err(e) => return Ok(err(vm, &e.to_string())),
    };
    let deadline = timeout.map(|d| Instant::now() + d);
    let mut last = String::new();
    for addr in addrs {
        let mut stream = match TcpStream::connect(addr) {
            Ok(s) => s,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        let token = new_token();
        match wait_connected(vm, &mut stream, token, deadline) {
            Ok(()) => {
                let _ = REACTOR.registry.deregister(&mut stream);
                return new_socket(vm, stream);
            }
            Err(e) => {
                let _ = REACTOR.registry.deregister(&mut stream);
                last = e;
            }
        }
    }
    Ok(err(vm, &last))
}

/// Waits for a non-blocking connect to finish. A connect has finished when its socket reports
/// readiness (`take_error` then tells failure from success); `peer_addr` cannot decide, since on
/// Windows it can succeed while the connect is still in flight. Edge-triggered readiness arrives
/// once, so the reactor records it in `connecting` rather than leaving it to be inferred from
/// which wakers remain: a waker armed after the only event would otherwise wait forever.
fn wait_connected(vm: &mut Vm, stream: &mut TcpStream, token: Token, deadline: Option<Instant>) -> Result<(), String> {
    REACTOR.connecting.lock().insert(token.0, false);
    let r = wait_connected_inner(vm, stream, token, deadline);
    REACTOR.connecting.lock().remove(&token.0);
    REACTOR.waiters.lock().remove(&token.0);
    r
}

fn wait_connected_inner(
    vm: &mut Vm,
    stream: &mut TcpStream,
    token: Token,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let fired = || REACTOR.connecting.lock().get(&token.0).copied().unwrap_or(false);
    if let Err(e) = REACTOR.registry.register(stream, token, Interest::READABLE | Interest::WRITABLE) {
        return Err(format!("cannot watch socket: {e}"));
    }
    loop {
        // The waker goes in before `fired` is checked; the reactor sets `fired` before taking the
        // wakers, so an event either shows in the check or finds the waker.
        wait_wake(vm, deadline, |wake| {
            REACTOR.waiters.lock().entry(token.0).or_default().push(wake);
            !fired()
        });
        match stream.take_error() {
            Ok(Some(e)) | Err(e) => return Err(e.to_string()),
            Ok(None) => {}
        }
        if fired() {
            return Ok(());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err("timed out".into());
        }
    }
}

/// Waits until a waker registered by `arm` fires or `until` passes. `arm` registers the waker and
/// returns false if waiting is not needed after all.
fn wait_wake(vm: &mut Vm, until: Option<Instant>, arm: impl FnOnce(Wake) -> bool) {
    let mut arm = Some(arm);
    if wait_event_until(vm, until, |w| (arm.take().unwrap())(w)).is_some() {
        return;
    }
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let p2 = pair.clone();
    let wake: Wake = Arc::new(move || {
        *p2.0.lock() = true;
        p2.1.notify_all();
    });
    if !(arm.take().unwrap())(wake) {
        return;
    }
    blocking(vm, || {
        let mut fired = pair.0.lock();
        while !*fired {
            match until {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d || pair.1.wait_for(&mut fired, d - now).timed_out() {
                        break;
                    }
                }
                None => pair.1.wait(&mut fired),
            }
        }
    });
}

fn read(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = conn_of(a[0])?;
    let max = arg(a, 1).map(int).transpose()?.unwrap_or(65536).clamp(1, 1 << 24) as usize;
    {
        let mut p = c.pending.lock();
        if !p.is_empty() {
            let n = p.len().min(max);
            let out: Vec<u8> = p.drain(..n).collect();
            drop(p);
            let v = new_str(vm, &String::from_utf8_lossy(&out));
            return Ok(ok(vm, v));
        }
    }
    let mut buf = vec![0u8; max];
    match io_wait(vm, c.token, deadline(c), || with_stream(c, |s| s.read(&mut buf))) {
        Ok(n) => {
            let v = new_str(vm, &String::from_utf8_lossy(&buf[..n]));
            Ok(ok(vm, v))
        }
        Err(e) => Ok(err(vm, &e)),
    }
}

fn read_line(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = conn_of(a[0])?;
    let dl = deadline(c);
    loop {
        {
            let mut p = c.pending.lock();
            if let Some(i) = p.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = p.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                drop(p);
                let v = new_str(vm, &String::from_utf8_lossy(&line));
                return Ok(ok(vm, v));
            }
        }
        let mut buf = vec![0u8; 8192];
        match io_wait(vm, c.token, dl, || with_stream(c, |s| s.read(&mut buf))) {
            Ok(0) => {
                // End of stream: the rest, if any, then nil.
                let rest: Vec<u8> = std::mem::take(&mut *c.pending.lock());
                if rest.is_empty() {
                    return Ok(ok(vm, Value::NIL));
                }
                let v = new_str(vm, &String::from_utf8_lossy(&rest));
                return Ok(ok(vm, v));
            }
            Ok(n) => c.pending.lock().extend_from_slice(&buf[..n]),
            Err(e) => return Ok(err(vm, &e)),
        }
    }
}

fn write(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = conn_of(a[0])?;
    let data = s(a[1])?.as_bytes().to_vec();
    let dl = deadline(c);
    let mut off = 0;
    while off < data.len() {
        match io_wait(vm, c.token, dl, || with_stream(c, |s| s.write(&data[off..]))) {
            Ok(0) => return Ok(err(vm, "connection closed")),
            Ok(n) => off += n,
            Err(e) => return Ok(err(vm, &e)),
        }
    }
    Ok(ok(vm, Value::small_int(data.len() as i64)))
}

fn close(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = conn_of(a[0])?;
    if let Some(mut s) = c.stream.lock().take() {
        let _ = REACTOR.registry.deregister(&mut s);
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    REACTOR.waiters.lock().remove(&c.token.0).into_iter().flatten().for_each(|w| w());
    Ok(Value::NIL)
}

fn set_timeout(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let c = conn_of(a[0])?;
    *c.timeout.lock() = arg(a, 1).filter(|v| !v.is_nil()).map(float).transpose()?.map(Duration::from_secs_f64);
    Ok(Value::NIL)
}

// ---- listeners ---------------------------------------------------------------------------------

fn listen(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let (host, port) = (s(a[0])?.to_string(), int(a[1])?);
    if !(0..=65535).contains(&port) {
        return Ok(err(vm, &format!("invalid port {port}")));
    }
    let addr = match blocking_io(vm, move || (host.as_str(), port as u16).to_socket_addrs().map(|mut a| a.next())) {
        Ok(Some(a)) => a,
        Ok(None) => return Ok(err(vm, "no address found")),
        Err(e) => return Ok(err(vm, &e.to_string())),
    };
    let mut l = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => return Ok(err(vm, &format!("{addr}: {e}"))),
    };
    let token = new_token();
    if let Err(e) = REACTOR.registry.register(&mut l, token, Interest::READABLE) {
        return Ok(err(vm, &format!("cannot watch listener: {e}")));
    }
    let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
    let h = vm.m.alloc_native(Box::new(Listen { listener: Mutex::new(Some(l)), token, port }));
    let lst = instance(vm, "Listener", &[("handle", h), ("port", Value::small_int(port as i64))])?;
    Ok(ok(vm, lst))
}

fn accept(vm: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let l = listen_of(a[0])?;
    let r = io_wait(vm, l.token, None, || match l.listener.lock().as_ref() {
        Some(l) => l.accept(),
        None => Err(std::io::Error::new(ErrorKind::NotConnected, "listener is closed")),
    });
    match r {
        Ok((stream, _)) => new_socket(vm, stream),
        Err(e) => Ok(err(vm, &e)),
    }
}

fn close_listener(_: &mut Vm, a: &[Value]) -> RtResult<Value> {
    let l = listen_of(a[0])?;
    let _ = l.port;
    if let Some(mut s) = l.listener.lock().take() {
        let _ = REACTOR.registry.deregister(&mut s);
    }
    REACTOR.waiters.lock().remove(&l.token.0).into_iter().flatten().for_each(|w| w());
    Ok(Value::NIL)
}

// ---- HTTP --------------------------------------------------------------------------------------

struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn headers_arg(vm: &Vm, v: Option<Value>) -> RtResult<Vec<(String, String)>> {
    let Some(d) = v.filter(|v| !v.is_nil()) else { return Ok(Vec::new()) };
    let pairs =
        catallaxy_runtime::ownership::read_dict(&vm.m, d, |t| t.iter().map(|e| (e.key, e.value)).collect::<Vec<_>>())?;
    let mut out = Vec::new();
    for (k, v) in pairs {
        out.push((s(k)?.to_string(), s(v)?.to_string()));
    }
    Ok(out)
}

fn http(
    vm: &mut Vm,
    method: &'static str,
    url: String,
    body: Option<String>,
    headers: Vec<(String, String)>,
) -> RtResult<Value> {
    let reply = blocking_io(vm, move || -> Result<HttpReply, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let mut resp = match (method, body) {
            ("GET", _) => {
                let mut r = agent.get(&url);
                for (k, v) in &headers {
                    r = r.header(k, v);
                }
                r.call()
            }
            (_, body) => {
                let mut r = agent.post(&url);
                for (k, v) in &headers {
                    r = r.header(k, v);
                }
                r.send(body.unwrap_or_default())
            }
        }
        .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        Ok(HttpReply { status, headers, body })
    });
    let r = match reply {
        Ok(r) => r,
        Err(e) => return Ok(err(vm, &e)),
    };
    let d = vm.m.alloc_dict(DictTable::new());
    vm.extra_roots().push(d);
    for (k, v) in &r.headers {
        let (kv, vv) = (new_str(vm, k), new_str(vm, v));
        catallaxy_vm::slow::dict_set(vm, d, kv, vv)?;
    }
    let body = new_str(vm, &r.body);
    let resp =
        instance(vm, "HttpResponse", &[("status", Value::small_int(r.status as i64)), ("headers", d), ("body", body)]);
    vm.extra_roots().pop();
    let resp = resp?;
    Ok(ok(vm, resp))
}

pub fn lookup(name: &str) -> Option<NativeFn> {
    Some(match name {
        "Net.connect" => connect,
        "Net.listen" => listen,
        "Socket.read" => read,
        "Socket.read_line" => read_line,
        "Socket.write" => write,
        "Socket.close" => close,
        "Socket.set_timeout" => set_timeout,
        "Listener.accept" => accept,
        "Listener.close" => close_listener,
        "Http.get" => |vm, a| {
            let url = s(a[0])?.to_string();
            let h = headers_arg(vm, arg(a, 1))?;
            http(vm, "GET", url, None, h)
        },
        "Http.post" => |vm, a| {
            let (url, body) = (s(a[0])?.to_string(), s(a[1])?.to_string());
            let h = headers_arg(vm, arg(a, 2))?;
            http(vm, "POST", url, Some(body), h)
        },
        _ => return None,
    })
}
