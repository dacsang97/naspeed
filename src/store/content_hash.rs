//! blake3 helpers for files and directory trees.

use std::io;
use std::path::Path;

pub fn hash_file(path: &Path) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize())
}

/// Hash a directory tree: sorted relative paths, file contents and symlink
/// targets. `skip` names (e.g. `.git`, `target`) are ignored at any depth.
pub fn hash_dir(root: &Path, skip: &[&str]) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hash_dir_into(root, root, skip, &mut hasher, &|p| hash_file(p))?;
    Ok(hasher.finalize())
}

/// Text files up to this size are path-normalised by `hash_dir_normalized`.
pub const TEXT_LIMIT: u64 = 1024 * 1024;

/// `hash_dir`, but small UTF-8 files are hashed after `encode` (placeholder
/// substitution). A build script's `OUT_DIR`
/// restored into another target dir has its pkg-config/cmake files
/// rewritten to the new paths; this keeps its hash identical.
pub fn hash_dir_normalized(
    root: &Path,
    encode: &dyn Fn(&str) -> String,
) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    let file_hash = |p: &Path| -> io::Result<blake3::Hash> {
        if std::fs::metadata(p)?.len() <= TEXT_LIMIT
            && let Ok(text) = std::fs::read_to_string(p)
        {
            return Ok(blake3::hash(encode(&text).as_bytes()));
        }
        hash_file(p)
    };
    hash_dir_into(root, root, &[], &mut hasher, &file_hash)?;
    Ok(hasher.finalize())
}

fn hash_dir_into(
    root: &Path,
    dir: &Path,
    skip: &[&str],
    hasher: &mut blake3::Hasher,
    file_hash: &dyn Fn(&Path) -> io::Result<blake3::Hash>,
) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name.to_str().is_some_and(|n| skip.contains(&n)) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            hasher.update(b"D\0");
            hasher.update(rel.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
            hash_dir_into(root, &path, skip, hasher, file_hash)?;
        } else if file_type.is_symlink() {
            hasher.update(b"L\0");
            hasher.update(rel.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
            hasher.update(std::fs::read_link(&path)?.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
        } else {
            hasher.update(b"F\0");
            hasher.update(rel.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
            hasher.update(file_hash(&path)?.as_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_hash_tracks_content_and_names_but_skips() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for d in [a.path(), b.path()] {
            std::fs::create_dir_all(d.join("src")).unwrap();
            std::fs::write(d.join("src/lib.rs"), "fn x() {}").unwrap();
        }
        std::fs::create_dir_all(b.path().join(".git")).unwrap();
        std::fs::write(b.path().join(".git/HEAD"), "ref").unwrap();
        let skip = [".git"];
        assert_eq!(
            hash_dir(a.path(), &skip).unwrap(),
            hash_dir(b.path(), &skip).unwrap()
        );
        std::fs::write(b.path().join("src/lib.rs"), "fn y() {}").unwrap();
        assert_ne!(
            hash_dir(a.path(), &skip).unwrap(),
            hash_dir(b.path(), &skip).unwrap()
        );
    }
}
