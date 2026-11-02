//! Throttled automatic store trim: at most once a day a wrapper that just
//! finished a miss starts `naspeed gc --auto` in the background, and every
//! half hour while the store's volume is below `min_free_disk_pct`. Builds
//! never wait for it beyond the moment gc holds the exclusive store lock.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const MARKER: &str = "last-auto-gc";
pub const INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// A nearly full volume cannot wait a day: misses are not stored meanwhile,
/// and the drive itself degrades as free space runs out.
pub const LOW_DISK_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// True (and the marker is touched) when a trim is due after `interval`.
pub fn claim(store_root: &Path, interval: Duration) -> bool {
    let marker = store_root.join(MARKER);
    let due = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_none_or(|age| age >= interval);
    // Touch first so concurrent wrappers do not all spawn a gc.
    due && std::fs::write(&marker, b"").is_ok()
}

/// Spawn `naspeed gc --auto` detached if a trim is due. Never fails a build.
pub fn maybe_spawn(store_root: &Path, min_free_disk_pct: f64) {
    if std::env::var_os("NASPEED_NO_AUTO_GC").is_some() {
        return;
    }
    let interval = if crate::store::disk_below(store_root, min_free_disk_pct) {
        LOW_DISK_INTERVAL
    } else {
        INTERVAL
    };
    if !claim(store_root, interval) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args(["gc", "--auto"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_once_per_interval() {
        let dir = tempfile::tempdir().unwrap();
        assert!(claim(dir.path(), INTERVAL));
        assert!(!claim(dir.path(), INTERVAL));
        let old = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(dir.path().join(MARKER), old).unwrap();
        assert!(claim(dir.path(), INTERVAL));
    }

    #[test]
    fn low_disk_interval_is_shorter() {
        let dir = tempfile::tempdir().unwrap();
        assert!(claim(dir.path(), INTERVAL));
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        filetime::set_file_mtime(
            dir.path().join(MARKER),
            filetime::FileTime::from_system_time(hour_ago),
        )
        .unwrap();
        assert!(!claim(dir.path(), INTERVAL));
        assert!(claim(dir.path(), LOW_DISK_INTERVAL));
    }
}
