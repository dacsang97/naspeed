//! Atomic read/write of `ActionResult` records under `actions/`, with full
//! verification before a record is visible as a hit. Records are untrusted
//! bytes: anything that fails to validate is a typed cache miss and the
//! record is removed (quarantined by deletion), never served.
//!
//! Ordering invariant (same as the legacy store): every object and tree a
//! record references is committed read-only *before* the record lands.
//! Writers of the same action key serialize on a per-key lock under
//! `leases/<key>/`.

use std::io;
use std::path::Path;

use anyhow::Result;

use super::{key_lock, tree, Store};
use crate::engine::action_result::{ActionResult, ACTION_RESULT_VERSION};

/// Outcome of `lookup`: hit, or a typed miss (never a partially-valid
/// record). Fail-closed: every reason below removes/quarantines the record
/// except `NotCached`/`Uncacheable`/`Failed`, where no bad record exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    /// No record on disk.
    NotCached,
    /// Record bytes did not parse or contained unknown fields.
    Malformed,
    /// Version we do not support.
    UnknownSchema(u32),
    /// The key in the record did not match the requested key.
    KeyMismatch,
    /// The output tree record was missing or failed verification.
    BadTree,
    /// A referenced object was missing, wrong size, or wrong digest.
    BadObject,
    /// The action ran and failed; observable but never a hit.
    Failed(i32),
    /// The action was never marked cacheable.
    Uncacheable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Hit(ActionResult, tree::Tree),
    Miss(Miss),
}

impl Lookup {
    pub fn is_hit(&self) -> bool {
        matches!(self, Lookup::Hit(..))
    }
}

/// Read `key`'s record and verify everything it references before returning
/// `Hit`. Any inconsistency is a typed miss; the offending record or tree is
/// removed so a later lookup does not keep re-reading corrupt bytes.
pub fn lookup(store: &Store, key: &str) -> Lookup {
    let path = store.action_path(key);
    let Ok(bytes) = std::fs::read(&path) else {
        return Lookup::Miss(Miss::NotCached);
    };
    let result: ActionResult = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => {
            quarantine(&path);
            return Lookup::Miss(Miss::Malformed);
        }
    };
    if result.version != ACTION_RESULT_VERSION {
        quarantine(&path);
        return Lookup::Miss(Miss::UnknownSchema(result.version));
    }
    if result.action_key != key {
        quarantine(&path);
        return Lookup::Miss(Miss::KeyMismatch);
    }
    // Verify every object the record references, tree included.
    let output_tree = if result.output_tree.is_empty() {
        tree::Tree {
            version: tree::TREE_VERSION,
            entries: vec![],
        }
    } else {
        let Some(t) = tree::read_verified(store, &result.output_tree) else {
            quarantine(&path);
            return Lookup::Miss(Miss::BadTree);
        };
        t
    };
    if !verify_streams(store, &result) {
        quarantine(&path);
        return Lookup::Miss(Miss::BadObject);
    }
    if !result.is_hit() {
        // Observable but never a hit; the record stays for diagnostics.
        return Lookup::Miss(Miss::Failed(result.exit_status));
    }
    Lookup::Hit(result, output_tree)
}

fn verify_streams(store: &Store, result: &ActionResult) -> bool {
    for s in [&result.stdout, &result.stderr].into_iter().flatten() {
        if !verify_object(store, &s.digest, s.size) {
            return false;
        }
    }
    true
}

/// Verify one object exists, with matching size and blake3 digest.
/// Re-hashes — the digest is the trust boundary, not the filename.
pub fn verify_object(store: &Store, digest: &str, size: u64) -> bool {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let path = store.object_path(digest);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !meta.is_file() || meta.len() != size {
        return false;
    }
    super::content_hash::hash_file(&path)
        .map(|h| h.to_hex().as_str() == digest)
        .unwrap_or(false)
}

fn quarantine(path: &Path) {
    let _ = crate::platform::remove_file(path);
}

/// Write a stream's bytes as a store object and return its `StreamRef`
/// shape (digest, size) — same ordering invariant as everything else:
/// object first, record later.
pub fn put_bytes(
    store: &Store,
    bytes: &[u8],
) -> Result<crate::engine::action_result::StreamRef> {
    let digest = blake3::hash(bytes).to_hex().to_string();
    if !verify_object(store, &digest, bytes.len() as u64) {
        let tmp = store.tmp_path("a");
        std::fs::write(&tmp, bytes)?;
        crate::platform::set_mode(&tmp, 0o444)?;
        store.install_object(&tmp, &digest)?;
    }
    Ok(crate::engine::action_result::StreamRef {
        digest,
        size: bytes.len() as u64,
    })
}

/// Per-action-key write lock under `leases/<key>/`. Serializes concurrent
/// writers of the same key so at most one valid record lands and no
/// partial record is ever visible.
fn action_lock(store: &Store, key: &str) -> io::Result<key_lock::FileLock> {
    key_lock::lock(&store.action_lock_path(key), key_lock::Mode::Exclusive)
}

/// Commit a finished result: verify every referenced object/tree exists
/// and hashes correctly *before* the record becomes visible, then write it
/// under the key's lock. A prior valid hit for the same key wins (writers
/// of the same key are interchangeable); a bad prior record is replaced.
///
/// `cacheable = false` and failed actions are still written (they stay
/// observable) — the contract is on `lookup`, which never hits for them.
pub fn store_result(store: &Store, key: &str, result: &ActionResult) -> Result<()> {
    let _lock = action_lock(store, key)?;
    debug_assert!(result.action_key == key);
    // Verify referential integrity before making the record visible.
    if !result.output_tree.is_empty() && tree::read_verified(store, &result.output_tree).is_none()
    {
        anyhow::bail!("output tree {} not verified", result.output_tree);
    }
    if !verify_streams(store, result) {
        anyhow::bail!("referenced stream object not verified");
    }
    if matches!(lookup(store, key), Lookup::Hit(..)) {
        return Ok(()); // someone else already landed a valid hit
    }
    let mut result = result.clone();
    result.version = ACTION_RESULT_VERSION;
    let bytes = serde_json::to_vec(&result)?;
    store.write_file(&store.action_path(key), &bytes)?;
    Ok(())
}

/// Serialize for callers that need record bytes (tests, transfers).
pub fn record_bytes(result: &ActionResult) -> Result<Vec<u8>> {
    serde_json::to_vec(result).map_err(Into::into)
}

/// Parse without trusting — mirrors `lookup`'s fail-closed checks but
/// returns the typed miss reason for diagnostics.
pub fn validate_record(bytes: &[u8], key: &str) -> Result<ActionResult, Miss> {
    let r: ActionResult = serde_json::from_slice(bytes).map_err(|_| Miss::Malformed)?;
    if r.version != ACTION_RESULT_VERSION {
        return Err(Miss::UnknownSchema(r.version));
    }
    if r.action_key != key {
        return Err(Miss::KeyMismatch);
    }
    Ok(r)
}

