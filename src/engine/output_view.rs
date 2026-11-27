//! Output views: a disposable per-worktree manifestation of one output
//! tree. The canonical artifact stays in `objects/`; a view is only
//! metadata plus — when the filesystem supports it — name-bearing links.
//!
//! Backends, selected by a runtime capability probe, never by OS:
//!
//! - `Link`: one symlink per file, `worktree/<logical>` →
//!   `objects/<2>/<digest>` (Unix symlink or Windows file symlink/junction
//!   semantics via `platform::symlink`). Link targets keep their read-only
//!   CAS mode: a view can never make CAS bytes writable.
//! - `DirectPath`: no links at all. The engine writes
//!   `.ns/paths.map` (`<logical>\t<canonical>` lines) and `.ns/view.json`
//!   into the worktree; consumers resolve paths through `View::resolve` or
//!   the generated map. On filesystems where links must carry a tool-
//!   conventional name (rustc's `--extern` needs `lib*.rlib`), the
//!   canonical path is also available through `View::canonical`.
//!
//! Two manifest forms exist and only one contains absolute paths:
//! `.ns/paths.map` inside the worktree (local, never shared) and the
//! store-side bookkeeping record under `views/<project>/<view>/`. The
//! shared `.ns/view.json` lists logical paths and digests only, and
//! project/view ids are blake3 digests — no host paths leak into shared
//! metadata.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store::lease::{self, Lease, Purpose};
use crate::store::tree::{EntryKind, Tree};
use crate::store::{Store, now_secs};

pub const VIEW_VERSION: u32 = 1;
/// Directory inside a worktree that holds view metadata.
pub const NS_DIR: &str = ".ns";
/// `<logical>\t<canonical>` resolution map for the direct-path backend.
pub const PATHS_MAP: &str = "paths.map";

/// How a view exposes canonical files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Symlinks/junctions materialized into the worktree.
    Link,
    /// Nothing materialized: engine metadata resolves canonical paths.
    DirectPath,
}

/// One file a view exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewFile {
    /// Logical path inside the worktree, `/`-separated, already validated.
    pub path: String,
    /// Object digest the logical path resolves to.
    pub object: String,
    pub size: u64,
    /// Recorded mode; materialization never grants write.
    pub mode: u32,
}

/// Shared view record: logical paths + digests only, safe to replicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewManifest {
    pub version: u32,
    pub project: String,
    pub view: String,
    pub backend: Backend,
    /// Action key whose output tree this view materializes.
    pub action_key: String,
    /// Digest of the `trees/` record the files come from.
    pub output_tree: String,
    #[serde(default)]
    pub files: Vec<ViewFile>,
}

/// Store-side bookkeeping: extends the shared manifest with the worktree
/// location so status can measure it and recovery can find orphans.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewRecord {
    #[serde(flatten)]
    manifest: ViewManifest,
    /// Absolute worktree path. Local bookkeeping only — never part of the
    /// shared `.ns/view.json`.
    worktree: String,
    created: u64,
}

/// blake3 hex of `text` truncated to 16 chars: stable, reveals nothing.
fn id_of(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..16].to_owned()
}

/// Project id from canonical project identity (adapter-supplied; e.g.
/// manifest path relative to the VCS root). A hash, so worktree and store
/// paths never appear in shared metadata.
pub fn project_id(identity: &str) -> String {
    id_of(&format!("naspeed/project/1\0{identity}"))
}

/// View id for one worktree of `project`.
pub fn view_id(project: &str, worktree: &Path) -> String {
    let canon = crate::platform::canonicalize(worktree)
        .unwrap_or_else(|_| worktree.to_path_buf());
    id_of(&format!("naspeed/view/1\0{project}\0{}", canon.display()))
}

/// Try to create a symlink in `dir`: the backend probe. `DirectPath` is
/// the answer whenever linking fails, whatever the OS is.
pub fn detect_backend(dir: &Path) -> Backend {
    let target = dir.join(format!(".probe-t.{}", std::process::id()));
    let link = dir.join(format!(".probe-l.{}", std::process::id()));
    let ok = std::fs::write(&target, b"p").is_ok()
        && crate::platform::symlink(&target, &link).is_ok();
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&target);
    if ok { Backend::Link } else { Backend::DirectPath }
}

/// A resolved view: per-file canonical paths plus the lease that keeps
/// the artifacts alive for the holder's whole run.
pub struct View {
    manifest: ViewManifest,
    worktree: PathBuf,
    store: Store,
    lease: Lease,
}

impl View {
    /// The action key and view identity.
    pub fn action_key(&self) -> &str {
        &self.manifest.action_key
    }
    pub fn backend(&self) -> Backend {
        self.manifest.backend
    }
    /// The worktree-side metadata dir (`<worktree>/.ns`).
    pub fn ns_dir(&self) -> PathBuf {
        self.worktree.join(NS_DIR)
    }
    /// `<logical>\t<canonical>` map path (direct-path backend).
    pub fn map_path(&self) -> PathBuf {
        self.ns_dir().join(PATHS_MAP)
    }
    /// The shared, path-free manifest written into the worktree.
    pub fn manifest(&self) -> &ViewManifest {
        &self.manifest
    }

    /// Canonical object path for a logical view path.
    pub fn canonical(&self, logical: &str) -> Option<PathBuf> {
        let f = self.manifest.files.iter().find(|f| f.path == logical)?;
        Some(self.store.object_path(&f.object))
    }

    /// Where the consumer finds `logical`'s bytes:
    /// `worktree/<logical>` (link backend materialized it) or the
    /// canonical object path (direct-path backend).
    pub fn resolve(&self, logical: &str) -> Option<PathBuf> {
        match self.manifest.backend {
            Backend::Link => {
                self.manifest
                    .files
                    .iter()
                    .any(|f| f.path == logical)
                    .then(|| self.worktree.join(logical))
            }
            Backend::DirectPath => self.canonical(logical),
        }
    }

    /// Drop the lease and forget the view handle. Materialized links stay
    /// put (the worktree owns them); callers that are tearing the worktree
    /// down use `drop_view` to also clean store-side records.
    pub fn release(self) -> Lease {
        self.lease
    }
}

/// Manifest path inside the store for bookkeeping.
fn record_path(store: &Store, project: &str, view: &str) -> PathBuf {
    store
        .views_dir()
        .join(project)
        .join(view)
        .join("manifest.json")
}

fn view_files(tree: &Tree) -> Result<Vec<ViewFile>> {
    let mut files = Vec::new();
    for e in &tree.entries {
        match e.kind {
            EntryKind::File => files.push(ViewFile {
                path: e.path.clone(),
                object: e.digest.clone(),
                size: e.size,
                mode: e.mode,
            }),
            // A stored symlink resolves inside its own output tree —
            // expose the resolved entry's canonical object instead, so no
            // view ever creates a link the store did not produce.
            EntryKind::Symlink => {
                let target = normalize_link(&e.path, &e.target);
                let resolved = tree
                    .entries
                    .iter()
                    .find(|c| c.kind == EntryKind::File && c.path == target)
                    .ok_or_else(|| {
                        anyhow::anyhow!("symlink {} target {} not in tree", e.path, e.target)
                    })?;
                files.push(ViewFile {
                    path: e.path.clone(),
                    object: resolved.digest.clone(),
                    size: resolved.size,
                    mode: resolved.mode,
                });
            }
        }
    }
    Ok(files)
}

/// The tree-relative path a stored symlink points at: `target` relative
/// to the link's parent, `.`/`..` resolved.
fn normalize_link(link_path: &str, target: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(parent) = link_path.rsplit_once('/').map(|(p, _)| p)
        && !parent.is_empty()
    {
        parts.extend(parent.split('/'));
    }
    for comp in target.split('/') {
        match comp {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// Materialize `tree` (the verified output tree of a hit on `key`) into
/// `worktree`. Acquires the view lease first, probes the filesystem for
/// link support, writes `.ns/` metadata, and on the link backend links
/// every file. If any link fails the view falls back to direct-path —
/// partial links are removed so the worktree is never half-materialized.
pub fn materialize(
    store: &Store,
    tree: &Tree,
    tree_digest: &str,
    action_key: &str,
    project_identity: &str,
    worktree: &Path,
) -> Result<View> {
    let lease = lease::acquire(store, action_key, Purpose::View, &[])?;
    let project = project_id(project_identity);
    let view = view_id(&project, worktree);
    let files = view_files(tree)?;
    let backend = detect_backend(worktree);

    let manifest = ViewManifest {
        version: VIEW_VERSION,
        project: project.clone(),
        view: view.clone(),
        backend,
        action_key: action_key.to_owned(),
        output_tree: tree_digest.to_owned(),
        files,
    };

    let mut backend = backend;
    if backend == Backend::Link {
        if let Err(e) = link_all(store, &manifest, worktree) {
            // Filesystem said yes to the probe but a link failed: back out
            // and serve direct paths instead of leaving a half view.
            let _ = remove_links(&manifest, worktree);
            backend = Backend::DirectPath;
            let _ = e;
        }
    }
    let manifest = ViewManifest { backend, ..manifest };

    let ns = worktree.join(NS_DIR);
    std::fs::create_dir_all(&ns).with_context(|| format!("create {}", ns.display()))?;
    store.write_file(&ns.join("view.json"), &serde_json::to_vec(&manifest)?)?;

    if backend == Backend::DirectPath {
        let mut map = String::new();
        for f in &manifest.files {
            map.push_str(&format!(
                "{}\t{}\n",
                f.path,
                store.object_path(&f.object).display()
            ));
        }
        // paths.map is local tool config — it carries absolute canonical
        // paths, which the shared view.json deliberately does not.
        std::fs::write(ns.join(PATHS_MAP), map)?;
    }

    // Store-side bookkeeping: findable by status and by orphan recovery.
    let record = ViewRecord {
        manifest: manifest.clone(),
        worktree: crate::platform::canonicalize(worktree)
            .unwrap_or_else(|_| worktree.to_path_buf())
            .to_string_lossy()
            .into_owned(),
        created: now_secs(),
    };
    store.write_file(
        &record_path(store, &project, &view),
        &serde_json::to_vec(&record)?,
    )?;

    Ok(View {
        manifest,
        worktree: worktree.to_path_buf(),
        store: store.clone(),
        lease,
    })
}

fn link_all(store: &Store, manifest: &ViewManifest, worktree: &Path) -> Result<()> {
    for f in &manifest.files {
        let dst = worktree.join(&f.path);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::symlink_metadata(&dst).is_ok() {
            let _ = crate::platform::remove_file(&dst);
        }
        crate::platform::symlink(store.object_path(&f.object), &dst)
            .with_context(|| format!("link {}", dst.display()))?;
    }
    Ok(())
}

fn remove_links(manifest: &ViewManifest, worktree: &Path) -> Result<()> {
    for f in &manifest.files {
        let dst = worktree.join(&f.path);
        if std::fs::symlink_metadata(&dst)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&dst);
        }
    }
    Ok(())
}

/// Resume a view in an existing worktree: validates the shared manifest
/// and takes a fresh lease. `None` when no view is present; unparseable
/// manifests are removed rather than trusted.
pub fn resume(store: &Store, worktree: &Path) -> Result<Option<View>> {
    let path = worktree.join(NS_DIR).join("view.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(None);
    };
    let manifest: ViewManifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(_) => {
            let _ = crate::platform::remove_file(&path);
            return Ok(None);
        }
    };
    if manifest.version != VIEW_VERSION
        || manifest.files.iter().any(|f| {
            crate::store::tree::validate_path(&f.path).is_err()
                || !crate::store::action_cache::verify_object(store, &f.object, f.size)
        })
    {
        let _ = crate::platform::remove_file(&path);
        return Ok(None);
    }
    let lease = lease::acquire(store, &manifest.action_key, Purpose::View, &[])?;
    Ok(Some(View {
        manifest,
        worktree: worktree.to_path_buf(),
        store: store.clone(),
        lease,
    }))
}

/// Worktree teardown: remove the view's materialized links and `.ns/`,
/// the store-side record, and the lease. The canonical artifact and other
/// worktrees' views are untouched.
pub fn drop_view(view: View) -> Result<()> {
    let _ = remove_links(&view.manifest, &view.worktree);
    let _ = std::fs::remove_dir_all(view.ns_dir());
    let _ = crate::platform::remove_file(record_path(
        &view.store,
        &view.manifest.project,
        &view.manifest.view,
    ));
    view.lease.release();
    Ok(())
}

/// Aggregate byte accounting for every recorded view.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ViewsUsage {
    pub count: u64,
    /// Bytes the views expose (sum of their output sizes).
    pub exposed: u64,
    /// Bytes the store spends on view bookkeeping manifests.
    pub manifests: u64,
    /// Bytes the materializations allocate inside worktrees: `.ns/`
    /// metadata plus link nodes — never artifact payloads.
    pub allocated: u64,
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_files(&p, out),
            Ok(_) => out.push(p),
            Err(_) => {}
        }
    }
}

/// Scan `views/`, sum bookkeeping and — for worktrees still present —
/// on-disk materialization costs.
pub fn usage(store: &Store) -> ViewsUsage {
    let mut u = ViewsUsage::default();
    let mut records = Vec::new();
    walk_files(&store.views_dir(), &mut records);
    for path in records {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        u.manifests += size;
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<ViewRecord>(&bytes) else {
            continue;
        };
        u.count += 1;
        u.exposed += record.manifest.files.iter().map(|f| f.size).sum::<u64>();
        let worktree = PathBuf::from(&record.worktree);
        // Allocated = what the materialization costs on disk: .ns
        // metadata plus link inodes. Payload bytes are never counted —
        // views do not store them.
        let mut view_files_on_disk = Vec::new();
        walk_files(&worktree.join(NS_DIR), &mut view_files_on_disk);
        u.allocated += view_files_on_disk
            .iter()
            .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum::<u64>();
        if record.manifest.backend == Backend::Link {
            u.allocated += record.manifest.files.len() as u64; // link nodes ≈ 1 block-free
        }
    }
    u
}

/// Startup recovery: bookkeeping records whose worktree (or `.ns`) is
/// gone are orphans; remove them after `grace`. Returns removed dirs.
pub fn sweep_orphans(store: &Store, grace: Duration, dry_run: bool) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(projects) = std::fs::read_dir(store.views_dir()) else {
        return removed;
    };
    for project in projects.flatten() {
        let Ok(views) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for view in views.flatten() {
            let dir = view.path();
            let manifest_path = dir.join("manifest.json");
            let Ok(bytes) = std::fs::read(&manifest_path) else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<ViewRecord>(&bytes) else {
                // Unparseable bookkeeping: safe to drop once old.
                if older_than(&manifest_path, grace) {
                    removed.push(dir.clone());
                    if !dry_run {
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
                continue;
            };
            let worktree_gone = !PathBuf::from(&record.worktree)
                .join(NS_DIR)
                .join("view.json")
                .exists();
            if worktree_gone && older_than(&manifest_path, grace) {
                removed.push(dir.clone());
                if !dry_run {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
    }
    removed
}

fn older_than(path: &Path, age: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    meta.modified()
        .map(|m| m.elapsed().map(|e| e >= age).unwrap_or(false))
        .unwrap_or(false)
}
