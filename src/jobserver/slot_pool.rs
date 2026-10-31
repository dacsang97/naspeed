//! Machine-wide rustc slots without a daemon: N lock files, a slot is held
//! by holding an exclusive `flock` on one of them. The kernel drops the lock
//! when the process dies, so slots are never orphaned.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::store::key_lock::{self, FileLock, Mode};

#[derive(Debug)]
pub struct Slot {
    pub index: usize,
    _lock: FileLock,
}

fn slot_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i}.lock"))
}

/// Take a free slot, or wait on one. `None` only if the slot dir is
/// unusable, in which case rustc runs unthrottled rather than failing.
pub fn acquire(dir: &Path, slots: usize) -> Option<Slot> {
    let slots = slots.max(1);
    std::fs::create_dir_all(dir).ok()?;
    let start = std::process::id() as usize % slots;
    // Poll every slot non-blocking first, then block on one picked by pid.
    // Polling again after a short sleep spreads waiters over freed slots
    // instead of queueing them all behind one.
    let mut delay = Duration::from_millis(20);
    for round in 0.. {
        for offset in 0..slots {
            let i = (start + offset) % slots;
            match key_lock::try_lock(&slot_path(dir, i), Mode::Exclusive) {
                Ok(Some(lock)) => {
                    return Some(Slot {
                        index: i,
                        _lock: lock,
                    });
                }
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        if round >= 50 {
            let lock = key_lock::lock(&slot_path(dir, start), Mode::Exclusive).ok()?;
            return Some(Slot {
                index: start,
                _lock: lock,
            });
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(250));
    }
    None
}

/// Number of slots currently held by some process.
pub fn in_use(dir: &Path, slots: usize) -> usize {
    (0..slots)
        .filter(|i| {
            let path = slot_path(dir, *i);
            path.exists() && matches!(key_lock::try_lock(&path, Mode::Exclusive), Ok(None))
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let a = acquire(dir.path(), 2).unwrap();
        let b = acquire(dir.path(), 2).unwrap();
        assert_ne!(a.index, b.index);
        assert_eq!(in_use(dir.path(), 2), 2);
        drop(a);
        assert_eq!(in_use(dir.path(), 2), 1);
        let c = acquire(dir.path(), 2).unwrap();
        assert_ne!(c.index, b.index);
    }
}
