//! How much memory a new run could still use, so the scheduler does not
//! start one that would push the machine into swap.

/// Available memory in bytes, or `None` when this platform's figure cannot
/// be read (the caller then skips the check and says so once).
#[must_use]
pub fn available() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        linux_available(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("vm_stat").output().ok()?;
        macos_available(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// `MemAvailable` from `/proc/meminfo` (kB).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_available(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

/// Free + inactive + speculative + purgeable pages from `vm_stat`, the pages
/// macOS hands out without paging anything else out.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_available(vm_stat: &str) -> Option<u64> {
    let page = vm_stat.lines().next()?.split("page size of ").nth(1)?.split_whitespace().next()?.parse::<u64>().ok()?;
    let pages = |name: &str| -> u64 {
        vm_stat.lines().find_map(|l| l.strip_prefix(name)).and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok()).unwrap_or(0)
    };
    let free = pages("Pages free:") + pages("Pages inactive:") + pages("Pages speculative:") + pages("Pages purgeable:");
    Some(free * page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_meminfo_is_read_in_bytes() {
        let meminfo = "MemTotal:       12000000 kB\nMemFree:          500000 kB\nMemAvailable:    7340032 kB\n";
        assert_eq!(linux_available(meminfo), Some(7_340_032 * 1024));
        assert_eq!(linux_available("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn macos_vm_stat_counts_reclaimable_pages() {
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               1000.\nPages active:                             9999.\nPages inactive:                            500.\nPages speculative:                         100.\nPages purgeable:                            50.\n";
        assert_eq!(macos_available(vm), Some((1000 + 500 + 100 + 50) * 16384));
        assert_eq!(macos_available("garbage"), None);
    }

    #[test]
    fn this_host_reports_a_figure() {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(available().is_some_and(|b| b > 0));
        }
    }
}
