//! Delay starting rustc while the machine is short on memory.
//!
//! "Available" is what the OS can hand out without swapping: on macOS
//! free + inactive + purgeable pages (the plain "free" count is misleadingly
//! low because the page cache fills it), on Linux `MemAvailable`, on Windows
//! the available physical memory (standby pages included). Read
//! directly instead of through `sysinfo` to control exactly which metric is
//! used.

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn available_bytes() -> Option<u64> {
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<libc::vm_statistics64>()
        / std::mem::size_of::<libc::integer_t>())
        as libc::mach_msg_type_number_t;
    let rc = unsafe {
        libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut stats as *mut _ as libc::host_info64_t,
            &mut count,
        )
    };
    if rc != 0 {
        return None;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    }
    let pages =
        stats.free_count as u64 + stats.inactive_count as u64 + stats.purgeable_count as u64;
    Some(pages * page as u64)
}

#[cfg(target_os = "linux")]
pub fn available_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(windows)]
pub fn available_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain struct with its length set, as the API requires.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullAvailPhys)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn available_bytes() -> Option<u64> {
    None
}

/// Wait (with backoff, at most `max_wait`) until available memory reaches
/// `min_free_mb`. Returns how long it waited. Never blocks forever: a build
/// that runs late beats a build that hangs.
pub fn wait_for_memory(min_free_mb: u64, max_wait: Duration) -> Duration {
    let start = Instant::now();
    if min_free_mb == 0 {
        return Duration::ZERO;
    }
    let need = min_free_mb * 1024 * 1024;
    let mut delay = Duration::from_millis(250);
    while start.elapsed() < max_wait {
        match available_bytes() {
            Some(avail) if avail < need => {
                std::thread::sleep(delay.min(max_wait.saturating_sub(start.elapsed())));
                delay = (delay * 2).min(Duration::from_secs(4));
            }
            _ => break,
        }
    }
    start.elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_some_memory() {
        let avail = available_bytes().expect("available memory");
        assert!(avail > 16 * 1024 * 1024);
    }

    #[test]
    fn zero_threshold_never_waits() {
        assert_eq!(wait_for_memory(0, Duration::from_secs(5)), Duration::ZERO);
    }

    #[test]
    fn impossible_threshold_is_bounded() {
        let waited = wait_for_memory(u64::MAX / (1024 * 1024 * 2), Duration::from_millis(300));
        assert!(waited < Duration::from_secs(2));
    }
}
