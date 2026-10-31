//! Shrink the store below its cap by evicting least-recently-used entries,
//! then drop unreferenced objects older than the grace period.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use rusqlite::params;

use crate::store::{Entry, Store, usage_log};

/// Objects without an entry are kept this long: a wrapper writes objects
/// first and its entry a moment later.
pub const GRACE: Duration = Duration::from_secs(3600);

#[derive(Debug, Default)]
pub struct LruReport {
    pub entries_before: usize,
    pub entries_removed: usize,
    pub objects_removed: usize,
    pub bytes_before: u64,
    pub bytes_freed: u64,
    pub tmp_removed: usize,
    pub test_results_removed: usize,
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_files(&e.path(), out),
            Ok(t) if t.is_file() => out.push(e.path()),
            _ => {}
        }
    }
}

fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|d| d >= age)
}

/// Caller must hold the exclusive store lock.
/// Caller must hold the exclusive store lock. Entries are evicted oldest
/// first while the store is above `cap_bytes`, and regardless of size when
/// unused for longer than `max_age`.
pub fn run(
    store: &Store,
    cap_bytes: u64,
    max_age: Option<Duration>,
    dry_run: bool,
    grace: Duration,
) -> Result<LruReport> {
    let stale_before =
        max_age.map(|age| crate::store::now_secs().saturating_sub(age.as_secs()) as i64);
    let mut report = LruReport::default();
    let mut conn = usage_log::open_index(&store.root)?;
    usage_log::merge(&store.root, &mut conn)?;

    let mut entry_files = Vec::new();
    walk_files(&store.entries_dir(), &mut entry_files);
    let mut entries: Vec<(Entry, PathBuf, i64)> = Vec::new();
    for path in entry_files {
        let Some(entry) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
        else {
            // Unreadable entry: it can never hit, remove it.
            if !dry_run {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        };
        let last_used: i64 = conn
            .query_row(
                "SELECT last_used FROM entries WHERE key = ?1",
                params![entry.key],
                |r| r.get(0),
            )
            // Not indexed yet (fetched, pushed or imported): the entry
            // file's mtime is when it arrived here, `created` is when some
            // machine first built it.
            .unwrap_or_else(|_| {
                let arrived = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                entry.created.max(arrived) as i64
            });
        conn.execute(
            "INSERT OR IGNORE INTO entries (key, crate_name, size, created, last_used) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry.key, entry.crate_name, entry.size() as i64, entry.created as i64, last_used],
        )?;
        entries.push((entry, path, last_used));
    }
    report.entries_before = entries.len();

    // Object sizes and reference counts.
    let mut object_files = Vec::new();
    walk_files(&store.objects_dir(), &mut object_files);
    let mut sizes: HashMap<String, (PathBuf, u64)> = HashMap::new();
    for path in object_files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        sizes.insert(name, (path, size));
    }
    let mut refs: HashMap<&str, usize> = HashMap::new();
    for (entry, _, _) in &entries {
        for o in &entry.outputs {
            *refs.entry(o.object.as_str()).or_default() += 1;
        }
    }
    let mut total: u64 = sizes.values().map(|(_, s)| s).sum();
    report.bytes_before = total;

    // Evict LRU entries until referenced + pending objects fit the cap.
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| entries[i].2);
    let mut removed_objects: HashSet<String> = HashSet::new();
    let mut evicted = Vec::new();
    for i in order {
        let (entry, path, last_used) = &entries[i];
        let too_old = stale_before.is_some_and(|t| *last_used < t);
        if total <= cap_bytes && !too_old {
            // Sorted by last use: nothing after this is older either.
            break;
        }
        for o in &entry.outputs {
            let count = refs.entry(o.object.as_str()).or_default();
            *count = count.saturating_sub(1);
            if *count == 0
                && removed_objects.insert(o.object.clone())
                && let Some((_, size)) = sizes.get(&o.object)
            {
                total = total.saturating_sub(*size);
                report.bytes_freed += size;
            }
        }
        evicted.push((entry.key.clone(), path.clone()));
    }
    report.entries_removed = evicted.len();

    // Unreferenced objects past the grace period (e.g. left by a crash).
    for (name, (path, size)) in &sizes {
        if refs.get(name.as_str()).copied().unwrap_or(0) == 0
            && !removed_objects.contains(name)
            && older_than(path, grace)
        {
            removed_objects.insert(name.clone());
            report.bytes_freed += size;
        }
    }
    report.objects_removed = removed_objects.len();

    let mut tmp_files = Vec::new();
    walk_files(&store.tmp_dir(), &mut tmp_files);
    let stale_tmp: Vec<_> = tmp_files
        .into_iter()
        .filter(|p| older_than(p, grace))
        .collect();
    report.tmp_removed = stale_tmp.len();

    // Cached test results: tiny, so only age matters (a replay bumps mtime).
    let mut test_results = Vec::new();
    if let Some(age) = max_age {
        walk_files(&store.tests_dir(), &mut test_results);
        test_results.retain(|p| older_than(p, age));
    }
    report.test_results_removed = test_results.len();

    if dry_run {
        return Ok(report);
    }
    for (key, path) in &evicted {
        let _ = std::fs::remove_file(path);
        conn.execute("DELETE FROM entries WHERE key = ?1", params![key])?;
    }
    for name in &removed_objects {
        if let Some((path, _)) = sizes.get(name) {
            let _ = std::fs::remove_file(path);
        }
    }
    for path in stale_tmp.into_iter().chain(test_results) {
        let _ = std::fs::remove_file(path);
    }
    // No wrapper holds a key lock while we hold the exclusive store lock,
    // so lock files can go without breaking mutual exclusion.
    let mut locks = Vec::new();
    walk_files(&store.root.join("locks"), &mut locks);
    for path in locks {
        let _ = std::fs::remove_file(path);
    }
    // Manifests whose tier-1 key has no entry left.
    let live_tier1: HashSet<&str> = entries
        .iter()
        .filter(|(e, _, _)| !evicted.iter().any(|(k, _)| k == &e.key))
        .map(|(e, _, _)| e.tier1.as_str())
        .collect();
    let mut manifests = Vec::new();
    walk_files(&store.manifests_dir(), &mut manifests);
    for path in manifests {
        let tier1 = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !live_tier1.contains(tier1.as_str()) && older_than(&path, grace) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(report)
}
