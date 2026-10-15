//! Buffered standard output.
//!
//! `puts` and `print` write to a 64 KB buffer, flushed when it fills, at exit, before reading
//! input or blocking, and before writing to standard error. When standard output is a terminal
//! the buffer is also flushed at the end of every line, so progress output appears as it is
//! printed.
//!
//! When the reader of standard output goes away (`catx run prog.cx | head -2`), the next write
//! fails with a broken pipe, and the program exits quietly with status 141 (128 + `SIGPIPE`, what
//! a shell reports for a process killed by `SIGPIPE`). An interrupt or termination signal
//! (`SIGINT`, `SIGTERM`, `SIGHUP`; Ctrl-C or Ctrl-Break on Windows) flushes the buffer before the
//! process dies of the signal, so its exit status is unchanged and no output is lost.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;

/// The exit status of a program whose standard output was closed by its reader.
pub const BROKEN_PIPE_STATUS: i32 = 141;

static STDOUT: Mutex<Option<std::io::BufWriter<std::io::Stdout>>> = Mutex::new(None);

/// Whether standard output is a terminal (output is then flushed line by line).
static LINE_BUFFERED: OnceLock<bool> = OnceLock::new();

/// Set once a write or flush finds standard output closed by its reader.
static BROKEN: AtomicBool = AtomicBool::new(false);

/// Writes to the buffered standard output. Exits with [`BROKEN_PIPE_STATUS`] if its reader
/// has gone away.
pub fn write_out(s: &str) {
    if BROKEN.load(Ordering::Relaxed) {
        broken_pipe_exit();
    }
    let mut g = STDOUT.lock();
    let w = g.get_or_insert_with(|| std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()));
    let mut r = w.write_all(s.as_bytes());
    if r.is_ok() && *LINE_BUFFERED.get_or_init(|| std::io::stdout().is_terminal()) && s.contains('\n') {
        r = w.flush();
    }
    if r.is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
        broken_pipe_exit();
    }
}

/// Flushes buffered standard output (at exit, before reading input, before errors). A broken
/// pipe is remembered, not reported: the program exits at its next write (see [`write_out`]),
/// and [`stdout_broken`] tells the end of the program.
pub fn flush() {
    flush_locked(&mut STDOUT.lock());
}

fn flush_locked(g: &mut Option<std::io::BufWriter<std::io::Stdout>>) {
    if let Some(w) = g.as_mut() {
        if w.flush().is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
            BROKEN.store(true, Ordering::Relaxed);
            // Drop what could not be written, so it is not tried again.
            *g = None;
        }
    }
}

/// Whether standard output's reader has gone away; a program that ends normally then exits
/// with [`BROKEN_PIPE_STATUS`].
pub fn stdout_broken() -> bool {
    BROKEN.load(Ordering::Relaxed)
}

fn broken_pipe_exit() -> ! {
    std::process::exit(BROKEN_PIPE_STATUS)
}

/// Flushes from a signal's watcher thread. Another thread may hold the buffer in the middle of a
/// write; wait for it a little, but never hang a dying process on a blocked writer.
fn flush_for_signal() {
    if let Some(mut g) = STDOUT.try_lock_for(Duration::from_millis(500)) {
        flush_locked(&mut g);
    }
}

/// Flushes buffered output when the process is interrupted or terminated, then lets the signal
/// take its default course (so the exit status still says which signal ended the program).
/// Signals that were ignored when the program started (`nohup`, background jobs) stay ignored.
/// For `catx run`; an embedding host keeps its own signal handling.
pub fn install_signal_flush() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(sys::install);
}

#[cfg(unix)]
mod sys {
    use std::sync::atomic::{AtomicI32, Ordering};

    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// The write end of the self-pipe the handler reports signals on.
    static PIPE_W: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_signal(sig: libc::c_int) {
        // Only async-signal-safe calls here: hand the signal to the watcher thread.
        let b = sig as u8;
        // SAFETY: `write` is async-signal-safe; a full pipe already holds a pending signal.
        unsafe { libc::write(PIPE_W.load(Ordering::Relaxed), &b as *const u8 as *const libc::c_void, 1) };
    }

    pub(super) fn install() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: plain system calls on descriptors this function owns.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return;
            }
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        let (r, w) = (fds[0], fds[1]);
        PIPE_W.store(w, Ordering::Relaxed);
        let spawned = std::thread::Builder::new().name("catx-signals".into()).spawn(move || loop {
            let mut b = 0u8;
            // SAFETY: reads one byte into a local.
            let n = unsafe { libc::read(r, &mut b as *mut u8 as *mut libc::c_void, 1) };
            if n == 1 {
                super::flush_for_signal();
                let sig = b as libc::c_int;
                // SAFETY: restores the default action and re-raises, which ends the process.
                unsafe {
                    libc::signal(sig, libc::SIG_DFL);
                    libc::raise(sig);
                }
                // The signal was blocked somehow: exit as a shell reports a death by it.
                std::process::exit(128 + sig);
            } else if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            } else {
                return;
            }
        });
        if spawned.is_err() {
            return;
        }
        for sig in SIGNALS {
            // SAFETY: installs a handler that only writes to a pipe.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 || old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                act.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut act.sa_mask);
                libc::sigaction(sig, &act, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    /// Runs on a thread of its own when Ctrl-C, Ctrl-Break or a console close arrives. Returning
    /// false hands the event to the default handler, which ends the process.
    unsafe extern "system" fn on_ctrl(_event: u32) -> i32 {
        super::flush_for_signal();
        0
    }

    pub(super) fn install() {
        // SAFETY: registers a handler with the signature the API expects.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    pub(super) fn install() {}
}
