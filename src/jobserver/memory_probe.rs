//! Raw memory readings from the OS: how much memory a new rustc can get
//! without swapping, whether the kernel reports memory pressure, total RAM,
//! and the resident size of a running process. Read directly instead of
//! through `sysinfo` to control exactly which metric is used.
//!
//! "Available" deliberately excludes memory the OS can only reclaim by
//! compressing or swapping. On macOS the inactive list mostly holds
//! anonymous (application) pages on a loaded machine; counting them made a
//! 16 GB Mac with 11 GB of swap in use look like it had 3-5 GB to spare.

#[cfg(target_os = "macos")]
mod imp {
    #[allow(deprecated)]
    fn vm_stats() -> Option<(libc::vm_statistics64, u64)> {
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
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (rc == 0 && page > 0).then_some((stats, page as u64))
    }

    /// free (speculative included) + purgeable + the file-backed part of the
    /// inactive list. Anonymous inactive pages are left out: freeing them
    /// costs compression or swap.
    pub fn available_bytes() -> Option<u64> {
        let (s, page) = vm_stats()?;
        let pages = super::reclaimable_pages(
            s.free_count as u64,
            s.purgeable_count as u64,
            s.inactive_count as u64,
            s.external_page_count as u64,
        );
        Some(pages * page)
    }

    fn sysctl_by_name<T: Default>(name: &std::ffi::CStr) -> Option<T> {
        let mut value = T::default();
        let mut len = std::mem::size_of::<T>();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut value as *mut T as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && len == std::mem::size_of::<T>()).then_some(value)
    }

    pub fn total_bytes() -> Option<u64> {
        sysctl_by_name::<u64>(c"hw.memsize")
    }

    /// The kernel's own verdict, the one Activity Monitor's memory pressure
    /// graph shows: 1 normal, 2 warning, 4 critical.
    pub fn under_pressure() -> bool {
        sysctl_by_name::<i32>(c"kern.memorystatus_vm_pressure_level").is_some_and(|l| l >= 2)
    }

    pub fn process_rss_bytes(pid: u32) -> Option<u64> {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then_some(info.pti_resident_size)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    fn meminfo_kb(key: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = text.lines().find(|l| l.starts_with(key))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    /// The kernel's own estimate, which already leaves out what needs swap.
    pub fn available_bytes() -> Option<u64> {
        meminfo_kb("MemAvailable:").map(|kb| kb * 1024)
    }

    pub fn total_bytes() -> Option<u64> {
        meminfo_kb("MemTotal:").map(|kb| kb * 1024)
    }

    /// Pressure stall information: share of the last 10 s in which some task
    /// waited on memory. Absent on kernels without PSI.
    pub fn under_pressure() -> bool {
        std::fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|t| super::psi_some_avg10(&t))
            .is_some_and(|avg| avg >= super::PSI_PRESSURE_AVG10)
    }

    pub fn process_rss_bytes(pid: u32) -> Option<u64> {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page > 0).then(|| pages * page as u64)
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    fn status() -> Option<MEMORYSTATUSEX> {
        // SAFETY: plain struct with its length set, as the API requires.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
    }

    /// Available physical memory, standby pages included.
    pub fn available_bytes() -> Option<u64> {
        status().map(|s| s.ullAvailPhys)
    }

    pub fn total_bytes() -> Option<u64> {
        status().map(|s| s.ullTotalPhys)
    }

    /// No cheap pressure signal; the available count carries the decision.
    pub fn under_pressure() -> bool {
        false
    }

    /// (current working set, peak working set) of a process.
    pub fn child_working_set(child: &std::process::Child) -> Option<(u64, u64)> {
        use std::os::windows::io::AsRawHandle;
        // The handle stays open as long as `child` is alive.
        working_set(child.as_raw_handle())
    }

    /// (current working set, peak working set) of an open process handle.
    fn working_set(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<(u64, u64)> {
        // SAFETY: plain struct with its size passed alongside.
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        (unsafe { K32GetProcessMemoryInfo(handle, &mut c, size) } != 0)
            .then_some((c.WorkingSetSize as u64, c.PeakWorkingSetSize as u64))
    }

    pub fn process_rss_bytes(pid: u32) -> Option<u64> {
        // SAFETY: the handle is checked, used once and closed.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let ws = working_set(handle).map(|(current, _)| current);
            CloseHandle(handle);
            ws
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    pub fn available_bytes() -> Option<u64> {
        None
    }
    pub fn total_bytes() -> Option<u64> {
        None
    }
    pub fn under_pressure() -> bool {
        false
    }
    pub fn process_rss_bytes(_pid: u32) -> Option<u64> {
        None
    }
}

#[cfg(windows)]
pub use imp::child_working_set;
pub use imp::{available_bytes, process_rss_bytes, total_bytes, under_pressure};

/// Linux PSI `some avg10` (percent) at or above which memory counts as
/// under pressure.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PSI_PRESSURE_AVG10: f64 = 10.0;

/// Pages a new process can get without compressing or swapping: free pages,
/// purgeable pages, and inactive pages up to the file-backed total (cache
/// that can be dropped). Anonymous inactive pages need compression or swap.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn reclaimable_pages(free: u64, purgeable: u64, inactive: u64, file_backed: u64) -> u64 {
    free + purgeable + inactive.min(file_backed)
}

/// `some avg10=…` from `/proc/pressure/memory`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_some_memory() {
        assert!(available_bytes().is_some_and(|b| b > 0) || cfg!(windows));
        assert!(total_bytes().expect("total memory") > 16 * 1024 * 1024);
    }

    #[test]
    fn anonymous_inactive_pages_are_not_available() {
        // The incident's numbers (16 KiB pages): 145 MiB free, 2.8 GiB
        // inactive of which only 2.4 GiB file-backed.
        let pages = reclaimable_pages(9_280, 2, 180_259, 140_085);
        assert_eq!(pages, 9_280 + 2 + 140_085);
        // Mostly cache: every inactive page counts.
        assert_eq!(reclaimable_pages(10, 0, 100, 500), 110);
    }

    #[test]
    fn psi_line_is_parsed() {
        let text = "some avg10=12.50 avg60=3.00 avg300=1.00 total=123\n\
                    full avg10=1.00 avg60=0.00 avg300=0.00 total=45\n";
        assert_eq!(psi_some_avg10(text), Some(12.5));
        assert_eq!(psi_some_avg10("garbage"), None);
    }

    #[test]
    fn own_rss_is_readable() {
        let rss = process_rss_bytes(std::process::id()).expect("own rss");
        assert!(rss > 1024 * 1024);
    }
}
