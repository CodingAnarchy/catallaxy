//! Standard output of `catx run` at its edges: a reader that goes away, a terminal, and signals.
//! Each case runs the program in a child process.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const CATX: &str = env!("CARGO_BIN_EXE_catx");

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("catx-output-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Waits up to `limit` for `child`; kills it (and fails) if it is still running.
fn wait_within(child: &mut Child, limit: Duration, what: &str) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > limit {
            child.kill().ok();
            child.wait().ok();
            panic!("{what}: still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn broken_pipe_ends_the_program_with_141() {
    let dir = scratch_dir("pipe");
    let prog = dir.join("spin.cx");
    std::fs::write(&prog, "i = 0\nloop {\n  puts(i)\n  i += 1\n}\n").unwrap();
    let mut child =
        Command::new(CATX).arg("run").arg(&prog).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // Read a little, then go away like `head -2`.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 16];
    stdout.read_exact(&mut buf).unwrap();
    assert!(buf.starts_with(b"0\n1\n"), "{buf:?}");
    drop(stdout);
    let status = wait_within(&mut child, Duration::from_secs(60), "writer to a closed pipe");
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert_eq!(status.code(), Some(141), "{err}");
    assert!(err.is_empty(), "a broken pipe ends the program quietly: {err}");

    // Output written at the end, to a reader that has already gone, is a broken pipe too.
    let prog = dir.join("short.cx");
    std::fs::write(&prog, "puts(1)\nputs(2)\n").unwrap();
    let mut child = Command::new(CATX).arg("run").arg(&prog).stdout(Stdio::piped()).spawn().unwrap();
    drop(child.stdout.take());
    let status = wait_within(&mut child, Duration::from_secs(60), "short program");
    assert_eq!(status.code(), Some(141));
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;

    /// The output printed before a signal is written out, and the signal still ends the process.
    #[test]
    fn signals_flush_output_and_keep_the_status() {
        let dir = scratch_dir("signals");
        for sig in [libc::SIGINT, libc::SIGTERM] {
            let marker = dir.join(format!("started-{sig}"));
            let prog = dir.join("prog.cx");
            // Writing the marker flushes nothing yet; the ten lines after it stay in the buffer.
            let src = format!(
                "File.write({:?}, \"\").unwrap()\nfor i in 1..10 {{\n  puts(\"line #{{i}}\")\n}}\nloop {{\n}}\n",
                marker.to_str().unwrap()
            );
            std::fs::write(&prog, src).unwrap();
            let out_path = dir.join(format!("out-{sig}.txt"));
            let out = std::fs::File::create(&out_path).unwrap();
            let mut child = Command::new(CATX).arg("run").arg(&prog).stdout(out).spawn().unwrap();
            let start = Instant::now();
            while !marker.exists() {
                assert!(start.elapsed() < Duration::from_secs(60), "program did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(300));
            // SAFETY: signals our own child.
            unsafe { libc::kill(child.id() as libc::pid_t, sig) };
            let status = wait_within(&mut child, Duration::from_secs(30), "signalled program");
            assert_eq!(status.signal(), Some(sig), "{status:?}");
            let got = std::fs::read_to_string(&out_path).unwrap();
            assert_eq!(got.lines().count(), 10, "signal {sig}: {got:?}");
            assert!(got.ends_with("line 10\n"), "{got:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Opens a pseudo-terminal; returns the controller and the device's path.
    fn open_pty() -> (OwnedFd, PathBuf) {
        // SAFETY: the POSIX pseudo-terminal calls, checked for failure.
        unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0, "posix_openpt");
            assert_eq!(libc::grantpt(fd), 0, "grantpt");
            assert_eq!(libc::unlockpt(fd), 0, "unlockpt");
            let name = libc::ptsname(fd);
            assert!(!name.is_null(), "ptsname");
            let path = std::ffi::CStr::from_ptr(name).to_str().unwrap().to_string();
            (OwnedFd::from_raw_fd(fd), PathBuf::from(path))
        }
    }

    /// On a terminal each line appears when it is printed, not when the program ends.
    #[test]
    fn terminal_output_is_line_buffered() {
        let dir = scratch_dir("tty");
        let prog = dir.join("progress.cx");
        std::fs::write(&prog, "puts(\"started\")\nloop {\n}\n").unwrap();
        let (controller, device) = open_pty();
        let tty = std::fs::OpenOptions::new().read(true).write(true).open(&device).unwrap();
        let mut child = Command::new(CATX).arg("run").arg(&prog).stdout(tty).spawn().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut reader = std::fs::File::from(controller);
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 256];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(7).any(|w| w == b"started") {
                    tx.send(()).ok();
                    break;
                }
            }
        });
        let shown = rx.recv_timeout(Duration::from_secs(60));
        child.kill().ok();
        child.wait().ok();
        assert!(shown.is_ok(), "a line printed to a terminal did not appear while the program ran");
        std::fs::remove_dir_all(&dir).ok();
    }
}
