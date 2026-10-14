//! Memory limits: which allocation requests the runtime refuses up front (so they become a
//! `MemoryError` panic with a stack trace instead of an allocator abort), and the optional heap
//! cap set with `CATX_MAX_HEAP`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub use crate::heap::release_headroom;

/// The heap cap from `CATX_MAX_HEAP`: a byte count with an optional `K`, `M` or `G` suffix
/// (powers of 1024). Unset, empty or unparsable means no cap.
pub fn max_heap() -> Option<usize> {
    static MAX: OnceLock<Option<usize>> = OnceLock::new();
    *MAX.get_or_init(|| std::env::var("CATX_MAX_HEAP").ok().and_then(|v| parse_size(&v)))
}

/// Parses `"512M"`, `"2g"`, `"1048576"` and the like.
pub fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let (digits, shift) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 10),
        (i, 'm' | 'M') => (&s[..i], 20),
        (i, 'g' | 'G') => (&s[..i], 30),
        _ => (s, 0),
    };
    let n: usize = digits.trim().parse().ok()?;
    n.checked_mul(1 << shift).filter(|&b| b > 0)
}

/// Physical memory, where the platform reports it.
pub fn physical_memory() -> Option<usize> {
    static PHYS: OnceLock<Option<usize>> = OnceLock::new();
    *PHYS.get_or_init(|| {
        #[cfg(all(unix, not(miri)))]
        {
            // SAFETY: sysconf has no preconditions.
            let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
            if pages > 0 && size > 0 {
                return (pages as usize).checked_mul(size as usize);
            }
        }
        #[cfg(all(windows, not(miri)))]
        {
            #[repr(C)]
            struct MemoryStatusEx {
                length: u32,
                memory_load: u32,
                total_phys: u64,
                avail_phys: u64,
                total_page_file: u64,
                avail_page_file: u64,
                total_virtual: u64,
                avail_virtual: u64,
                avail_extended_virtual: u64,
            }
            #[link(name = "kernel32")]
            extern "system" {
                fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
            }
            // SAFETY: the struct is MEMORYSTATUSEX with its length set, as the call requires.
            unsafe {
                let mut st: MemoryStatusEx = std::mem::zeroed();
                st.length = std::mem::size_of::<MemoryStatusEx>() as u32;
                if GlobalMemoryStatusEx(&mut st) != 0 && st.total_phys > 0 {
                    return usize::try_from(st.total_phys).ok();
                }
            }
        }
        None
    })
}

/// Why a single request of `bytes` would be refused, if it would: it is larger than the heap cap
/// or than physical memory. (With overcommit, the allocator may hand out such a block and the
/// process dies later when it is touched; refusing it up front keeps the failure catchable.)
pub fn check_request(bytes: usize) -> Result<(), String> {
    if let Some(max) = max_heap() {
        if bytes > max {
            return Err(format!("cannot allocate {bytes} bytes (CATX_MAX_HEAP is {max} bytes)"));
        }
    }
    if let Some(phys) = physical_memory() {
        if bytes > phys {
            return Err(format!("cannot allocate {bytes} bytes (more than the machine's {phys} bytes of memory)"));
        }
    }
    if bytes > isize::MAX as usize {
        return Err(format!("cannot allocate {bytes} bytes"));
    }
    Ok(())
}

static OVER_LIMIT: AtomicBool = AtomicBool::new(false);
static OUT_OF_MEMORY: AtomicBool = AtomicBool::new(false);

/// After a major collection: notes whether the live heap is over the cap. A collection that
/// finds it back under the cap withdraws an error no thread has raised yet.
pub(crate) fn after_collection(live_bytes: usize) {
    if let Some(max) = max_heap() {
        OVER_LIMIT.store(live_bytes > max, Ordering::Release);
    }
}

/// After a major collection: notes whether the heap is out of memory, that is, whether the
/// collector could not restore its emergency reserve of segments (see `heap.rs`). As with
/// [`after_collection`], a later collection that restores it withdraws a pending error.
pub(crate) fn note_out_of_memory(out: bool) {
    OUT_OF_MEMORY.store(out, Ordering::Release);
}

/// The allocation budget until the next collection, capped so that a collection happens
/// before the heap passes `CATX_MAX_HEAP`.
pub(crate) fn cap_budget(budget: usize, live_bytes: usize) -> usize {
    match max_heap() {
        Some(max) => budget.min(max.saturating_sub(live_bytes).max(64 * 1024)),
        None => budget,
    }
}

/// The `MemoryError` message a thread should panic with at a safepoint, if the last major
/// collection found more live data than `CATX_MAX_HEAP` allows, or found that the system would
/// not give the heap more memory. The first thread to ask after such a collection gets it.
/// Callers check this on their safepoint slow path only, after taking part in any collection.
#[inline]
pub fn take_pending_error() -> Option<String> {
    if !OVER_LIMIT.load(Ordering::Relaxed) && !OUT_OF_MEMORY.load(Ordering::Relaxed) {
        return None;
    }
    take_pending_slow()
}

#[cold]
fn take_pending_slow() -> Option<String> {
    if OVER_LIMIT.swap(false, Ordering::AcqRel) {
        return Some(format!(
            "MemoryError: {:.1} MB live after collection, over the CATX_MAX_HEAP limit of {:.1} MB",
            crate::stats::live_bytes() as f64 / 1e6,
            max_heap().unwrap_or(0) as f64 / 1e6
        ));
    }
    if OUT_OF_MEMORY.swap(false, Ordering::AcqRel) {
        return Some(format!(
            "MemoryError: out of memory: the system refused an allocation ({:.1} MB live after collection)",
            crate::stats::live_bytes() as f64 / 1e6
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1048576"), Some(1 << 20));
        assert_eq!(parse_size("64K"), Some(64 << 10));
        assert_eq!(parse_size(" 512m "), Some(512 << 20));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn huge_requests_are_refused() {
        assert!(check_request(1024).is_ok());
        assert!(check_request(usize::MAX).is_err());
        assert!(check_request(isize::MAX as usize + 1).is_err());
    }
}
