//! `ns cache export`: most recently used entries first, until the size cap.
//!
//! Exporting into a dir that already holds an earlier export is incremental:
//! files that are already there are kept, and everything the new selection
//! no longer references is pruned, so a cache dir restored and re-exported
//! on every CI run stays under the cap instead of growing without bound.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::params;

use super::{LAST_USED_FILE, hex_stem, temp_sibling, walk_files, write_atomic};
use crate::config::CowMode;
use crate::store::physical_usage::human;
use crate::store::{Entry, Store, cow_copy, usage_log};

#[derive(Debug, Default)]
struct ExportReport {
    entries: usize,
    entries_skipped: usize,
    objects_copied: usize,
    objects_kept: usize,
    /// Objects plus entry JSON, compared against the cap.
    bytes: u64,
    object_bytes: u64,
    bytes_copied: u64,
    pruned: usize,
}

/// What `run` selected and copied, for the caller's logs and the
/// machine-readable export summary.
#[derive(Debug, Default)]
pub(crate) struct ExportOutcome {
    /// Selected native bytes (entries, objects, memo, cached tests) — the
    /// amount the shared cap subtracts.
    pub selected_bytes: u64,
    /// Object and small-cache bytes actually written into the transfer dir
    /// this run (entry/manifest JSON is negligible and uncounted).
    pub bytes_new: u64,
    pub entries: usize,
    pub entries_skipped: usize,
}

/// Every readable entry with its last use (index, else creation time).
fn entries_by_recency(store: &Store) -> Result<Vec<(Entry, Vec<u8>, i64)>> {
    let mut conn = usage_log::open_index(&store.root)?;
    usage_log::merge(&store.root, &mut conn)?;
    let mut files = Vec::new();
    walk_files(&store.entries_dir(), &mut files);
    let mut out = Vec::new();
    for path in files {
        let Some(key) = hex_stem(&path) else { continue };
        let Some(entry) = store.read_entry(&key) else {
            continue;
        };
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let last_used: i64 = conn
            .query_row(
                "SELECT last_used FROM entries WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .unwrap_or(entry.created as i64);
        out.push((entry, bytes, last_used));
    }
    // Newest first; the key breaks ties so the selection is deterministic.
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.key.cmp(&b.0.key)));
    Ok(out)
}

/// Copy `src` to `dst` unless a file of the same size is already there.
fn place_object(src: &Path, dst: &Path, size: u64, mode: CowMode) -> Result<bool> {
    if std::fs::metadata(dst).is_ok_and(|m| m.len() == size) {
        return Ok(false);
    }
    let tmp = temp_sibling(dst)?;
    cow_copy::clone_or_copy(src, &tmp, mode)
        .with_context(|| format!("copy {} → {}", src.display(), tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("rename to {}", dst.display()))?;
    Ok(true)
}

/// Remove files under `dir` that are not in `keep`.
fn prune(dir: &Path, keep: &HashSet<PathBuf>) -> usize {
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    files
        .into_iter()
        .filter(|f| !keep.contains(f))
        .filter(|f| std::fs::remove_file(f).is_ok())
        .count()
}

/// Select memo and cached-test files newest-first within `cap`. Test file
/// names remain validated; writer temp files are ignored. Returns
/// `(selected bytes, bytes actually written, test results, pruned)`.
fn mirror_small_caches(store: &Store, out: &Store, cap: u64) -> Result<(u64, u64, usize, usize)> {
    let mut candidates: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut memo = Vec::new();
    walk_files(&store.memo_dir(), &mut memo);
    for src in memo {
        let Ok(rel) = src.strip_prefix(store.memo_dir()).map(Path::to_path_buf) else {
            continue;
        };
        if rel
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        candidates.push((src, out.memo_dir().join(rel)));
    }
    let mut tests = Vec::new();
    walk_files(&store.tests_dir(), &mut tests);
    for src in tests {
        let Some(key) = hex_stem(&src) else { continue };
        candidates.push((src, out.test_result_path(&key)));
    }
    candidates.sort_by(|(a, _), (b, _)| {
        let modified = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        };
        modified(b).cmp(&modified(a)).then_with(|| a.cmp(b))
    });
    let mut used = 0u64;
    let mut written = 0u64;
    let mut keep = HashSet::new();
    let mut test_results = 0;
    for (src, dst) in candidates {
        let size = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(u64::MAX);
        if cap == 0 || size > cap.saturating_sub(used) {
            continue;
        }
        let bytes = std::fs::read(&src).with_context(|| format!("read {}", src.display()))?;
        if std::fs::read(&dst).ok().as_deref() != Some(bytes.as_slice()) {
            write_atomic(&dst, &bytes)?;
            written += size;
        }
        used += size;
        if dst.starts_with(out.tests_dir()) {
            test_results += 1;
        }
        keep.insert(dst);
    }
    let pruned = prune(&out.memo_dir(), &keep) + prune(&out.tests_dir(), &keep);
    Ok((used, written, test_results, pruned))
}

pub fn run(root: &Path, dir: &Path, cap: u64, cow_mode: CowMode) -> Result<ExportOutcome> {
    let store = Store::open(root, cow_mode)?;
    // Exclusive: the usage log is folded into the index (for last_used) and
    // no wrapper can be half-way through writing an entry we would copy.
    let _lock = store.exclusive_lock().context("lock store")?;
    // Always create a recognizable transfer layout, including for Go/image-
    // only stores with no native entries.
    let out = Store::open(dir, cow_mode)?;
    // Export files must never share an inode with store objects.
    let copy_mode = match cow_mode {
        CowMode::Hardlink => CowMode::Auto,
        other => other,
    };

    let mut report = ExportReport::default();
    let mut objects: HashMap<String, u64> = HashMap::new();
    let mut manifests = HashSet::new();
    let mut selected: Vec<(Entry, Vec<u8>, i64)> = Vec::new();
    for (entry, bytes, last_used) in entries_by_recency(&store)? {
        let mut new: HashMap<&str, u64> = HashMap::new();
        for o in &entry.outputs {
            if !objects.contains_key(&o.object) {
                new.insert(&o.object, o.size);
            }
        }
        // Keep every native entry complete: its new objects, entry JSON,
        // manifest (once), and last-used row are selected atomically.
        let object_bytes: u64 = new.values().sum();
        let manifest_bytes = if manifests.contains(&entry.tier1) {
            0
        } else {
            std::fs::metadata(store.manifest_path(&entry.tier1))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        let last_used_bytes = format!("{}\t{last_used}\n", entry.key).len() as u64;
        let new_bytes = object_bytes + bytes.len() as u64 + manifest_bytes + last_used_bytes;
        let complete = entry.outputs.iter().all(|o| {
            std::fs::metadata(store.object_path(&o.object)).is_ok_and(|m| m.len() == o.size)
        });
        if !complete || report.bytes + new_bytes > cap {
            report.entries_skipped += 1;
            continue;
        }
        report.bytes += new_bytes;
        report.object_bytes += object_bytes;
        manifests.insert(entry.tier1.clone());
        objects.extend(new.into_iter().map(|(k, v)| (k.to_owned(), v)));
        selected.push((entry, bytes, last_used));
    }

    // Objects before entries, as in the store: a reader of a half-written
    // export never sees an entry whose objects are missing.
    let mut keep_objects = HashSet::new();
    for (hash, size) in &objects {
        let dst = out.object_path(hash);
        if place_object(&store.object_path(hash), &dst, *size, copy_mode)? {
            report.objects_copied += 1;
            report.bytes_copied += size;
        } else {
            report.objects_kept += 1;
        }
        keep_objects.insert(dst);
    }
    let mut keep_entries = HashSet::new();
    let mut keep_manifests = HashSet::new();
    let mut last_used_tsv = String::new();
    for (entry, bytes, last_used) in &selected {
        let dst = out.entry_path(&entry.key);
        if std::fs::read(&dst).ok().as_deref() != Some(bytes.as_slice()) {
            write_atomic(&dst, bytes)?;
        }
        keep_entries.insert(dst);
        let manifest = store.manifest_path(&entry.tier1);
        if let Ok(text) = std::fs::read(&manifest) {
            let dst = out.manifest_path(&entry.tier1);
            if std::fs::read(&dst).ok() != Some(text.clone()) {
                write_atomic(&dst, &text)?;
            }
            keep_manifests.insert(dst);
        }
        last_used_tsv.push_str(&format!("{}\t{last_used}\n", entry.key));
    }
    report.entries = selected.len();
    write_atomic(&dir.join(LAST_USED_FILE), last_used_tsv.as_bytes())?;

    report.pruned = prune(&out.entries_dir(), &keep_entries)
        + prune(&out.manifests_dir(), &keep_manifests)
        + prune(&out.objects_dir(), &keep_objects);
    let (small_bytes, small_written, test_results, pruned_small) =
        mirror_small_caches(&store, &out, cap.saturating_sub(report.bytes))?;
    report.bytes += small_bytes;
    report.pruned += pruned_small;

    crate::ui_info!(format!("exported {} entries", report.entries);
        objects = objects.len(), size = human(report.object_bytes));
    crate::ui_info!(format!("copied {} objects", report.objects_copied);
        size = human(report.bytes_copied), already_there = report.objects_kept,
        pruned = report.pruned);
    if report.entries_skipped > 0 {
        crate::ui_info!(format!("skipped {} entries over the cap or incomplete", report.entries_skipped);
            cap = human(cap));
    }
    if test_results > 0 {
        crate::ui_info!(format!("exported {test_results} cached test results"));
    }
    Ok(ExportOutcome {
        selected_bytes: report.bytes,
        bytes_new: report.bytes_copied + small_written,
        entries: report.entries,
        entries_skipped: report.entries_skipped,
    })
}
