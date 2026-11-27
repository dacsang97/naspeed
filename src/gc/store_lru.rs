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
    /// ActionResult records evicted (age only).
    pub actions_removed: usize,
    /// Output-tree records swept once nothing references them.
    pub trees_removed: usize,
    /// Dead lease files reaped.
    pub leases_removed: usize,
    /// Abandoned staging dirs (owning lease gone) removed.
    pub staging_removed: usize,
    /// Orphan view records (worktree gone) removed.
    pub views_removed: usize,
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
                let _ = crate::platform::remove_file(&path);
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

    // ---- Mark phase: action records, their trees, and active leases ----
    // A lease marks its key's record and everything it references; a
    // record merely on disk keeps its own tree and objects live. Records
    // age out by `max_age` only: cap pressure applies to entries, since
    // freeing record objects needs the grace window anyway.
    let live_keys: HashSet<String> = crate::store::lease::active_keys(store, grace);
    let mut live_objects: HashSet<String> =
        crate::store::lease::active_guards(store, grace)
            .into_iter()
            .collect();
    let mut live_trees: HashSet<String> = HashSet::new();
    let mut dead_action_files = Vec::new();
    let mut action_files = Vec::new();
    walk_files(&store.actions_dir(), &mut action_files);
    for path in action_files {
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let key = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let leased = live_keys.contains(&key);
        let parsed = std::fs::read(&path).ok().and_then(|b| {
            serde_json::from_slice::<crate::engine::action_result::ActionResult>(&b).ok()
        });
        let mut last_used = mtime as i64;
        let (objects, tree_ok, tree_digest) = match parsed
            .as_ref()
            .filter(|r| r.action_key == key)
        {
            Some(r) => {
                last_used = r.created.max(mtime) as i64;
                let mut objects = Vec::new();
                let mut tree_ok = r.output_tree.is_empty();
                if !r.output_tree.is_empty()
                    && let Ok(tb) = std::fs::read(store.tree_path(&r.output_tree))
                    && let Ok(t) =
                        serde_json::from_slice::<crate::store::tree::Tree>(&tb)
                {
                    tree_ok = true;
                    for e in &t.entries {
                        if !e.digest.is_empty() {
                            objects.push(e.digest.clone());
                        }
                    }
                }
                for s in [&r.stdout, &r.stderr].into_iter().flatten() {
                    objects.push(s.digest.clone());
                }
                (objects, tree_ok, r.output_tree.clone())
            }
            None => (Vec::new(), false, String::new()),
        };
        // A record keeps its references live while it survives; a leased
        // key survives regardless of `max_age`.
        let too_old = stale_before.is_some_and(|t| last_used < t);
        let broken = !tree_ok;
        if leased || (!too_old && !(broken && older_than(&path, grace))) {
            if !tree_digest.is_empty() {
                live_trees.insert(tree_digest);
            }
            live_objects.extend(objects);
        } else {
            dead_action_files.push(path);
        }
    }
    report.actions_removed = dead_action_files.len();

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
                && !live_objects.contains(&o.object)
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
        if live_objects.contains(name.as_str()) {
            continue;
        }
        if refs.get(name.as_str()).copied().unwrap_or(0) == 0
            && !removed_objects.contains(name)
            && older_than(path, grace)
        {
            removed_objects.insert(name.clone());
            report.bytes_freed += size;
        }
    }

    // ---- Sweeps for the new store areas, all after the mark phase ----
    // Output-tree records with no live record referencing them.
    let mut tree_files = Vec::new();
    walk_files(&store.trees_dir(), &mut tree_files);
    let stale_trees: Vec<_> = tree_files
        .into_iter()
        .filter(|p| {
            let digest = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            !live_trees.contains(&digest) && older_than(p, grace)
        })
        .collect();
    report.trees_removed = stale_trees.len();

    // Abandoned staging dirs (owning lease dead) and dead lease files.
    let abandoned = crate::engine::staging::recover_abandoned(store, grace, dry_run);
    report.staging_removed = abandoned.len();
    report.leases_removed = crate::store::lease::sweep_dead(store, grace, dry_run);

    // Orphan view bookkeeping (worktree gone). Materialization bytes live
    // inside the worktree and go with it.
    report.views_removed = crate::engine::output_view::sweep_orphans(store, grace, dry_run).len();
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
    for path in dead_action_files.iter().chain(stale_trees.iter()) {
        let _ = crate::platform::remove_file(path);
    }
    for (key, path) in &evicted {
        let _ = crate::platform::remove_file(path);
        conn.execute("DELETE FROM entries WHERE key = ?1", params![key])?;
    }
    for name in &removed_objects {
        if let Some((path, _)) = sizes.get(name) {
            let _ = crate::platform::remove_file(path);
        }
    }
    for path in stale_tmp.into_iter().chain(test_results) {
        let _ = crate::platform::remove_file(path);
    }
    // No wrapper holds a key lock while we hold the exclusive store lock,
    // so lock files can go without breaking mutual exclusion.
    let mut locks = Vec::new();
    walk_files(&store.root.join("locks"), &mut locks);
    for path in locks {
        let _ = crate::platform::remove_file(path);
    }
    // Manifests whose tier-1 key has no entry left.
    let live_tier1: HashSet<&str> = entries
        .iter()
        .filter(|(e, _, _)| !evicted.iter().any(|(k, _)| k == &e.key))
        .map(|(e, _, _)| e.tier1.as_str())
        .collect();
    let mut manifests = Vec::new();
    walk_files(&store.manifests_dir(), &mut manifests);
    let rust_manifests = crate::engine::rust::manifest_dir(store);
    for path in manifests {
        if path.starts_with(&rust_manifests) {
            if max_age.is_some_and(|age| older_than(&path, age.max(grace))) {
                let _ = crate::platform::remove_file(&path);
            }
            continue;
        }
        let tier1 = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !live_tier1.contains(tier1.as_str()) && older_than(&path, grace) {
            let _ = crate::platform::remove_file(path);
        }
    }
    Ok(report)
}
