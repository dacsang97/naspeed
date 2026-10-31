//! `ns cache import`: merge an export dir into the store.
//!
//! Same write order as the wrapper: objects (content-verified, temp file +
//! rename via `Store::ingest_object`) before the entry that references them,
//! so a concurrent reader never sees an entry with missing objects. Entries,
//! objects, manifests and memo files the store already has are left alone.
//! Imported entries are registered in `index.sqlite` with their exported
//! `last_used`, so `gc`'s LRU and the next export keep the original order.

use std::collections::HashMap;
use std::path::{Component, Path};

use anyhow::{Context, Result};
use rusqlite::params;

use super::{LAST_USED_FILE, hex_stem, is_hex_name, walk_files, write_atomic};
use crate::config::CowMode;
use crate::store::physical_usage::human;
use crate::store::{ENTRY_VERSION, Entry, Manifest, Store, usage_log};

/// Memo files are a few hundred bytes; anything big is not ours.
const MEMO_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default)]
struct ImportReport {
    entries: usize,
    entries_present: usize,
    invalid: usize,
    objects_copied: usize,
    objects_present: usize,
    bytes_copied: u64,
    memo: usize,
}

fn read_last_used(dir: &Path) -> HashMap<String, i64> {
    let text = std::fs::read_to_string(dir.join(LAST_USED_FILE)).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (key, ts) = line.split_once('\t')?;
            Some((key.to_owned(), ts.trim().parse().ok()?))
        })
        .filter(|(key, _)| is_hex_name(key))
        .collect()
}

/// Parse an exported entry file; `None` unless it is a well-formed entry
/// for exactly the key its file name claims.
fn read_entry(path: &Path) -> Option<Entry> {
    let key = hex_stem(path)?;
    let entry: Entry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let valid = entry.version == ENTRY_VERSION
        && entry.key == key
        && is_hex_name(&entry.tier1)
        && entry.outputs.iter().all(|o| is_hex_name(&o.object));
    valid.then_some(entry)
}

/// Make sure every object of `entry` is in the store, copying (and
/// content-verifying) missing ones from `src`. False if one is missing or
/// does not hash to its name.
fn import_objects(
    store: &Store,
    src: &Store,
    entry: &Entry,
    report: &mut ImportReport,
) -> Result<bool> {
    for o in &entry.outputs {
        if std::fs::metadata(store.object_path(&o.object)).is_ok_and(|m| m.len() == o.size) {
            report.objects_present += 1;
            continue;
        }
        let from = src.object_path(&o.object);
        // Never follow a symlink planted in a restored archive.
        if !std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
            return Ok(false);
        }
        let (hash, size) = store
            .ingest_object(&from)
            .with_context(|| format!("import {}", from.display()))?;
        if hash != o.object || size != o.size {
            // Stored under its real hash, unreferenced: gc drops it later.
            return Ok(false);
        }
        report.objects_copied += 1;
        report.bytes_copied += size;
    }
    Ok(true)
}

/// Copy memo files the store does not have yet.
fn import_memo(store: &Store, src: &Store) -> Result<usize> {
    let mut files = Vec::new();
    walk_files(&src.memo_dir(), &mut files);
    let mut n = 0;
    for from in files {
        let Ok(rel) = from.strip_prefix(src.memo_dir()) else {
            continue;
        };
        let safe = rel.components().all(|c| match c {
            Component::Normal(name) => !name.to_string_lossy().starts_with('.'),
            _ => false,
        });
        let dst = store.memo_dir().join(rel);
        if !safe
            || dst.exists()
            || std::fs::metadata(&from).map_or(true, |m| m.len() > MEMO_MAX_BYTES)
        {
            continue;
        }
        let bytes = std::fs::read(&from).with_context(|| format!("read {}", from.display()))?;
        write_atomic(&dst, &bytes)?;
        n += 1;
    }
    Ok(n)
}

/// Copy cached test results the store does not have; each must parse and
/// carry the key it is named after.
fn import_tests(store: &Store, src: &Store) -> Result<usize> {
    let mut files = Vec::new();
    walk_files(&src.tests_dir(), &mut files);
    let mut n = 0;
    for from in files {
        let Some(key) = hex_stem(&from) else {
            continue;
        };
        let dst = store.test_result_path(&key);
        if dst.exists() {
            continue;
        }
        let bytes = std::fs::read(&from).with_context(|| format!("read {}", from.display()))?;
        let valid = serde_json::from_slice::<crate::testcache::TestResult>(&bytes)
            .is_ok_and(|r| r.key == key && r.version == crate::testcache::TEST_RESULT_VERSION);
        if valid {
            write_atomic(&dst, &bytes)?;
            n += 1;
        }
    }
    Ok(n)
}

pub fn run(root: &Path, dir: &Path, cow_mode: CowMode) -> Result<()> {
    let store = Store::open(root, cow_mode)?;
    // Exclusive: index.sqlite is only written under the exclusive lock.
    let _lock = store.exclusive_lock().context("lock store")?;
    let src = Store {
        root: dir.to_path_buf(),
        cow_mode,
    };
    let last_used = read_last_used(dir);
    let mut report = ImportReport::default();
    let mut registered: Vec<(Entry, i64)> = Vec::new();

    let mut files = Vec::new();
    walk_files(&src.entries_dir(), &mut files);
    files.sort();
    for path in files {
        let Some(entry) = read_entry(&path) else {
            if hex_stem(&path).is_some() {
                report.invalid += 1;
            }
            continue;
        };
        if store.read_entry(&entry.key).is_some() {
            report.entries_present += 1;
            continue;
        }
        if !import_objects(&store, &src, &entry, &mut report)? {
            report.invalid += 1;
            continue;
        }
        if store.read_manifest(&entry.tier1).is_none()
            && let Some(manifest) = std::fs::read(src.manifest_path(&entry.tier1))
                .ok()
                .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
        {
            store.write_manifest(&entry.tier1, &manifest)?;
        }
        store.write_entry(&entry)?;
        let used = last_used
            .get(&entry.key)
            .copied()
            .unwrap_or(entry.created as i64);
        registered.push((entry, used));
        report.entries += 1;
    }
    report.memo = import_memo(&store, &src)?;
    let test_results = import_tests(&store, &src)?;

    let mut conn = usage_log::open_index(&store.root)?;
    let tx = conn.transaction()?;
    for (entry, used) in &registered {
        tx.execute(
            "INSERT INTO entries (key, crate_name, size, created, last_used)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET last_used = max(last_used, ?5)",
            params![
                entry.key,
                entry.crate_name,
                entry.size() as i64,
                entry.created as i64,
                used
            ],
        )?;
    }
    tx.commit()?;

    crate::ui_info!(format!("imported {} entries", report.entries);
        objects_copied = report.objects_copied, size = human(report.bytes_copied),
        already_present = report.entries_present);
    if report.invalid > 0 {
        crate::ui_warn!(format!("skipped {} invalid or incomplete entries", report.invalid);
            dir = dir.display());
    }
    if report.memo > 0 {
        crate::ui_info!(format!("added {} memo files", report.memo));
    }
    if test_results > 0 {
        crate::ui_info!(format!("added {test_results} cached test results"));
    }
    Ok(())
}
