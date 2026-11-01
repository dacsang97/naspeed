//! Whole-file advisory locks (`flock` on Unix, `LockFileEx` on Windows,
//! through std). Locks die with the process, so a killed wrapper never
//! leaves a lock behind.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    /// Unlock explicitly instead of relying on close: a child forked by
    /// another thread holds a copy of the fd until its exec, and closing only
    /// our copy would keep the lock held for that window.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

fn open(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Block until the lock is held.
pub fn lock(path: &Path, mode: Mode) -> io::Result<FileLock> {
    let file = open(path)?;
    loop {
        let res = match mode {
            Mode::Shared => file.lock_shared(),
            Mode::Exclusive => file.lock(),
        };
        match res {
            Ok(()) => return Ok(FileLock { file }),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn held(file: File, res: Result<(), TryLockError>) -> io::Result<Option<FileLock>> {
    match res {
        Ok(()) => Ok(Some(FileLock { file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// `Ok(None)` when someone else holds a conflicting lock.
pub fn try_lock(path: &Path, mode: Mode) -> io::Result<Option<FileLock>> {
    let file = open(path)?;
    let res = match mode {
        Mode::Shared => file.try_lock_shared(),
        Mode::Exclusive => file.try_lock(),
    };
    held(file, res)
}

/// Try an exclusive lock on an existing file without creating it (cargo's
/// own lock files).
pub fn try_lock_existing(path: &Path) -> io::Result<Option<FileLock>> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let res = file.try_lock();
    held(file, res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_excludes_shared() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("store.lock");
        let shared = lock(&p, Mode::Shared).unwrap();
        assert!(try_lock(&p, Mode::Shared).unwrap().is_some());
        assert!(try_lock(&p, Mode::Exclusive).unwrap().is_none());
        drop(shared);
        let ex = try_lock(&p, Mode::Exclusive).unwrap();
        assert!(ex.is_some());
        assert!(try_lock(&p, Mode::Shared).unwrap().is_none());
    }
}
