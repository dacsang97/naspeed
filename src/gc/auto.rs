//! Throttled automatic store trim: at most once a
//! day a wrapper that just stored a miss starts `naspeed gc --auto` in the
//! background. Builds never wait for it beyond the moment gc holds the
//! exclusive store lock.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const MARKER: &str = "last-auto-gc";
pub const INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// True (and the marker is touched) when a trim is due.
pub fn claim(store_root: &Path) -> bool {
    let marker = store_root.join(MARKER);
    let due = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_none_or(|age| age >= INTERVAL);
    // Touch first so concurrent wrappers do not all spawn a gc.
    due && std::fs::write(&marker, b"").is_ok()
}

/// Spawn `naspeed gc --auto` detached if a trim is due. Never fails a build.
pub fn maybe_spawn(store_root: &Path) {
    if std::env::var_os("NASPEED_NO_AUTO_GC").is_some() || !claim(store_root) {
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
        assert!(claim(dir.path()));
        assert!(!claim(dir.path()));
        let old = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(dir.path().join(MARKER), old).unwrap();
        assert!(claim(dir.path()));
    }
}
