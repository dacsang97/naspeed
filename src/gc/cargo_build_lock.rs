//! Respect cargo's own locks before touching a profile dir. Cargo 1.98
//! holds `.cargo-lock`, `.cargo-build-lock` and `.cargo-artifact-lock`
//! (flock) in profile dirs while it builds; if any is held we skip the dir.

use std::path::Path;

use crate::store::key_lock::{FileLock, try_lock_existing};

pub const CARGO_LOCK_FILES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// Exclusive locks on every cargo lock file present in `profile_dir` (and
/// its target root). `None` when cargo is using the dir right now.
pub fn lock_profile_dir(profile_dir: &Path) -> Option<Vec<FileLock>> {
    let mut held = Vec::new();
    let mut dirs = vec![profile_dir.to_path_buf()];
    // target/debug → target, target/<triple>/debug → target/<triple>, target
    let mut parent = profile_dir.parent();
    for _ in 0..2 {
        if let Some(p) = parent {
            dirs.push(p.to_path_buf());
            parent = p.parent();
        }
    }
    for dir in dirs {
        for name in CARGO_LOCK_FILES {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            match try_lock_existing(&path) {
                Ok(Some(lock)) => held.push(lock),
                Ok(None) => return None,
                Err(_) => continue,
            }
        }
    }
    Some(held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::key_lock::{Mode, lock};

    #[test]
    fn busy_profile_dir_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("target/debug");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join(".cargo-lock"), b"").unwrap();
        let cargo_holds = lock(&profile.join(".cargo-lock"), Mode::Exclusive).unwrap();
        assert!(lock_profile_dir(&profile).is_none());
        drop(cargo_holds);
        assert!(lock_profile_dir(&profile).is_some());
    }
}
