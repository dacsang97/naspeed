//! Identity of an `--extern` artifact for the cache key.
//!
//! The identity is the blake3 of the file content, memoised in the xattr
//! `user.naspeed.id` (an alternate data stream on Windows) as
//! `<hash>:<size>:<mtime_ns>`. Every artifact naspeed
//! restores is a clone of one store object, so the same dependency has the
//! same identity in every target dir; artifacts built twice independently
//! (they embed their own `OUT_DIR` paths) differ and simply miss.

use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::store::content_hash::hash_file;

pub const XATTR_NAME: &str = "user.naspeed.id";

fn stat_stamp(path: &Path) -> io::Result<(u64, u128)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok((meta.len(), mtime))
}

pub fn read_xattr(path: &Path) -> Option<String> {
    String::from_utf8(crate::platform::get_attr(path, XATTR_NAME)?).ok()
}

/// The memoised hash if the file has not changed since it was stamped.
fn memoised(path: &Path) -> Option<String> {
    let value = read_xattr(path)?;
    let mut parts = value.split(':');
    let (hash, size, mtime) = (parts.next()?, parts.next()?, parts.next()?);
    let (cur_size, cur_mtime) = stat_stamp(path).ok()?;
    (size.parse::<u64>().ok()? == cur_size && mtime.parse::<u128>().ok()? == cur_mtime)
        .then(|| hash.to_owned())
}

/// Record `hash` (the file's content hash) on `path`. Best effort: a volume
/// without xattr support just loses the memo.
pub fn stamp(path: &Path, hash: &str) -> io::Result<()> {
    let (size, mtime) = stat_stamp(path)?;
    let value = format!("{hash}:{size}:{mtime}");
    crate::platform::set_attr(path, XATTR_NAME, value.as_bytes())
}

pub fn identity(path: &Path) -> io::Result<String> {
    if let Some(hash) = memoised(path) {
        return Ok(hash);
    }
    let hash = hash_file(path)?.to_hex().to_string();
    let _ = stamp(path, &hash);
    Ok(hash)
}

/// Marks files naspeed restored from the store (not ones rustc wrote), so
/// `purge` can find exactly those.
pub const PLACED_XATTR: &str = "user.naspeed.placed";

pub fn mark_placed(path: &Path) -> io::Result<()> {
    crate::platform::set_attr(path, PLACED_XATTR, b"1")
}

pub fn is_placed(path: &Path) -> bool {
    crate::platform::get_attr(path, PLACED_XATTR).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_content_hash_and_memo_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("libx.rmeta");
        std::fs::write(&p, b"one").unwrap();
        let first = identity(&p).unwrap();
        assert_eq!(first, blake3::hash(b"one").to_hex().to_string());

        // Rewriting changes size/mtime, so the stale memo must not be used.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&p, b"two!").unwrap();
        assert_eq!(
            identity(&p).unwrap(),
            blake3::hash(b"two!").to_hex().to_string()
        );
    }
}
