//! Leases: an exclusively locked file under `leases/<action-key>/` that says
//! "this action's outputs (or staging dir, or view) are in use".
//!
//! A lease is a small JSON file `<lease-id>.lease`, locked for the holder's
//! whole lifetime. The lock dies with the process — a crashed wrapper or
//! runner leaves an unlocked file that GC treats as dead and reaps. The
//! per-key `write.lock` used by `action_cache` is not a lease.
//!
//! Two protections, both enforced by `gc::store_lru`:
//!
//! - every active lease marks its action key live (the result record, its
//!   output tree, and the tree's objects survive any trim);
//! - `guards` digests recorded at acquisition mark bare objects live even
//!   before a result record exists (a run mid-materialization, an execution
//!   between ingest and commit).
//!
//! Acquire *before* materializing or running and release only after every
//! child process has exited: the lease is the GC contract.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::key_lock::{FileLock, Mode, try_lock_existing};
use super::{Store, now_secs};

/// Suffix identifying lease files under `leases/<key>/`.
pub const LEASE_EXT: &str = "lease";

/// Why the holder is referencing the action: for debugging only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// An execution writing to `staging/<lease-id>/` until its commit.
    Run,
    /// A materialized view; held while child processes may use it.
    View,
}

/// On-lease metadata: `key` is implied by the parent directory. `guards`
/// are extra object digests the holder needs regardless of any record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseInfo {
    pub pid: u32,
    pub created: u64,
    pub purpose: Purpose,
    #[serde(default)]
    pub guards: Vec<String>,
}

/// A held lease. `release` (or drop) unlocks and removes the file.
pub struct Lease {
    file: PathBuf,
    lock: Option<FileLock>,
}

impl Lease {
    /// Unique id of this lease; `staging/<id>` is the execution dir.
    pub fn id(&self) -> &str {
        self.file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
    }

    /// Drop the lock and remove the lease file.
    pub fn release(mut self) {
        self.lock.take();
        let _ = crate::platform::remove_file(&self.file);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.lock.take();
        let _ = crate::platform::remove_file(&self.file);
    }
}

fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    format!("{}.{}", std::process::id(), nanos)
}

/// Hold a lease on `key`. The file's exclusive lock is the liveness signal:
/// `is_live` sees it held until this process exits or releases.
pub fn acquire(
    store: &Store,
    key: &str,
    purpose: Purpose,
    guards: &[String],
) -> Result<Lease> {
    let dir = store.lease_dir(key);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = dir.join(format!("{}.{}", new_id(), LEASE_EXT));
    let lock = super::key_lock::lock(&file, Mode::Exclusive)?;
    let info = LeaseInfo {
        pid: std::process::id(),
        created: now_secs(),
        purpose,
        guards: guards.to_vec(),
    };
    if let Ok(bytes) = serde_json::to_vec(&info) {
        let _ = fs::write(&file, bytes);
    }
    Ok(Lease {
        file,
        lock: Some(lock),
    })
}

/// True while `file` is held or was just created (inside `grace`) by a
/// process between opening and locking it. Missing files are dead.
fn file_live(file: &Path, grace: Duration) -> bool {
    match try_lock_existing(file) {
        Ok(None) => true,
        Ok(Some(_probe)) => modified_within(file, grace),
        Err(_) => false,
    }
}

fn modified_within(path: &Path, age: Duration) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(elapsed) = meta.modified().map(|m| m.elapsed()) else {
        return false;
    };
    elapsed.map(|e| e < age).unwrap_or(false)
}

/// Iterate `*.lease` files under `leases/`, partitioned by a held lock.
/// A file no process locks counts as active only while younger than
/// `grace` — it may belong to a holder between create and lock.
pub fn active(store: &Store, grace: Duration) -> Vec<(String, PathBuf, Option<LeaseInfo>)> {
    let mut out = Vec::new();
    let Ok(keys) = fs::read_dir(store.leases_dir()) else {
        return out;
    };
    for key_dir in keys.flatten() {
        let Ok(entries) = fs::read_dir(key_dir.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some(LEASE_EXT) {
                continue;
            }
            if !file_live(&path, grace) {
                continue;
            }
            let info = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<LeaseInfo>(&b).ok());
            out.push((
                key_dir.file_name().to_string_lossy().into_owned(),
                path,
                info,
            ));
        }
    }
    out
}

/// The keys behind every active lease.
pub fn active_keys(store: &Store, grace: Duration) -> std::collections::HashSet<String> {
    active(store, grace).into_iter().map(|(k, _, _)| k).collect()
}

/// Extra object digests guarded by active leases (info may be absent or
/// unwritten yet: only parsed files contribute).
pub fn active_guards(store: &Store, grace: Duration) -> Vec<String> {
    active(store, grace)
        .into_iter()
        .flat_map(|(_, _, info)| info.map(|i| i.guards).unwrap_or_default())
        .collect()
}

/// True while some live lease is named `<id>.lease`, whatever key it sits
/// under — `staging/<id>` directories are identified this way.
pub fn id_active(store: &Store, id: &str, grace: Duration) -> bool {
    let Ok(keys) = fs::read_dir(store.leases_dir()) else {
        return false;
    };
    keys.flatten().any(|key_dir| {
        file_live(
            &key_dir.path().join(format!("{id}.{LEASE_EXT}")),
            grace,
        )
    })
}

/// Remove dead lease files: unlocked and older than `grace` (a holder's
/// file is never older than its own lifetime). Returns the count removed.
pub fn sweep_dead(store: &Store, grace: Duration, dry_run: bool) -> usize {
    let mut removed = 0;
    let Ok(keys) = fs::read_dir(store.leases_dir()) else {
        return 0;
    };
    for key_dir in keys.flatten() {
        let Ok(entries) = fs::read_dir(key_dir.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some(LEASE_EXT) {
                continue;
            }
            if file_live(&path, grace) {
                continue;
            }
            removed += 1;
            if !dry_run {
                let _ = crate::platform::remove_file(&path);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CowMode;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), CowMode::Auto).unwrap();
        (dir, s)
    }

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn held_lease_is_live_release_is_dead() {
        let (_d, s) = store();
        let lease = acquire(&s, KEY, Purpose::Run, &["deadbeef".into()]).unwrap();
        assert!(active_keys(&s, Duration::ZERO).contains(KEY));
        assert_eq!(active_guards(&s, Duration::ZERO), vec!["deadbeef"]);
        let file = lease.file.clone();
        lease.release();
        assert!(!file.exists());
        assert!(active_keys(&s, Duration::ZERO).is_empty());
    }

    #[test]
    fn simulated_crash_leaves_dead_lease_swept() {
        let (_d, s) = store();
        let dir = s.lease_dir(KEY);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("123.4.lease");
        fs::write(&file, b"{}").unwrap();
        // An unlocked file younger than grace could be mid-acquire: live.
        assert!(active_keys(&s, Duration::from_secs(60)).contains(KEY));
        // Past grace it is a corpse.
        assert!(active_keys(&s, Duration::ZERO).is_empty());
        assert_eq!(sweep_dead(&s, Duration::ZERO, false), 1);
        assert!(!file.exists());
    }

    #[test]
    fn write_lock_is_not_a_lease() {
        let (_d, s) = store();
        let _lock = crate::store::key_lock::lock(&s.action_lock_path(KEY), Mode::Exclusive).unwrap();
        assert!(active(&s, Duration::ZERO).is_empty());
    }

    #[test]
    fn id_matches_staging_dir_liveness() {
        let (_d, s) = store();
        let lease = acquire(&s, KEY, Purpose::Run, &[]).unwrap();
        assert!(id_active(&s, lease.id(), Duration::ZERO));
        assert!(!id_active(&s, "999.0", Duration::ZERO));
        let id = lease.id().to_owned();
        lease.release();
        assert!(!id_active(&s, &id, Duration::ZERO));
    }
}
