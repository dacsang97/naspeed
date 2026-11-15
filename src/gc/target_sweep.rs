//! Remove stale `-<hash>` unit variants from a profile dir without causing
//! rebuilds of anything still in use.
//!
//! Units are read in either of cargo's layouts (see `target_layout`): the
//! flat one spreads a unit over `.fingerprint/`, `deps/` and `build/`, the
//! per-unit one (cargo 1.100 on) keeps it all in `build/<pkg>/<hash>/`.
//!
//! File age alone is useless here: a no-op build touches nothing, so a crate
//! used every day can look months old. Instead we use cargo's fingerprint
//! graph: every `.fingerprint/<pkg>-<hash>/` holds the unit's fingerprint
//! value (`<kind>` file, 16 hex digits) and its dependencies' fingerprint
//! values (`<kind>.json` → `deps[i][3]`).
//!
//! Roots:
//! - the newest unit of each (package, kind), always;
//! - the newest unit of each (package, kind, flavor) used within `max_age`.
//!   The flavor is the fingerprint's features, profile, target, rustflags,
//!   config and compile kind: `cargo check`, `cargo build` and `cargo test`,
//!   or two `-p` selections, keep separate variants of one crate that are all
//!   in use.
//!
//! "Used" is the newest of a rebuild and a mark from `target_usage` (`ns dev`
//! marks every unit its cargo runs report, fresh or rebuilt). Everything
//! reachable from a root is live; the rest goes. The first rule alone is the
//! original sweep, so the second one only ever keeps more.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::target_layout::{self, UnitDir, UnitFiles};

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
    fingerprint: PathBuf,
    files: UnitFiles,
    /// (kind, own fingerprint value, flavor)
    kinds: Vec<(String, u64, String)>,
    deps: Vec<u64>,
    /// Last rebuild or `target_usage` mark, whichever is newer.
    last_used: SystemTime,
}

/// Fingerprint fields that tell `check`/`build`/`test` (and feature or
/// target) variants of one crate apart.
const FLAVOR_FIELDS: &[&str] = &[
    "features",
    "profile",
    "target",
    "rustflags",
    "config",
    "compile_kind",
];

fn flavor_of(json: &serde_json::Value) -> String {
    FLAVOR_FIELDS
        .iter()
        .map(|f| json.get(*f).map(|v| v.to_string()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("|")
}

/// Cargo writes a fingerprint as `to_hex(u64)`: the value's little-endian
/// bytes in hex. `deps[i][3]` in the unit JSON holds the same `u64`.
fn parse_fingerprint(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.len() != 16 {
        return None;
    }
    let mut bytes = [0u8; 8];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(u64::from_le_bytes(bytes))
}

fn read_unit(unit: UnitDir) -> Option<Unit> {
    let dir = unit.fingerprint.as_path();
    let mut values = Vec::new();
    let mut flavors: HashMap<String, String> = HashMap::new();
    let mut deps = Vec::new();
    // The dir's own mtime carries `target_usage` marks.
    let mut last_used = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if let Some(kind) = file.strip_suffix(".json") {
            if let Ok(json) = std::fs::read(&path)
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&json)
            {
                flavors.insert(kind.to_owned(), flavor_of(&v));
                if let Some(list) = v.get("deps").and_then(|d| d.as_array()) {
                    deps.extend(
                        list.iter()
                            .filter_map(|d| d.get(3).and_then(|x| x.as_u64())),
                    );
                }
            }
            continue;
        }
        if file == "invoked.timestamp" || file.starts_with("dep-") || file.starts_with("output-") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(value) = parse_fingerprint(&text)
        {
            values.push((file, value));
            if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                last_used = last_used.max(m);
            }
        }
    }
    let kinds = values
        .into_iter()
        .map(|(kind, value)| {
            let flavor = flavors.get(&kind).cloned().unwrap_or_default();
            (kind, value, flavor)
        })
        .collect();
    Some(Unit {
        pkg: unit.pkg,
        hash: unit.hash,
        fingerprint: unit.fingerprint,
        files: unit.files,
        kinds,
        deps,
        last_used,
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
    target_layout::split_unit_name(stem).map(|(_, h)| h)
}

/// Caller holds cargo's locks for `profile`. Units unused for longer than
/// `max_age` are dropped unless a live unit still depends on them.
pub fn sweep_profile(
    profile: &Path,
    dry_run: bool,
    max_age: Option<Duration>,
) -> Result<SweepStats> {
    let mut stats = SweepStats::default();
    let units: Vec<Unit> = target_layout::units(profile)
        .into_iter()
        .filter_map(read_unit)
        .collect();
    stats.units_total = units.len();

    let cutoff = max_age.and_then(|age| SystemTime::now().checked_sub(age));
    let mut newest_kind: HashMap<(&str, &str), usize> = HashMap::new();
    let mut newest_flavor: HashMap<(&str, &str, &str), usize> = HashMap::new();
    let mut by_value: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, unit) in units.iter().enumerate() {
        for (kind, value, flavor) in &unit.kinds {
            by_value.entry(*value).or_default().push(i);
            let newer = |slot: &usize| units[*slot].last_used < unit.last_used;
            let slot = newest_kind
                .entry((unit.pkg.as_str(), kind.as_str()))
                .or_insert(i);
            if newer(slot) {
                *slot = i;
            }
            let slot = newest_flavor
                .entry((unit.pkg.as_str(), kind.as_str(), flavor.as_str()))
                .or_insert(i);
            if newer(slot) {
                *slot = i;
            }
        }
    }
    let recent = |i: &usize| cutoff.is_none_or(|c| units[*i].last_used >= c);
    let mut live: HashSet<usize> = HashSet::new();
    let mut queue: VecDeque<usize> = newest_kind
        .values()
        .copied()
        .chain(newest_flavor.values().copied().filter(recent))
        .collect();
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
    let mut dead_hashes: HashSet<&str> = HashSet::new();
    stats.units_removed = dead.len();
    for unit in &dead {
        match &unit.files {
            UnitFiles::Flat => {
                dead_hashes.insert(unit.hash.as_str());
                remove(&unit.fingerprint, dry_run, &mut stats);
                let build = profile
                    .join("build")
                    .join(format!("{}-{}", unit.pkg, unit.hash));
                if build.exists() {
                    remove(&build, dry_run, &mut stats);
                }
            }
            UnitFiles::Dir(dir) => {
                remove(dir, dry_run, &mut stats);
                if !dry_run && let Some(pkg) = dir.parent() {
                    // Only succeeds once the package has no unit left.
                    let _ = std::fs::remove_dir(pkg);
                }
            }
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
        unit_with_profile(fp, name, kind, value, deps, 0);
    }

    fn unit_with_profile(
        fp: &Path,
        name: &str,
        kind: &str,
        value: u64,
        deps: &[u64],
        profile: u64,
    ) {
        let dir = fp.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let hex: String = value
            .to_le_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(dir.join(kind), hex).unwrap();
        let deps: Vec<_> = deps
            .iter()
            .map(|d| serde_json::json!([1, "x", false, d]))
            .collect();
        std::fs::write(
            dir.join(format!("{kind}.json")),
            serde_json::json!({ "deps": deps, "profile": profile }).to_string(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    /// Backdates a unit's files and dir, as if last built and used `days` ago.
    fn age(fp: &Path, name: &str, days: u64) {
        let when = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(days * 86_400),
        );
        let dir = fp.join(name);
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            filetime::set_file_mtime(entry.path(), when).unwrap();
        }
        filetime::set_file_mtime(&dir, when).unwrap();
    }

    #[test]
    fn keeps_check_build_and_test_variants_of_one_crate() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        // Same crate and kind, three profiles: metadata-only check, build, test.
        unit_with_profile(&fp, "core-aaaaaaaaaaaaaaaa", "lib-core", 1, &[], 11);
        unit_with_profile(&fp, "core-bbbbbbbbbbbbbbbb", "lib-core", 2, &[], 22);
        unit_with_profile(&fp, "core-cccccccccccccccc", "lib-core", 3, &[], 33);
        let stats = sweep_profile(&profile, false, None).unwrap();
        assert_eq!(stats.units_removed, 0);
        assert!(fp.join("core-aaaaaaaaaaaaaaaa").exists());
    }

    #[test]
    fn drops_units_unused_for_max_age_unless_a_live_unit_needs_them() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        let max_age = Some(Duration::from_secs(14 * 86_400));
        // `app` in another flavor has not been used for a month; `dep` is as
        // old but `app` in the current flavor, used today, depends on it.
        unit_with_profile(&fp, "app-aaaaaaaaaaaaaaaa", "lib-app", 1, &[], 7);
        unit(&fp, "dep-bbbbbbbbbbbbbbbb", "lib-dep", 2, &[]);
        unit(&fp, "app-cccccccccccccccc", "lib-app", 3, &[2]);
        age(&fp, "app-aaaaaaaaaaaaaaaa", 30);
        age(&fp, "dep-bbbbbbbbbbbbbbbb", 30);

        let without_age = sweep_profile(&profile, true, None).unwrap();
        assert_eq!(without_age.units_removed, 0);

        let stats = sweep_profile(&profile, false, max_age).unwrap();
        assert_eq!(stats.units_removed, 1);
        assert!(!fp.join("app-aaaaaaaaaaaaaaaa").exists());
        assert!(fp.join("dep-bbbbbbbbbbbbbbbb").exists());
        assert!(fp.join("app-cccccccccccccccc").exists());
    }

    #[test]
    fn reads_fingerprints_as_cargo_writes_them() {
        // `to_hex(0x0102_0304_0506_0708)` in cargo: little-endian bytes.
        assert_eq!(
            parse_fingerprint("0807060504030201\n"),
            Some(0x0102_0304_0506_0708)
        );
        assert_eq!(parse_fingerprint("0807"), None);
        assert_eq!(parse_fingerprint("zz07060504030201"), None);
    }

    #[test]
    fn a_usage_mark_keeps_an_old_unit_alive() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        unit(&fp, "tool-aaaaaaaaaaaaaaaa", "lib-tool", 1, &[]);
        age(&fp, "tool-aaaaaaaaaaaaaaaa", 30);
        // A fresh (no-op) build marks the dir only.
        filetime::set_file_mtime(fp.join("tool-aaaaaaaaaaaaaaaa"), filetime::FileTime::now())
            .unwrap();
        let stats = sweep_profile(&profile, false, Some(Duration::from_secs(14 * 86_400))).unwrap();
        assert_eq!(stats.units_removed, 0);
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
        let stats = sweep_profile(&profile, false, None).unwrap();
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

    /// A unit in cargo 1.100's layout: `build/<pkg>/<hash>/{fingerprint,out}`.
    fn per_unit(profile: &Path, pkg: &str, hash: &str, kind: &str, value: u64, deps: &[u64]) {
        let dir = profile.join("build").join(pkg).join(hash);
        let fp = dir.join("fingerprint");
        std::fs::create_dir_all(&fp).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out").join(format!("lib{pkg}-{hash}.rlib")), b"x").unwrap();
        let hex: String = value
            .to_le_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(fp.join(kind), hex).unwrap();
        let deps: Vec<_> = deps
            .iter()
            .map(|d| serde_json::json!([1, "x", false, d]))
            .collect();
        std::fs::write(
            fp.join(format!("{kind}.json")),
            serde_json::json!({ "deps": deps, "profile": 0 }).to_string(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    #[test]
    fn sweeps_units_in_the_per_unit_layout() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        // `serde` old variant, unreachable; `serde` new one used by `app`.
        per_unit(&profile, "serde", "aaaaaaaaaaaaaaaa", "lib-serde", 1, &[]);
        per_unit(&profile, "serde", "bbbbbbbbbbbbbbbb", "lib-serde", 2, &[]);
        per_unit(&profile, "app", "cccccccccccccccc", "lib-app", 3, &[2]);
        // `gone` has a single, superseded-by-nothing unit; `old` two.
        per_unit(&profile, "old", "dddddddddddddddd", "lib-old", 4, &[]);
        per_unit(&profile, "old", "eeeeeeeeeeeeeeee", "lib-old", 5, &[]);
        let stats = sweep_profile(&profile, false, None).unwrap();
        assert_eq!(stats.units_total, 5);
        assert_eq!(stats.units_removed, 2);
        let unit = |pkg: &str, hash: &str| profile.join("build").join(pkg).join(hash);
        assert!(!unit("serde", "aaaaaaaaaaaaaaaa").exists());
        assert!(unit("serde", "bbbbbbbbbbbbbbbb").exists());
        assert!(unit("app", "cccccccccccccccc").exists());
        assert!(!unit("old", "dddddddddddddddd").exists());
        assert!(unit("old", "eeeeeeeeeeeeeeee").exists());
    }

    #[test]
    fn drops_an_emptied_package_dir_in_the_per_unit_layout() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        per_unit(&profile, "a", "aaaaaaaaaaaaaaaa", "lib-a", 1, &[]);
        per_unit(&profile, "a", "bbbbbbbbbbbbbbbb", "lib-a", 2, &[]);
        let fp = profile.join("build/a/aaaaaaaaaaaaaaaa/fingerprint");
        let old = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(30 * 86_400),
        );
        filetime::set_file_mtime(fp.join("lib-a"), old).unwrap();
        filetime::set_file_mtime(&fp, old).unwrap();
        sweep_profile(&profile, false, None).unwrap();
        assert!(!profile.join("build/a/aaaaaaaaaaaaaaaa").exists());
        assert!(
            profile.join("build/a").exists(),
            "the package still has a unit"
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        unit(&fp, "a-aaaaaaaaaaaaaaaa", "lib-a", 1, &[]);
        unit(&fp, "a-bbbbbbbbbbbbbbbb", "lib-a", 2, &[]);
        let stats = sweep_profile(&profile, true, None).unwrap();
        assert_eq!(stats.units_removed, 1);
        assert!(fp.join("a-aaaaaaaaaaaaaaaa").exists());
    }
}
