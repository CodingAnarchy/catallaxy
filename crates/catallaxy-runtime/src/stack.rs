//! The native stack guard.
//!
//! Calls between bytecode functions do not use the native stack, but natives that call back into
//! the VM (`Mutex#lock`, `Atomic#update`, `par_map`, a user `==` called by a dict lookup) and the
//! structural operations in [`crate::ops`] do. [`check`] compares the stack pointer with a limit
//! kept per thread, and turns a native recursion that would run off the end of the stack into a
//! `SystemStackError` panic, which the program can catch, instead of a crash.
//!
//! The limit sits [`margin`] above the lowest usable address of the running stack. The margin
//! covers what native code may use below the last check: one interpreter frame and the deepest
//! native that does not check (formatting, JSON, regex, a collection), with room to spare.
//!
//! A thread's bounds are read from the OS the first time it checks (or when it starts, through
//! [`init_thread`]). A fiber runs on its own stack: the scheduler swaps the limit with
//! [`set_limit`] around every switch to it. Where the bounds are unknown (other platforms, Miri)
//! the check never fires.

use std::cell::Cell;

use crate::mutator::{RtResult, Trap};

thread_local! {
    /// The lowest stack address a check accepts. 0: not read yet; 1: unknown (never fires).
    static LIMIT: Cell<usize> = const { Cell::new(0) };
}

/// The panic raised when the stack runs out.
pub const OVERFLOW: &str = "SystemStackError: stack level too deep";

/// How much of a stack of `size` bytes is kept back below the limit: a quarter, at most
/// 256 KB (64 KB of a fiber's 256 KB stack; 256 KB of a thread's).
pub fn margin(size: usize) -> usize {
    (size / 4).min(256 << 10)
}

/// The limit for a stack spanning `lo..hi` (its lowest address may include a guard page).
pub fn limit_for(lo: usize, hi: usize) -> usize {
    lo + margin(hi.saturating_sub(lo))
}

/// An address in the current stack frame.
#[inline(always)]
fn sp() -> usize {
    let probe = 0u8;
    std::hint::black_box(&probe) as *const u8 as usize
}

/// Raises `SystemStackError` if the native stack is within the margin of its end.
#[inline]
pub fn check() -> RtResult<()> {
    let lim = LIMIT.with(|l| l.get());
    if sp() < lim || lim == 0 {
        return check_slow();
    }
    Ok(())
}

/// As [`check`], for a native about to use up to `extra` bytes of stack beyond the margin
/// without checking (a bounded recursion such as parsing JSON).
pub fn check_extra(extra: usize) -> RtResult<()> {
    let mut lim = LIMIT.with(|l| l.get());
    if lim == 0 {
        lim = init_thread();
    }
    if lim > 1 && sp() < lim.saturating_add(extra) {
        return Err(Trap::Panic(OVERFLOW.into()));
    }
    Ok(())
}

#[cold]
#[inline(never)]
fn check_slow() -> RtResult<()> {
    let mut lim = LIMIT.with(|l| l.get());
    if lim == 0 {
        lim = init_thread();
    }
    if sp() < lim {
        return Err(Trap::Panic(OVERFLOW.into()));
    }
    Ok(())
}

/// Reads the current thread's stack bounds from the OS and sets its limit, which it returns.
pub fn init_thread() -> usize {
    let lim = match thread_bounds() {
        Some((lo, hi)) if lo < hi => limit_for(lo, hi),
        _ => 1,
    };
    LIMIT.with(|l| l.set(lim));
    lim
}

/// Replaces the current limit, returning the previous one (for switching to a fiber's stack
/// and back).
pub fn set_limit(lim: usize) -> usize {
    LIMIT.with(|l| l.replace(lim))
}

/// The current thread's stack as `(lowest, highest)` address.
#[cfg(all(not(miri), any(target_os = "linux", target_os = "android")))]
fn thread_bounds() -> Option<(usize, usize)> {
    // SAFETY: plain queries about the calling thread; the attribute object is initialized by
    // `pthread_getattr_np` and destroyed here.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let r = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        (r == 0 && !addr.is_null()).then(|| (addr as usize, addr as usize + size))
    }
}

#[cfg(all(not(miri), any(target_os = "macos", target_os = "ios")))]
fn thread_bounds() -> Option<(usize, usize)> {
    // SAFETY: plain queries about the calling thread.
    unsafe {
        let t = libc::pthread_self();
        let hi = libc::pthread_get_stackaddr_np(t) as usize;
        let size = libc::pthread_get_stacksize_np(t);
        (hi > size).then(|| (hi - size, hi))
    }
}

#[cfg(all(not(miri), windows))]
fn thread_bounds() -> Option<(usize, usize)> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let (mut lo, mut hi) = (0usize, 0usize);
    // SAFETY: writes the calling thread's stack bounds to the two locals.
    unsafe { GetCurrentThreadStackLimits(&mut lo, &mut hi) };
    Some((lo, hi))
}

#[cfg(any(
    miri,
    not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios", windows))
))]
fn thread_bounds() -> Option<(usize, usize)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shallow_check_passes() {
        assert!(check().is_ok());
    }

    #[test]
    fn limit_above_the_stack_fires() {
        let prev = set_limit(usize::MAX);
        let r = check();
        set_limit(prev);
        assert!(matches!(r, Err(Trap::Panic(m)) if m == OVERFLOW));
    }

    #[cfg(not(miri))]
    #[test]
    fn deep_native_recursion_is_caught() {
        fn down(n: u64) -> RtResult<u64> {
            check()?;
            let pad = std::hint::black_box([n; 32]);
            Ok(down(n + 1)? + pad[0])
        }
        // Run on a small thread so the test is quick.
        let r = std::thread::Builder::new().stack_size(1 << 20).spawn(|| down(0)).unwrap().join().unwrap();
        assert!(r.is_err());
    }
}
