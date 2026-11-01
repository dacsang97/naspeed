//! Replace byte-identical files across target dirs with clones of one copy.
//!
//! Most rlib/rmeta built before naspeed are not byte-identical across
//! target dirs (rmeta embeds its `OUT_DIR`), so this mainly recovers build
//! script output and other identical blobs; the big saving comes from
//! rebuilding through the store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use filetime::FileTime;

use crate::config::CowMode;
use crate::store::content_hash::hash_file;
use crate::store::cow_copy;
use crate::store::physical_usage::private_size;
use crate::wrapper::extern_identity::{PLACED_XATTR, XATTR_NAME};

pub const MIN_SIZE: u64 = 64 * 1024;

#[derive(Debug, Default, Clone)]
pub struct DedupStats {
    pub candidates: usize,
    pub replaced: usize,
    pub bytes_freed: u64,
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            walk(&e.path(), out);
        } else if t.is_file()
            && let Ok(m) = e.metadata()
            && m.len() >= MIN_SIZE
        {
            out.push((e.path(), m.len()));
        }
    }
}

fn copy_xattrs(from: &Path, to: &Path) {
    for name in [XATTR_NAME, PLACED_XATTR] {
        if let Some(value) = crate::platform::get_attr(from, name) {
            let _ = crate::platform::set_attr(to, name, &value);
        }
    }
}

/// Clone `keeper` over `dup` keeping `dup`'s mode, mtime and atime (cargo
/// compares mtimes; changing them would cause rebuilds).
fn replace_with_clone(keeper: &Path, dup: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(dup)?;
    let tmp = dup.with_file_name(format!(
        ".naspeed-dedup-{}-{}",
        std::process::id(),
        dup.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&tmp);
    if cow_copy::clone_or_copy(keeper, &tmp, CowMode::Auto)? != cow_copy::Method::Clone {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other("volume does not support clones"));
    }
    copy_xattrs(dup, &tmp);
    // Times before the mode: setting times needs write access, which a
    // read-only mode takes away on Windows.
    filetime::set_file_times(
        &tmp,
        FileTime::from_last_access_time(&meta),
        FileTime::from_last_modification_time(&meta),
    )?;
    std::fs::set_permissions(&tmp, meta.permissions())?;
    std::fs::rename(&tmp, dup).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `dirs` must already be locked against cargo by the caller.
pub fn dedup(dirs: &[PathBuf], dry_run: bool) -> Result<DedupStats> {
    let mut stats = DedupStats::default();
    let mut files = Vec::new();
    for dir in dirs {
        walk(dir, &mut files);
    }
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (path, size) in files {
        by_size.entry(size).or_default().push(path);
    }
    for (size, paths) in by_size {
        if paths.len() < 2 {
            continue;
        }
        let mut by_hash: HashMap<blake3::Hash, Vec<PathBuf>> = HashMap::new();
        for p in paths {
            if let Ok(h) = hash_file(&p) {
                by_hash.entry(h).or_default().push(p);
            }
        }
        for (_, mut group) in by_hash {
            if group.len() < 2 {
                continue;
            }
            group.sort();
            let keeper = group.remove(0);
            for dup in group {
                // Already fully shared with something: nothing to gain.
                if private_size(&dup) == Some(0) {
                    continue;
                }
                stats.candidates += 1;
                if dry_run {
                    stats.bytes_freed += size;
                    continue;
                }
                if replace_with_clone(&keeper, &dup).is_ok() {
                    stats.replaced += 1;
                    stats.bytes_freed += size;
                }
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_and_keeps_metadata() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a/debug/build/x-1/out");
        let b = t.path().join("b/debug/build/x-1/out");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let blob = vec![42u8; 200 * 1024];
        std::fs::write(a.join("gen.rs"), &blob).unwrap();
        std::fs::write(b.join("gen.rs"), &blob).unwrap();
        let old = FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(b.join("gen.rs"), old).unwrap();
        crate::platform::set_mode(&b.join("gen.rs"), 0o600).unwrap();
        let mode = crate::platform::mode_of(&std::fs::metadata(b.join("gen.rs")).unwrap());

        let stats = dedup(&[t.path().join("a"), t.path().join("b")], false).unwrap();
        if !cow_copy::supports_clone(t.path()) {
            return;
        }
        assert_eq!(stats.replaced, 1);
        let meta = std::fs::metadata(b.join("gen.rs")).unwrap();
        assert_eq!(FileTime::from_last_modification_time(&meta), old);
        assert_eq!(crate::platform::mode_of(&meta), mode);
        assert_eq!(std::fs::read(b.join("gen.rs")).unwrap(), blob);
        assert_eq!(private_size(&b.join("gen.rs")).unwrap_or(0), 0);
    }
}
