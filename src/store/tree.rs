//! Output trees: sorted relative paths plus object digest and mode, stored
//! content-addressed under `trees/<2>/<digest>.json`. Ingestion stages every
//! object first, verifies it, commits it read-only, then commits the tree —
//! the same ordering invariant as `objects/` before `entries/`.
//!
//! Path safety is enforced before *any* filesystem write: absolute paths,
//! `..`, `.`, alternate separators, non-portable characters, symlink
//! escape, and case-collision ambiguity are all rejected up front.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::Store;

pub const TREE_VERSION: u32 = 1;
/// Domain tag for the tree digest's canonical encoding.
const TREE_DOMAIN: &[u8] = b"naspeed/tree/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TreeEntry {
    /// Relative, `/`-separated, already validated by `validate_path`.
    pub path: String,
    pub kind: EntryKind,
    /// blake3 hex of the file content; empty for symlinks.
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    /// Unix permission bits; 0 where the platform has none.
    #[serde(default)]
    pub mode: u32,
    /// Symlink target (relative); empty for files.
    #[serde(default)]
    pub target: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tree {
    pub version: u32,
    /// Sorted by `path` (byte order); always normalized at construction.
    pub entries: Vec<TreeEntry>,
}

/// Why a path was rejected. Everything here is decided before write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Absolute,
    ParentTraversal,
    DotComponent,
    AlternateSeparator,
    BadComponent(String),
    CaseCollision(String),
    SymlinkEscape(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Empty => write!(f, "empty path"),
            PathError::Absolute => write!(f, "absolute path"),
            PathError::ParentTraversal => write!(f, "`..` component"),
            PathError::DotComponent => write!(f, "`.` component"),
            PathError::AlternateSeparator => write!(f, "backslash separator"),
            PathError::BadComponent(c) => write!(f, "non-portable component {c:?}"),
            PathError::CaseCollision(p) => write!(f, "case-colliding name {p:?}"),
            PathError::SymlinkEscape(p) => write!(f, "symlink escapes root: {p}"),
        }
    }
}

impl std::error::Error for PathError {}

/// Validate one relative path. Pure string checks — no filesystem access —
/// so callers can (and must) run it before writing anything.
pub fn validate_path(path: &str) -> std::result::Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(PathError::Absolute);
    }
    if path.contains('\\') {
        return Err(PathError::AlternateSeparator);
    }
    // Drive letters and other colon uses are non-portable.
    if path.contains(':') {
        return Err(PathError::BadComponent(path.into()));
    }
    let mut prev = "";
    for comp in path.split('/') {
        match comp {
            "" => return Err(PathError::Empty), // `//` or trailing `/`
            ".." => return Err(PathError::ParentTraversal),
            "." => return Err(PathError::DotComponent),
            _ => {}
        }
        if comp
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | '|' | '?' | '*' | '"'))
        {
            return Err(PathError::BadComponent(comp.into()));
        }
        prev = comp;
    }
    let _ = prev;
    Ok(())
}

/// Validate a whole candidate set: every path valid, and no two names in
/// the same directory colliding when compared case-insensitively (macOS /
/// Windows checkouts are typically case-insensitive; such a tree cannot be
/// materialized safely there).
pub fn validate_paths(paths: &[&str]) -> std::result::Result<(), PathError> {
    for p in paths {
        validate_path(p)?;
    }
    let mut parents: std::collections::BTreeMap<String, BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for p in paths {
        let mut dirs: Vec<&str> = p.split('/').collect();
        // Every ancestor directory is also a "name" inside its parent.
        while let Some(name) = dirs.pop() {
            let parent = dirs.join("/");
            let names = parents.entry(parent).or_default();
            let folded = name.to_lowercase();
            if let Some(existing) = names
                .iter()
                .find(|n| n.to_lowercase() == folded && **n != name)
            {
                return Err(PathError::CaseCollision(format!(
                    "{existing} vs {name}"
                )));
            }
            names.insert((*name).to_owned());
        }
    }
    Ok(())
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Canonical digest of a normalized tree.
pub fn tree_digest(entries: &[TreeEntry]) -> String {
    let mut out = Vec::new();
    out.extend_from_slice(TREE_DOMAIN);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        put_str(&mut out, &e.path);
        out.push(match e.kind {
            EntryKind::File => b'F',
            EntryKind::Symlink => b'L',
        });
        put_str(&mut out, &e.digest);
        out.extend_from_slice(&e.size.to_le_bytes());
        out.extend_from_slice(&e.mode.to_le_bytes());
        put_str(&mut out, &e.target);
    }
    blake3::hash(&out).to_hex().to_string()
}

/// Normalize (validate + sort) a caller-supplied entry list.
pub fn normalize(entries: &mut Vec<TreeEntry>) -> std::result::Result<(), PathError> {
    let refs: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    validate_paths(&refs)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    Ok(())
}

/// True if `link`'s target stays inside `root` after resolution. `link`
/// must be a symlink already inside `root`.
fn symlink_stays_inside(root: &Path, link: &Path) -> io::Result<bool> {
    let target = std::fs::read_link(link)?;
    if target.is_absolute() {
        return Ok(false);
    }
    let resolved = link
        .parent()
        .unwrap_or(root)
        .join(&target)
        .canonicalize()?;
    let root = root.canonicalize()?;
    Ok(resolved.starts_with(&root))
}

/// Ingest every file under `root` into `objects/` and commit a tree record.
/// All paths are validated and symlinks resolved *before* the first object
/// write. Returns the tree digest.
pub fn ingest_tree(store: &Store, root: &Path) -> Result<String> {
    let mut files: Vec<(String, PathBuf, EntryKind, String)> = Vec::new();
    collect(root, root, &mut files)?;

    // Validate everything before touching the store.
    let paths: Vec<&str> = files.iter().map(|(p, _, _, _)| p.as_str()).collect();
    validate_paths(&paths)?;
    for (rel, abs, kind, _) in &files {
        if *kind == EntryKind::Symlink
            && !symlink_stays_inside(root, abs)
                .with_context(|| format!("resolve {}", abs.display()))?
        {
            return Err(PathError::SymlinkEscape(rel.clone()).into());
        }
    }

    let mut entries = Vec::with_capacity(files.len());
    for (rel, abs, kind, target) in files {
        match kind {
            EntryKind::File => {
                let (digest, size) = store
                    .ingest_object(&abs)
                    .with_context(|| format!("ingest {}", abs.display()))?;
                let mode = std::fs::metadata(&abs)
                    .map(|m| crate::platform::mode_of(&m))
                    .unwrap_or(0o644);
                entries.push(TreeEntry {
                    path: rel,
                    kind,
                    digest,
                    size,
                    mode,
                    target: String::new(),
                });
            }
            EntryKind::Symlink => entries.push(TreeEntry {
                path: rel,
                kind,
                digest: String::new(),
                size: 0,
                mode: 0,
                target,
            }),
        }
    }
    write_tree(store, entries)
}

/// Commit a tree whose file objects were already placed in the store.
pub(crate) fn write_tree(store: &Store, mut entries: Vec<TreeEntry>) -> Result<String> {
    normalize(&mut entries)?;
    let digest = tree_digest(&entries);
    let tree = Tree {
        version: TREE_VERSION,
        entries,
    };
    let bytes = serde_json::to_vec(&tree)?;
    store.write_file(&store.tree_path(&digest), &bytes)?;
    Ok(digest)
}

fn collect(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf, EntryKind, String)>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect(root, &path, out)?;
        } else if ty.is_symlink() {
            let target = std::fs::read_link(&path)?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path, EntryKind::Symlink, target));
        } else {
            out.push((rel, path, EntryKind::File, String::new()));
        }
    }
    Ok(())
}

/// Read and fully verify a tree record plus every object it references.
/// Returns `None` on any inconsistency — untrusted bytes never produce a
/// partial tree.
pub fn read_verified(store: &Store, digest: &str) -> Option<Tree> {
    let bytes = std::fs::read(store.tree_path(digest)).ok()?;
    let tree: Tree = serde_json::from_slice(&bytes).ok()?;
    if tree.version != TREE_VERSION || tree_digest(&tree.entries) != digest {
        return None;
    }
    for e in &tree.entries {
        if e.kind == EntryKind::Symlink {
            if validate_path(&e.path).is_err() {
                return None;
            }
            continue;
        }
        if !super::action_cache::verify_object(store, &e.digest, e.size) {
            return None;
        }
    }
    Some(tree)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_paths() {
        for bad in [
            "",
            "/abs",
            "/",
            "a/../b",
            "..",
            "a/./b",
            "a\\b",
            "a//b",
            "a/",
            "C:/x",
            "a:b",
            "a\0b",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} accepted");
        }
        for good in ["a", "a/b/c.txt", ".hidden", "a..b", "x y/z-1_2.rs"] {
            assert!(validate_path(good).is_ok(), "{good:?} rejected");
        }
    }

    #[test]
    fn case_collision_detected_per_directory() {
        assert!(validate_paths(&["a/Foo.txt", "a/foo.txt"]).is_err());
        assert!(validate_paths(&["a/Foo", "a/foo/bar"]).is_err());
        // Same name in different directories is fine.
        assert!(validate_paths(&["a/Foo.txt", "b/foo.txt"]).is_ok());
    }
}
