//! The few OS services naspeed needs that std does not cover the same way on
//! Unix and Windows: permission bits, symlinks, volume identity, free space,
//! process priority and small per-file attributes (xattrs on Unix, NTFS/ReFS
//! alternate data streams on Windows). Everything else stays plain std.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

use std::io;
use std::path::{Path, PathBuf};

/// `std::fs::canonicalize` without Windows' `\\?\` prefix where a plain path
/// means the same thing: rustc, cargo and the paths they print use the plain
/// form, and both forms must compare equal.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// `name` as an executable file name on this OS (`rustc` → `rustc.exe`).
pub fn exe_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_writability() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        set_mode(&p, 0o444).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert!(meta.permissions().readonly());
        assert_eq!(mode_of(&meta) & 0o222, 0);
        set_mode(&p, 0o644).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert!(!meta.permissions().readonly());
        assert_ne!(mode_of(&meta) & 0o200, 0);
    }

    #[test]
    fn attrs_round_trip_and_keep_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"payload").unwrap();
        let old = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&p, old).unwrap();
        assert_eq!(get_attr(&p, "user.naspeed.test"), None);
        if set_attr(&p, "user.naspeed.test", b"v1").is_err() {
            // Filesystem without xattrs / streams (e.g. tmpfs without user
            // xattrs): the memo is optional, nothing more to check.
            return;
        }
        assert_eq!(
            get_attr(&p, "user.naspeed.test").as_deref(),
            Some(&b"v1"[..])
        );
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(filetime::FileTime::from_last_modification_time(&meta), old);
        assert_eq!(meta.len(), 7);
        assert_eq!(std::fs::read(&p).unwrap(), b"payload");
    }

    #[test]
    fn same_dir_is_same_device() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(device_of(dir.path()).is_some());
        assert_eq!(device_of(dir.path()), device_of(&sub));
        assert!(is_writable_dir(dir.path()));
        assert!(free_bytes(dir.path()).is_some_and(|b| b > 0));
    }
}
