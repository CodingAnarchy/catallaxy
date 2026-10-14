//! Memory limits: which allocation requests the runtime refuses up front (so they become a
//! `MemoryError` panic with a stack trace instead of an allocator abort), and the optional heap
//! cap set with `CATX_MAX_HEAP`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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

/// After a major collection: notes whether the live heap is over the cap.
pub(crate) fn after_collection(live_bytes: usize) {
    if let Some(max) = max_heap() {
        if live_bytes > max {
            OVER_LIMIT.store(true, Ordering::Release);
        }
    }
}

/// The allocation budget until the next collection, capped so that a collection happens
/// before the heap passes `CATX_MAX_HEAP`.
pub(crate) fn cap_budget(budget: usize, live_bytes: usize) -> usize {
    match max_heap() {
        Some(max) => budget.min(max.saturating_sub(live_bytes).max(64 * 1024)),
        None => budget,
    }
}

/// Whether the last major collection found more live data than `CATX_MAX_HEAP` allows. The
/// first thread to ask after such a collection gets `true` (and should raise a `MemoryError`).
#[inline]
pub fn take_over_limit() -> bool {
    OVER_LIMIT.load(Ordering::Relaxed) && OVER_LIMIT.swap(false, Ordering::AcqRel)
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
