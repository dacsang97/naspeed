//! Remove stale `-<hash>` unit variants from a profile dir without causing
//! rebuilds of anything still in use.
//!
//! File age is useless here: a no-op build touches nothing, so a crate used
//! every day can look months old. Instead we use cargo's fingerprint graph:
//! every `.fingerprint/<pkg>-<hash>/` holds the unit's fingerprint value
//! (`<kind>` file, 16 hex digits) and its dependencies' fingerprint values
//! (`<kind>.json` → `deps[i][3]`). The newest unit of each (package, kind)
//! is a root; everything reachable from a root is live; the rest goes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

#[derive(Debug, Default, Clone)]
pub struct SweepStats {
    pub units_total: usize,
    pub units_removed: usize,
    pub files_removed: usize,
    pub bytes_freed: u64,
    pub incremental_sessions_removed: usize,
}

impl SweepStats {
    pub fn add(&mut self, o: &SweepStats) {
        self.units_total += o.units_total;
        self.units_removed += o.units_removed;
        self.files_removed += o.files_removed;
        self.bytes_freed += o.bytes_freed;
        self.incremental_sessions_removed += o.incremental_sessions_removed;
    }
}

#[derive(Debug)]
struct Unit {
    pkg: String,
    hash: String,
    /// (kind, own fingerprint value)
    kinds: Vec<(String, u64)>,
    deps: Vec<u64>,
    mtime: SystemTime,
}

fn split_unit_name(name: &str) -> Option<(&str, &str)> {
    let (pkg, hash) = name.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some((pkg, hash))
}

fn read_unit(dir: &Path) -> Option<Unit> {
    let name = dir.file_name()?.to_str()?;
    let (pkg, hash) = split_unit_name(name)?;
    let mut kinds = Vec::new();
    let mut deps = Vec::new();
    let mut mtime = SystemTime::UNIX_EPOCH;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if file.ends_with(".json") {
            if let Ok(json) = std::fs::read(&path)
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&json)
                && let Some(list) = v.get("deps").and_then(|d| d.as_array())
            {
                deps.extend(
                    list.iter()
                        .filter_map(|d| d.get(3).and_then(|x| x.as_u64())),
                );
            }
            continue;
        }
        if file == "invoked.timestamp" || file.starts_with("dep-") || file.starts_with("output-") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(value) = u64::from_str_radix(text.trim(), 16)
        {
            kinds.push((file, value));
            if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                mtime = mtime.max(m);
            }
        }
    }
    Some(Unit {
        pkg: pkg.to_owned(),
        hash: hash.to_owned(),
        kinds,
        deps,
        mtime,
    })
}

fn size_of(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path)
            .map(|rd| rd.flatten().map(|e| size_of(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn remove(path: &Path, dry_run: bool, stats: &mut SweepStats) {
    let bytes = size_of(path);
    stats.bytes_freed += bytes;
    stats.files_removed += 1;
    if dry_run {
        return;
    }
    let _ = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
}

fn artifact_hash(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    split_unit_name(stem).map(|(_, h)| h)
}

/// Caller holds cargo's locks for `profile`.
pub fn sweep_profile(profile: &Path, dry_run: bool) -> Result<SweepStats> {
    let mut stats = SweepStats::default();
    let fp_dir = profile.join(".fingerprint");
    let units: Vec<Unit> = match std::fs::read_dir(&fp_dir) {
        Ok(rd) => rd.flatten().filter_map(|e| read_unit(&e.path())).collect(),
        Err(_) => return Ok(stats),
    };
    stats.units_total = units.len();

    let mut newest: HashMap<(String, String), usize> = HashMap::new();
    let mut by_value: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, unit) in units.iter().enumerate() {
        for (kind, value) in &unit.kinds {
            by_value.entry(*value).or_default().push(i);
            let slot = newest.entry((unit.pkg.clone(), kind.clone())).or_insert(i);
            if units[*slot].mtime < unit.mtime {
                *slot = i;
            }
        }
    }
    let mut live: HashSet<usize> = HashSet::new();
    let mut queue: VecDeque<usize> = newest.values().copied().collect();
    while let Some(i) = queue.pop_front() {
        if !live.insert(i) {
            continue;
        }
        for dep in &units[i].deps {
            for &j in by_value.get(dep).into_iter().flatten() {
                if !live.contains(&j) {
                    queue.push_back(j);
                }
            }
        }
    }

    let dead: Vec<&Unit> = units
        .iter()
        .enumerate()
        .filter(|(i, u)| !live.contains(i) && !u.kinds.is_empty())
        .map(|(_, u)| u)
        .collect();
    let dead_hashes: HashSet<&str> = dead.iter().map(|u| u.hash.as_str()).collect();
    stats.units_removed = dead.len();
    for unit in &dead {
        let name = format!("{}-{}", unit.pkg, unit.hash);
        remove(&fp_dir.join(&name), dry_run, &mut stats);
        let build = profile.join("build").join(&name);
        if build.exists() {
            remove(&build, dry_run, &mut stats);
        }
    }
    for sub in ["deps", "examples"] {
        let Ok(rd) = std::fs::read_dir(profile.join(sub)) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if artifact_hash(&name).is_some_and(|h| dead_hashes.contains(h)) {
                remove(&entry.path(), dry_run, &mut stats);
            }
        }
    }
    sweep_incremental(&profile.join("incremental"), dry_run, &mut stats);
    Ok(stats)
}

/// Keep only the newest session (`s-…` dir and its `.lock`) per crate.
fn sweep_incremental(dir: &Path, dry_run: bool, stats: &mut SweepStats) {
    let Ok(crates) = std::fs::read_dir(dir) else {
        return;
    };
    for krate in crates.flatten() {
        let Ok(rd) = std::fs::read_dir(krate.path()) else {
            continue;
        };
        let mut sessions: Vec<(SystemTime, PathBuf)> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| e.file_name().to_string_lossy().starts_with("s-"))
            .map(|e| {
                (
                    e.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                    e.path(),
                )
            })
            .collect();
        sessions.sort();
        sessions.pop();
        for (_, session) in sessions {
            remove(&session, dry_run, stats);
            stats.incremental_sessions_removed += 1;
            let lock = session.with_extension("lock");
            if lock.exists() {
                remove(&lock, dry_run, stats);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(fp: &Path, name: &str, kind: &str, value: u64, deps: &[u64]) {
        let dir = fp.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(kind), format!("{value:016x}")).unwrap();
        let deps: Vec<_> = deps
            .iter()
            .map(|d| serde_json::json!([1, "x", false, d]))
            .collect();
        std::fs::write(
            dir.join(format!("{kind}.json")),
            serde_json::json!({ "deps": deps }).to_string(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    #[test]
    fn keeps_reachable_variants_and_removes_stale_ones() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        // Two live `syn` variants (host for the proc-macro, target for app),
        // and one stale `app` + `serde` from an old feature set.
        unit(&fp, "serde-aaaaaaaaaaaaaaaa", "lib-serde", 1, &[]);
        unit(&fp, "app-bbbbbbbbbbbbbbbb", "lib-app", 2, &[1]);
        unit(&fp, "syn-cccccccccccccccc", "lib-syn", 3, &[]);
        unit(&fp, "syn-dddddddddddddddd", "lib-syn", 4, &[]);
        unit(&fp, "derive-eeeeeeeeeeeeeeee", "lib-derive", 5, &[3]);
        unit(&fp, "serde-ffffffffffffffff", "lib-serde", 6, &[5]);
        unit(&fp, "app-1111111111111111", "lib-app", 7, &[6, 4]);
        for f in [
            "libserde-aaaaaaaaaaaaaaaa.rlib",
            "app-bbbbbbbbbbbbbbbb.d",
            "libsyn-cccccccccccccccc.rlib",
            "libapp-1111111111111111.rlib",
        ] {
            std::fs::write(deps.join(f), b"x").unwrap();
        }
        let stats = sweep_profile(&profile, false).unwrap();
        assert_eq!(stats.units_total, 7);
        assert_eq!(stats.units_removed, 2);
        assert!(!fp.join("serde-aaaaaaaaaaaaaaaa").exists());
        assert!(!fp.join("app-bbbbbbbbbbbbbbbb").exists());
        assert!(
            fp.join("syn-cccccccccccccccc").exists(),
            "host syn reachable via derive"
        );
        assert!(fp.join("syn-dddddddddddddddd").exists());
        assert!(!deps.join("libserde-aaaaaaaaaaaaaaaa.rlib").exists());
        assert!(deps.join("libsyn-cccccccccccccccc.rlib").exists());
        assert!(deps.join("libapp-1111111111111111.rlib").exists());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        unit(&fp, "a-aaaaaaaaaaaaaaaa", "lib-a", 1, &[]);
        unit(&fp, "a-bbbbbbbbbbbbbbbb", "lib-a", 2, &[]);
        let stats = sweep_profile(&profile, true).unwrap();
        assert_eq!(stats.units_removed, 1);
        assert!(fp.join("a-aaaaaaaaaaaaaaaa").exists());
    }
}
