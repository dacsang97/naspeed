//! Private staging directories and the commit protocol.
//!
//! A compiler action never writes to a view or the CAS: `begin` allocates a
//! unique `staging/<execution-id>/` on the store volume (the lease id doubles
//! as the execution id, so `store::lease::id_active` decides liveness) and
//! the action writes there only. `finish` then:
//!
//! 1. compares what is on disk with the action's declared outputs —
//!    undeclared files mean the tree cannot be trusted: nothing is ingested,
//!    nothing is published, the native outcome is returned intact;
//! 2. ingests declared files into `objects/`, writes the `trees/` record,
//!    ingests captured streams, and publishes the `ActionResult` under the
//!    per-key write lock — the objects → tree → record order means no
//!    partial result is ever visible.
//!
//! Same-store rename: objects commit by rename inside the store, so the
//! staging dir must live on the store volume; `begin` enforces that by
//! construction (`Store::staging_dir`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::engine::action::ActionSpec;
use crate::engine::action_result::{ActionResult, Provenance, Resources, StreamRef};
use crate::store::lease::{self, Lease, Purpose};
use crate::store::tree::{self, EntryKind, PathError, TreeEntry, validate_paths};
use crate::store::{Store, action_cache, now_secs};

/// What the action did natively, before any cache decision.
pub struct NativeOutcome {
    /// Process exit code; negative when killed by a signal on Unix.
    pub exit_status: i32,
    pub duration_ms: u64,
    /// Captured streams, ingested as objects on publish.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub resources: Resources,
    pub provenance: Provenance,
}

/// How `finish` ended.
pub enum Finish {
    /// The result record is visible under the action key.
    Published(ActionResult),
    /// Files outside the declared output set were produced: rejected for
    /// publication. The native outcome is preserved untouched; no object
    /// was ingested, no record written.
    Rejected {
        native: NativeOutcome,
        undeclared: Vec<String>,
    },
}

/// A private execution directory plus the lease that owns it.
/// Drop removes the directory; only `finish` commits.
pub struct Staging {
    store: Store,
    dir: PathBuf,
    lease: Option<Lease>,
    declared: Vec<String>,
}

impl Staging {
    /// The directory the action must write to — private, unique, empty.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Lease id; `staging/<id>` under the store root.
    pub fn execution_id(&self) -> &str {
        self.lease.as_ref().map(|l| l.id()).unwrap_or_default()
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Allocate a staging dir for `spec`'s run. Declared output paths are
/// validated up front (a bad declaration fails before the run, not after).
/// The lease is held before the directory exists: a GC sweep racing the
/// gap cannot reap a live execution's dir.
pub fn begin(store: &Store, spec: &ActionSpec) -> Result<Staging> {
    let declared: Vec<String> = spec.outputs.iter().map(|o| o.path.clone()).collect();
    let refs: Vec<&str> = declared.iter().map(String::as_str).collect();
    validate_paths(&refs).context("declared outputs are not portable paths")?;

    let key = spec.key();
    let lease = lease::acquire(store, &key, Purpose::Run, &[])?;
    let dir = store.staging_dir().join(lease.id());
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create staging dir {}", dir.display()))?;
    Ok(Staging {
        store: store.clone(),
        dir,
        lease: Some(lease),
        declared,
    })
}

/// One disk entry found under the staging dir, in logical-path form.
struct DiskEntry {
    rel: String,
    abs: PathBuf,
    kind: EntryKind,
    target: String,
}

/// Recursively collect files/symlinks under `path` (a file or directory),
/// with `rel` as the logical path of `path` itself.
fn collect_entry(root: &Path, rel: &str, path: &Path, out: &mut Vec<DiskEntry>) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?
            .to_string_lossy()
            .replace('\\', "/");
        out.push(DiskEntry {
            rel: rel.to_owned(),
            abs: path.to_path_buf(),
            kind: EntryKind::Symlink,
            target,
        });
        return Ok(());
    }
    if meta.is_dir() {
        for e in std::fs::read_dir(path)? {
            let e = e?;
            let child_rel = format!("{}/{}", rel, e.file_name().to_string_lossy());
            collect_entry(root, &child_rel, &e.path(), out)?;
        }
        return Ok(());
    }
    if meta.is_file() {
        out.push(DiskEntry {
            rel: rel.to_owned(),
            abs: path.to_path_buf(),
            kind: EntryKind::File,
            target: String::new(),
        });
    }
    // Anything else (fifo, socket) cannot be an output: ignored here and
    // caught as undeclared when it appears at top level.
    let _ = root;
    Ok(())
}

/// Every filesystem entry directly under `root` as a logical path.
fn top_level(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            out.push(e.file_name().to_string_lossy().replace('\\', "/"));
        }
    }
    out
}

/// `link` is a symlink inside `root`; does its target stay inside?
fn symlink_stays_inside(root: &Path, link: &Path) -> std::io::Result<bool> {
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

/// Commit `native`'s on-disk outputs. On success publishes the record;
/// undeclared outputs (or, on a failed run, missing declared files)
/// reject publication without touching the store.
pub fn finish(
    mut staging: Staging,
    spec: &ActionSpec,
    native: NativeOutcome,
) -> Result<Finish> {
    let key = spec.key();

    // Every file/symlink on disk must sit under a declared output —
    // anything else is an undeclared write and the tree is rejected.
    let covered = |rel: &str| {
        staging
            .declared
            .iter()
            .any(|d| rel == d || rel.starts_with(&format!("{d}/")))
    };
    let mut undeclared: Vec<String> = Vec::new();
    for t in top_level(&staging.dir) {
        let p = staging.dir.join(&t);
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if !meta.is_file() && !meta.is_dir() && !meta.file_type().is_symlink() {
            // fifo, socket, device: never a valid output.
            undeclared.push(t);
            continue;
        }
        let mut all: Vec<DiskEntry> = Vec::new();
        collect_entry(&staging.dir, &t, &p, &mut all)
            .with_context(|| format!("read output {t}"))?;
        undeclared.extend(
            all.iter()
                .filter(|e| !covered(&e.rel))
                .map(|e| e.rel.clone()),
        );
    }
    if !undeclared.is_empty() {
        let _ = std::fs::remove_dir_all(&staging.dir);
        if let Some(l) = staging.lease.take() { l.release(); }
        return Ok(Finish::Rejected {
            native,
            undeclared,
        });
    }

    // Collect entries under each declared path that exists.
    let mut disk: Vec<DiskEntry> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for rel in &staging.declared {
        let abs = staging.dir.join(rel);
        if std::fs::symlink_metadata(&abs).is_err() {
            missing.push(rel.clone());
            continue;
        }
        collect_entry(&staging.dir, rel, &abs, &mut disk)
            .with_context(|| format!("read declared output {rel}"))?;
    }

    if native.exit_status == 0 && !missing.is_empty() {
        // A successful action that did not produce what it declared is not
        // cacheable output; report it as rejected-for-publication.
        let _ = std::fs::remove_dir_all(&staging.dir);
        if let Some(l) = staging.lease.take() { l.release(); }
        return Ok(Finish::Rejected {
            native,
            undeclared: missing
                .into_iter()
                .map(|m| format!("declared output missing: {m}"))
                .collect(),
        });
    }
    // Failed actions publish whatever they produced (observability); a
    // missing declared output is only suspicious on success.

    // Symlink safety, identical to tree::ingest_tree.
    for e in &disk {
        if e.kind == EntryKind::Symlink
            && !symlink_stays_inside(&staging.dir, &e.abs)
                .with_context(|| format!("resolve {}", e.abs.display()))?
        {
            anyhow::bail!(PathError::SymlinkEscape(e.rel.clone()));
        }
    }

    // Objects first, then the tree record, then the action record.
    let mut entries = Vec::with_capacity(disk.len());
    for e in disk {
        match e.kind {
            EntryKind::File => {
                let (digest, size) = staging
                    .store
                    .ingest_object(&e.abs)
                    .with_context(|| format!("ingest {}", e.abs.display()))?;
                let mode = std::fs::metadata(&e.abs)
                    .map(|m| crate::platform::mode_of(&m))
                    .unwrap_or(0o644);
                entries.push(TreeEntry {
                    path: e.rel,
                    kind: e.kind,
                    digest,
                    size,
                    mode,
                    target: String::new(),
                });
            }
            EntryKind::Symlink => entries.push(TreeEntry {
                path: e.rel,
                kind: e.kind,
                digest: String::new(),
                size: 0,
                mode: 0,
                target: e.target,
            }),
        }
    }
    let output_tree = if entries.is_empty() {
        String::new()
    } else {
        tree::write_tree(&staging.store, entries).context("commit staged output tree")?
    };

    let stream_ref = |bytes: &[u8]| -> Result<Option<StreamRef>> {
        if bytes.is_empty() {
            Ok(None)
        } else {
            action_cache::put_bytes(&staging.store, bytes).map(Some)
        }
    };
    let result = ActionResult {
        version: crate::engine::action_result::ACTION_RESULT_VERSION,
        action_key: key.clone(),
        exit_status: native.exit_status,
        stdout: stream_ref(&native.stdout)?,
        stderr: stream_ref(&native.stderr)?,
        output_tree,
        duration_ms: native.duration_ms,
        resources: native.resources,
        provenance: native.provenance,
        created: now_secs(),
    };
    action_cache::store_result(&staging.store, &key, &result)?;

    let _ = std::fs::remove_dir_all(&staging.dir);
    if let Some(l) = staging.lease.take() { l.release(); }
    Ok(Finish::Published(result))
}

/// Recovery sweep: staging dirs whose owning lease is gone are abandoned.
/// `grace` covers the acquire→mkdir gap (the lease is held before the dir
/// exists, so only a dir already past grace without a live lease is swept).
/// Returns removed dirs.
pub fn recover_abandoned(store: &Store, grace: Duration, dry_run: bool) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(store.staging_dir()) else {
        return removed;
    };
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let id = e.file_name().to_string_lossy().into_owned();
        if lease::id_active(store, &id, grace) || !older_than(&dir, grace) {
            continue;
        }
        removed.push(dir.clone());
        if !dry_run {
            let _ = std::fs::remove_dir_all(&dir);
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
