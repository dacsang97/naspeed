//! Content-addressed store, one per volume.
//!
//! ```text
//! <root>/objects/<2>/<blake3>        immutable, read-only file content
//! <root>/entries/<2>/<key>.json      outputs of one cached rustc call
//! <root>/manifests/<2>/<tier1>.json  env-dep names seen for a tier-1 key
//! <root>/locks/<tier1>.lock          one compile per key at a time
//! <root>/store.lock                  shared: wrappers; exclusive: gc
//! <root>/index.sqlite                entry sizes, last use, hit counts
//! <root>/usage.log                   append-only hits/misses/target dirs
//! <root>/tests/<2>/<key>.json        cached passing test-binary runs
//! <root>/memo/                       source-tree hash memo
//! <root>/tmp/
//! ```
//!
//! Objects are written before the entry that references them; gc leaves
//! unreferenced objects alone for a grace period so an in-flight writer's
//! objects are never collected.

#[cfg(windows)]
mod block_clone;
pub mod content_hash;
pub mod cow_copy;
pub mod key_lock;
pub mod path_placeholders;
pub mod physical_usage;
pub mod usage_log;
pub mod volume_root;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::CowMode;
use key_lock::{FileLock, Mode};

pub const ENTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputFile {
    /// File name inside rustc's `--out-dir`.
    pub name: String,
    pub object: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DepInfoFile {
    pub name: String,
    /// Placeholder-encoded `.d` text.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub version: u32,
    pub key: String,
    pub tier1: String,
    pub crate_name: String,
    pub outputs: Vec<OutputFile>,
    pub dep_info: Option<DepInfoFile>,
    /// Placeholder-encoded rustc stdout / stderr, replayed on hit.
    pub stdout: String,
    pub stderr: String,
    pub created: u64,
    /// Build-script runs only: small text files that mention this target
    /// dir (pkg-config, cmake configs), stored placeholder-encoded. `name`
    /// is relative to `OUT_DIR`, like `outputs[].name`.
    #[serde(default)]
    pub texts: Vec<DepInfoFile>,
    /// Build-script runs only: (relative path, placeholder-encoded target).
    #[serde(default)]
    pub symlinks: Vec<(String, String)>,
    /// Build-script runs only: every directory, so empty ones come back too.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Wall time of the rustc / build-script run that produced the entry;
    /// what a hit saves (0 for entries written before it was recorded).
    #[serde(default)]
    pub compile_ms: u64,
}

impl Entry {
    pub fn size(&self) -> u64 {
        self.outputs.iter().map(|o| o.size).sum()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub env_deps: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
    pub cow_mode: CowMode,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn sharded(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let shard = name.get(..2).unwrap_or("xx");
    dir.join(shard).join(format!("{name}{ext}"))
}

fn write_atomic(path: &Path, bytes: &[u8], tmp_dir: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_dir.join(format!(
        "w.{}.{}.{}",
        std::process::id(),
        now_nanos(),
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

impl Store {
    /// Open (creating the layout if needed) an existing store root.
    pub fn open(root: &Path, cow_mode: CowMode) -> Result<Store> {
        for sub in [
            "objects",
            "entries",
            "manifests",
            "locks",
            "tmp",
            "memo",
            "tests",
        ] {
            std::fs::create_dir_all(root.join(sub))
                .with_context(|| format!("create {}", root.join(sub).display()))?;
        }
        Ok(Store {
            root: root.to_path_buf(),
            cow_mode,
        })
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }
    pub fn entries_dir(&self) -> PathBuf {
        self.root.join("entries")
    }
    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
    pub fn memo_dir(&self) -> PathBuf {
        self.root.join("memo")
    }
    pub fn tests_dir(&self) -> PathBuf {
        self.root.join("tests")
    }
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.sqlite")
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        sharded(&self.objects_dir(), hash, "")
    }
    pub fn entry_path(&self, key: &str) -> PathBuf {
        sharded(&self.entries_dir(), key, ".json")
    }
    pub fn manifest_path(&self, tier1: &str) -> PathBuf {
        sharded(&self.manifests_dir(), tier1, ".json")
    }
    pub fn test_result_path(&self, key: &str) -> PathBuf {
        sharded(&self.tests_dir(), key, ".json")
    }

    pub fn shared_lock(&self) -> io::Result<FileLock> {
        key_lock::lock(&self.root.join("store.lock"), Mode::Shared)
    }
    pub fn exclusive_lock(&self) -> io::Result<FileLock> {
        key_lock::lock(&self.root.join("store.lock"), Mode::Exclusive)
    }
    pub fn try_exclusive_lock(&self) -> io::Result<Option<FileLock>> {
        key_lock::try_lock(&self.root.join("store.lock"), Mode::Exclusive)
    }
    pub fn key_lock(&self, tier1: &str) -> io::Result<FileLock> {
        key_lock::lock(
            &self.root.join("locks").join(format!("{tier1}.lock")),
            Mode::Exclusive,
        )
    }

    pub fn read_entry(&self, key: &str) -> Option<Entry> {
        let text = std::fs::read(self.entry_path(key)).ok()?;
        let entry: Entry = serde_json::from_slice(&text).ok()?;
        (entry.version == ENTRY_VERSION && entry.key == key).then_some(entry)
    }

    /// Write `bytes` to a store file through `tmp/` (atomic rename).
    pub fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        write_atomic(path, bytes, &self.tmp_dir())?;
        Ok(())
    }

    pub fn write_entry(&self, entry: &Entry) -> Result<()> {
        let bytes = serde_json::to_vec(entry)?;
        write_atomic(&self.entry_path(&entry.key), &bytes, &self.tmp_dir())?;
        Ok(())
    }

    pub fn remove_entry(&self, key: &str) {
        let _ = std::fs::remove_file(self.entry_path(key));
    }

    pub fn read_manifest(&self, tier1: &str) -> Option<Manifest> {
        let text = std::fs::read(self.manifest_path(tier1)).ok()?;
        serde_json::from_slice(&text).ok()
    }

    pub fn write_manifest(&self, tier1: &str, manifest: &Manifest) -> Result<()> {
        let bytes = serde_json::to_vec(manifest)?;
        write_atomic(&self.manifest_path(tier1), &bytes, &self.tmp_dir())?;
        Ok(())
    }

    /// Put `src`'s content into `objects/` (cloned, so free on CoW volumes)
    /// and return its hash and size. Existing objects are reused.
    pub fn ingest_object(&self, src: &Path) -> Result<(String, u64)> {
        let hash = content_hash::hash_file(src)
            .with_context(|| format!("hash {}", src.display()))?
            .to_hex()
            .to_string();
        let size = std::fs::metadata(src)?.len();
        let dst = self.object_path(&hash);
        if std::fs::metadata(&dst).is_ok_and(|m| m.len() == size) {
            return Ok((hash, size));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self
            .tmp_dir()
            .join(format!("o.{}.{}", std::process::id(), now_nanos()));
        // Hardlink mode still stores an independent copy: target files must
        // never share an inode with the objects they were built from.
        let mode = match self.cow_mode {
            CowMode::Hardlink => CowMode::Auto,
            other => other,
        };
        cow_copy::clone_or_copy(src, &tmp, mode)
            .with_context(|| format!("store {}", src.display()))?;
        crate::platform::set_mode(&tmp, 0o444)?;
        place_object(&tmp, &dst, size)?;
        Ok((hash, size))
    }

    /// Move a fully written, hash-verified file from `tmp/` into `objects/`.
    pub fn install_object(&self, tmp: &Path, hash: &str) -> Result<()> {
        let dst = self.object_path(hash);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::platform::set_mode(tmp, 0o444)?;
        let size = std::fs::metadata(tmp)?.len();
        place_object(tmp, &dst, size)?;
        Ok(())
    }

    /// A fresh, unique path under `tmp/`.
    pub fn tmp_path(&self, prefix: &str) -> PathBuf {
        self.tmp_dir()
            .join(format!("{prefix}.{}.{}", std::process::id(), now_nanos()))
    }

    pub fn append_usage(&self, record: &usage_log::Record) {
        usage_log::append(&self.root, record);
    }
}

/// Rename a finished object into place. Objects are content-addressed, so a
/// concurrent writer that got there first (Windows refuses to replace its
/// read-only file) leaves exactly the bytes we wanted.
fn place_object(tmp: &Path, dst: &Path, size: u64) -> std::io::Result<()> {
    match std::fs::rename(tmp, dst) {
        Ok(()) => Ok(()),
        Err(_) if std::fs::metadata(dst).is_ok_and(|m| m.len() == size) => {
            let _ = crate::platform::remove_file(tmp);
            Ok(())
        }
        Err(e) => {
            let _ = crate::platform::remove_file(tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placing_over_an_existing_read_only_object_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("obj");
        std::fs::write(&dst, b"same").unwrap();
        crate::platform::set_mode(&dst, 0o444).unwrap();
        let tmp = dir.path().join("tmp");
        std::fs::write(&tmp, b"same").unwrap();
        crate::platform::set_mode(&tmp, 0o444).unwrap();
        place_object(&tmp, &dst, 4).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"same");
        assert!(!tmp.exists());
        crate::platform::remove_file(&dst).unwrap();
        assert!(!dst.exists());
    }
}
