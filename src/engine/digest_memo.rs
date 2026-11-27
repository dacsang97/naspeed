//! Stat-to-digest memoization for plan-time input hashing.
//!
//! Reading every input file on every plan is the expensive part of
//! freshness; re-`stat`ing is cheap. [`DigestMemo`] caches
//! `(path -> StatSignature + digest)` and only re-hashes when the
//! signature moved.
//!
//! Two failure modes drive the signature's shape:
//!
//! - **Rapid same-size edits.** `size` alone is not a signature: two
//!   writes a millisecond apart can keep length identical. The signature
//!   carries nanosecond mtime, ctime (where the platform exposes it) and
//!   the file identity (dev+ino / volume+index), so a same-size rewrite
//!   — including a rename-over — still looks different.
//! - **Clock skew.** An mtime in the future, or one inside
//!   [`RECENT_WRITE`], is never trusted as a memo key: the file is hashed
//!   every time instead of risking a stale hit while a writer may still
//!   be producing it (and while another host's clock may sit ahead of
//!   ours).
//!
//! A file that changes between the pre-hash stat and the post-hash
//! re-stat produces a *correct* digest for the bytes actually read, but
//! the entry is not memoized — the next call re-reads rather than
//! trusting a mid-write signature.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// mtimes at most this far in the past (or at all in the future) always
/// bypass the memo and hash the file.
pub const RECENT_WRITE: Duration = Duration::from_secs(2);

/// Everything `stat` must report about a file for a memo hit to be legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatSignature {
    pub size: u64,
    /// Nanoseconds since the Unix epoch; `i64::MIN` when `mtime` precedes
    /// the epoch (never a hit, still hashed).
    pub mtime_ns: i64,
    /// Unix ctime (metadata change) in nanoseconds; 0 where unavailable.
    /// Catches the "same mtime, rewritten inode" and clock-rollback cases.
    pub ctime_ns: i64,
    /// dev/ino on Unix, volume-serial/file-index on Windows.
    pub identity: (u64, u64),
}

impl StatSignature {
    /// True when the mtime is too fresh (or in the future) to trust: the
    /// file is hashed unconditionally instead of memoized.
    pub fn recent(&self, now: SystemTime) -> bool {
        if self.mtime_ns == i64::MIN {
            return true;
        }
        let Ok(now_ns) = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
        else {
            // Our own clock is before the epoch; every mtime is "future".
            return true;
        };
        self.mtime_ns > now_ns || (now_ns - self.mtime_ns) < RECENT_WRITE.as_nanos() as i64
    }
}

#[cfg(unix)]
pub fn stat_signature(path: &Path) -> io::Result<StatSignature> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(StatSignature {
        size: meta.size(),
        mtime_ns: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(i64::MIN),
        ctime_ns: meta
            .ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.ctime_nsec()),
        identity: (meta.dev(), meta.ino()),
    })
}

#[cfg(windows)]
pub fn stat_signature(path: &Path) -> io::Result<StatSignature> {
    use std::os::windows::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(StatSignature {
        size: meta.file_size(),
        mtime_ns: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(i64::MIN),
        ctime_ns: 0,
        identity: (
            meta.volume_serial_number().unwrap_or(0),
            meta.file_index().unwrap_or(0),
        ),
    })
}

#[derive(Debug, Clone)]
struct Entry {
    signature: StatSignature,
    digest: String,
}

/// What a light-weight `probe` found for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileProbe {
    /// Signature identical to the memoized one (no content check needed).
    Same,
    /// File exists but its signature differs (or was never memoized).
    Changed,
    /// `stat` failed — treat as a change.
    Missing,
}

/// In-memory memo; disposable per service or per in-process plan session.
#[derive(Default)]
pub struct DigestMemo {
    entries: BTreeMap<PathBuf, Entry>,
    /// Paths observed absent at `prime` (optional manifests like go.sum):
    /// still-absent probes `Same`, appearing probes `Changed`.
    absent: BTreeSet<PathBuf>,
}

impl DigestMemo {
    pub fn new() -> DigestMemo {
        DigestMemo::default()
    }

    /// Signature-only check used for manifest rescans: records the current
    /// signature so a *second* unchanged plan reports `Same`, and reports
    /// anything else as `Changed`/`Missing`. Never hashes.
    pub fn probe(&mut self, path: &Path) -> FileProbe {
        match stat_signature(path) {
            Ok(signature) => {
                self.absent.remove(path);
                match self.entries.get(path) {
                    // An identical recorded signature is the baseline: the
                    // file has not moved since we last looked, even inside
                    // the recent-write window (fresh checkout/copy).
                    Some(entry) if entry.signature == signature => FileProbe::Same,
                    _ => {
                        // Signature-only entries carry an empty digest; a
                        // later `digest()` still has to read the file.
                        // Never clobber an entry holding a digest.
                        self.entries
                            .entry(path.to_path_buf())
                            .or_insert(Entry { signature, digest: String::new() })
                            .signature = signature;
                        FileProbe::Changed
                    }
                }
            }
            Err(_) => {
                self.entries.remove(path);
                if self.absent.contains(path) {
                    FileProbe::Same
                } else {
                    FileProbe::Missing
                }
            }
        }
    }

    /// Record the current state without hashing, so the first probe after
    /// a fresh (re)load is `Same` rather than `Changed` — including
    /// absent state for manifests that legitimately don't exist.
    pub fn prime(&mut self, path: &Path) {
        if let Ok(signature) = stat_signature(path) {
            self.entries
                .entry(path.to_path_buf())
                .or_insert(Entry { signature, digest: String::new() })
                .signature = signature;
            self.absent.remove(path);
        } else {
            self.entries.remove(path);
            self.absent.insert(path.to_path_buf());
        }
    }

    /// blake3 hex of `path`'s current content, memoized on its signature.
    pub fn digest(&mut self, path: &Path) -> io::Result<String> {
        let signature = stat_signature(path)?;
        if let Some(entry) = self.entries.get(path)
            && !entry.digest.is_empty()
            && entry.signature == signature
            && !signature.recent(SystemTime::now())
        {
            return Ok(entry.digest.clone());
        }
        let digest = hash_file(path)?;
        // Re-stat: if the file moved while we hashed, the signature we
        // would memoize describes different bytes — return the digest
        // (it is honest) but do not trust it next time.
        if stat_signature(path).is_ok_and(|after| after == signature) {
            self.entries
                .insert(path.to_path_buf(), Entry { signature, digest: digest.clone() });
        } else {
            self.entries.remove(path);
        }
        Ok(digest)
    }

    /// Digest only when content actually changed since the last call;
    /// `None` on a memo hit, `Err` when the file vanished.
    pub fn digest_if_changed(&mut self, path: &Path) -> io::Result<Option<String>> {
        match self.probe(path) {
            FileProbe::Same => Ok(None),
            FileProbe::Changed => self.digest(path).map(Some),
            FileProbe::Missing => {
                Err(io::Error::new(io::ErrorKind::NotFound, "input disappeared"))
            }
        }
    }

    /// Forget one path (rename-out, delete).
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = File::open(path)?;
    io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn same_size_rewrite_rehashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");
        std::fs::write(&path, b"aaaaaaaa").unwrap();
        let mut memo = DigestMemo::new();
        let first = memo.digest(&path).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(&path, b"bbbbbbbb").unwrap();
        let second = memo.digest(&path).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn future_mtime_bypasses_memo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.txt");
        std::fs::write(&path, b"one").unwrap();
        let stamp = filetime::FileTime::from_unix_time(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + 3600,
            0,
        );
        filetime::set_file_mtime(&path, stamp).unwrap();
        let mut memo = DigestMemo::new();
        let _ = memo.digest(&path).unwrap();
        // Rewrite identical-size content with the same forced future mtime:
        // size+mtime+ino all match, but the recent/future rule rehashes.
        let mut f = File::create(&path).unwrap();
        f.write_all(b"two").unwrap();
        drop(f);
        filetime::set_file_mtime(&path, stamp).unwrap();
        let digest = memo.digest(&path).unwrap();
        assert_eq!(
            digest,
            blake3::hash(b"two").to_hex().to_string()
        );
    }

    #[test]
    fn probe_tracks_manifest_stats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("go.mod");
        std::fs::write(&path, b"module x\n").unwrap();
        let mut memo = DigestMemo::new();
        assert_eq!(memo.probe(&path), FileProbe::Changed);
        // Second probe with an unchanged signature is a hit only once the
        // file leaves the recent-write window; force the window closed.
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(&path, old).unwrap();
        assert_eq!(memo.probe(&path), FileProbe::Changed);
        assert_eq!(memo.probe(&path), FileProbe::Same);
        assert_eq!(memo.probe(&dir.path().join("nope")), FileProbe::Missing);
    }
}
