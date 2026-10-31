//! Skip `cargo fmt` for packages whose sources are already known to be
//! formatted. After a clean run naspeed records the content hash of every
//! `.rs` file of the packages it formatted; a later run hashes the files
//! again and only hands packages with an unknown hash to `cargo fmt -p`.
//!
//! The set lives in the store (`memo/fmt-<key>.list`) and is keyed by the
//! rustfmt version and the workspace's rustfmt config, so it is shared by
//! every worktree of a project and dropped when either changes. A file's
//! entry also covers its package edition. Files are found per package dir
//! (gitignore-aware, nested members and the target dir excluded); a module
//! pulled in from outside its package with `#[path]` is not tracked.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::project::WorkspaceGraph;
use crate::store::content_hash::hash_file;

/// More entries than this and the list is compacted on the next write.
const MAX_ENTRIES: usize = 200_000;

pub struct FmtCache {
    path: PathBuf,
    known: HashSet<String>,
}

fn rustfmt_version() -> Result<String> {
    let out = Command::new(std::env::var_os("RUSTFMT").unwrap_or_else(|| "rustfmt".into()))
        .arg("--version")
        .output()
        .context("run rustfmt --version")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// rustfmt version plus every rustfmt config at the root or in a member dir.
fn cache_key(graph: &WorkspaceGraph) -> Result<String> {
    let mut h = blake3::Hasher::new();
    h.update(rustfmt_version()?.as_bytes());
    let dirs = std::iter::once(&graph.root).chain(graph.members.iter().map(|m| &m.dir));
    for dir in dirs {
        for name in ["rustfmt.toml", ".rustfmt.toml"] {
            if let Ok(bytes) = std::fs::read(dir.join(name)) {
                h.update(dir.join(name).to_string_lossy().as_bytes());
                h.update(&bytes);
            }
        }
    }
    Ok(h.finalize().to_hex()[..16].to_string())
}

/// `.rs` files of one package dir, without nested members and target dirs.
pub fn package_files(graph: &WorkspaceGraph, dir: &Path) -> Vec<PathBuf> {
    let nested: Vec<PathBuf> = graph
        .members
        .iter()
        .map(|m| m.dir.clone())
        .filter(|d| d != dir && d.starts_with(dir))
        .collect();
    let target = graph.target_dir.clone();
    let mut files: Vec<PathBuf> = WalkBuilder::new(dir)
        .require_git(false)
        .filter_entry(move |e| {
            let p = e.path();
            !p.starts_with(&target) && !nested.iter().any(|n| p.starts_with(n))
        })
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    files
}

fn entry(edition: &str, file: &Path) -> Option<String> {
    let content = hash_file(file).ok()?;
    let mut h = blake3::Hasher::new();
    h.update(edition.as_bytes());
    h.update(content.as_bytes());
    Some(h.finalize().to_hex().to_string())
}

impl FmtCache {
    pub fn open(store_root: &Path, graph: &WorkspaceGraph) -> Result<FmtCache> {
        let path = store_root
            .join("memo")
            .join(format!("fmt-{}.list", cache_key(graph)?));
        let known = std::fs::read_to_string(&path)
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(FmtCache { path, known })
    }

    /// Members with at least one file not known to be formatted.
    pub fn dirty_members<'g>(&self, graph: &'g WorkspaceGraph) -> Vec<&'g str> {
        graph
            .members
            .iter()
            .filter(|m| {
                package_files(graph, &m.dir)
                    .iter()
                    .any(|f| entry(&m.edition, f).is_none_or(|e| !self.known.contains(&e)))
            })
            .map(|m| m.name.as_str())
            .collect()
    }

    /// Record the current content of `members`' files as formatted.
    pub fn record(&mut self, graph: &WorkspaceGraph, members: &[&str]) -> Result<()> {
        let mut new = Vec::new();
        for m in graph
            .members
            .iter()
            .filter(|m| members.contains(&m.name.as_str()))
        {
            for f in package_files(graph, &m.dir) {
                if let Some(e) = entry(&m.edition, &f)
                    && self.known.insert(e.clone())
                {
                    new.push(e);
                }
            }
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.known.len() > MAX_ENTRIES {
            // Compact: keep only what this workspace has now.
            let keep: HashSet<String> = graph
                .members
                .iter()
                .flat_map(|m| {
                    package_files(graph, &m.dir)
                        .into_iter()
                        .filter_map(|f| entry(&m.edition, &f))
                        .collect::<Vec<_>>()
                })
                .filter(|e| self.known.contains(e))
                .collect();
            let tmp = self
                .path
                .with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(
                &tmp,
                keep.iter().map(|e| format!("{e}\n")).collect::<String>(),
            )?;
            std::fs::rename(&tmp, &self.path)?;
            self.known = keep;
            return Ok(());
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(
            new.iter()
                .map(|e| format!("{e}\n"))
                .collect::<String>()
                .as_bytes(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_packages_are_clean_and_edits_dirty_them() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project");
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let status = Command::new("cp")
            .args(["-R"])
            .arg(&fixture)
            .arg(&ws)
            .status()
            .unwrap();
        assert!(status.success());
        let graph = WorkspaceGraph::load(&ws).unwrap();
        let store = dir.path().join("store");
        let mut cache = FmtCache::open(&store, &graph).unwrap();
        assert_eq!(cache.dirty_members(&graph).len(), graph.members.len());

        let all: Vec<&str> = graph.members.iter().map(|m| m.name.as_str()).collect();
        cache.record(&graph, &all).unwrap();
        let cache = FmtCache::open(&store, &graph).unwrap();
        assert!(cache.dirty_members(&graph).is_empty());

        let lib = ws.join("crates/greeter/src/lib.rs");
        let text = std::fs::read_to_string(&lib).unwrap();
        std::fs::write(&lib, format!("{text}\n// edited\n")).unwrap();
        assert_eq!(cache.dirty_members(&graph), vec!["greeter"]);
    }
}
