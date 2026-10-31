//! `ns cache export|import` end to end: a store exported after one build,
//! imported into an empty store, serves a fresh checkout at another path;
//! the export size cap is honoured and re-exports prune what no longer fits.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

use common::{NASPEED, Sandbox};

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let t = e.file_type().unwrap();
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    out
}

fn bytes_under(dir: &Path) -> u64 {
    files_under(dir)
        .iter()
        .map(|f| std::fs::metadata(f).unwrap().len())
        .sum()
}

/// `naspeed cache ...` against an explicit store.
fn cache_cmd(sb: &Sandbox, store: &Path, args: &[&str]) -> String {
    let out = Command::new(NASPEED)
        .env("NO_COLOR", "1")
        .arg("cache")
        .args(args)
        .env("NASPEED_CONFIG_DIR", &sb.config)
        .env("NASPEED_STORE", store)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "cache {args:?} failed: {text}");
    text
}

fn usage(store: &Path) -> (usize, usize) {
    let text = std::fs::read_to_string(store.join("usage.log")).unwrap_or_default();
    let count = |kind: &str| text.lines().filter(|l| l.starts_with(kind)).count();
    (count("H\t"), count("M\t"))
}

#[test]
fn imported_store_serves_a_fresh_checkout() {
    let sb = Sandbox::new("xfer");
    sb.run("t1", &["build"]);
    let (_, cold_misses) = usage(&sb.store);
    assert!(cold_misses >= 10, "cold build stored {cold_misses} entries");

    let export = sb.root.join("ci-cache");
    let text = cache_cmd(&sb, &sb.store, &["export", export.to_str().unwrap()]);
    assert!(text.contains("exported"), "{text}");
    let exported = files_under(&export.join("entries")).len();
    let stored = files_under(&sb.store.join("entries")).len();
    assert_eq!(exported, stored, "every entry fits in 5 GB: {text}");

    // A second runner: empty store, checkout at another path.
    let store2 = sb.root.join("store-two");
    let ws2 = sb.root.join("fresh-checkout");
    common::copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_workspace"),
        &ws2,
    );
    let text = cache_cmd(&sb, &store2, &["import", export.to_str().unwrap()]);
    assert!(
        text.contains(&format!("imported {exported} entries")),
        "{text}"
    );
    let again = cache_cmd(&sb, &store2, &["import", export.to_str().unwrap()]);
    assert!(again.contains("imported 0 entries"), "{again}");

    // Imported entries are in the LRU index.
    assert_eq!(indexed_entries(&store2), exported);

    let out = sb
        .cargo("t2", &["build"])
        .current_dir(&ws2)
        .env("NASPEED_STORE", &store2)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (hits, misses) = usage(&store2);
    let rate = hits as f64 / (hits + misses) as f64;
    eprintln!("fresh checkout after import: {hits} hits, {misses} misses\n{text}");
    assert!(
        rate >= 0.95,
        "hit rate {rate} ({hits} hits, {misses} misses)"
    );
    let run = Command::new(sb.root.join("t2/debug/sample-app"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        r#"{"text":"hello","count":3} 42 lz4=true"#
    );
}

/// Entries registered in the store's LRU index.
fn indexed_entries(store: &Path) -> usize {
    let conn = rusqlite::Connection::open(store.join("index.sqlite")).unwrap();
    conn.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
        .unwrap() as usize
}

#[test]
fn export_honours_the_size_cap_and_prunes() {
    let sb = Sandbox::new("xfercap");
    sb.run("t1", &["build"]);
    let export = sb.root.join("ci-cache");
    let dir = export.to_str().unwrap();

    cache_cmd(&sb, &sb.store, &["export", dir]);
    let full_entries = files_under(&export.join("entries")).len();
    let full_bytes = bytes_under(&export.join("objects"));
    assert!(full_entries >= 10 && full_bytes > 0);

    // Re-export of an unchanged store copies nothing.
    let text = cache_cmd(&sb, &sb.store, &["export", dir]);
    assert!(text.contains("copied 0 objects"), "{text}");

    // A cap below the full size: fewer entries, objects within the cap,
    // and the earlier, larger export is pruned down to the new selection.
    let cap_bytes = full_bytes / 4;
    let cap_gb = cap_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    cache_cmd(
        &sb,
        &sb.store,
        &["export", dir, "--max-gb", &format!("{cap_gb:.9}")],
    );
    let capped_entries = files_under(&export.join("entries")).len();
    let capped_bytes = bytes_under(&export.join("objects"));
    assert!(
        capped_entries > 0 && capped_entries < full_entries,
        "{capped_entries} of {full_entries} entries"
    );
    assert!(
        capped_bytes <= cap_bytes,
        "{capped_bytes} bytes exported over a {cap_bytes}-byte cap"
    );

    cache_cmd(&sb, &sb.store, &["export", dir, "--max-gb", "0"]);
    assert!(files_under(&export.join("entries")).is_empty());
    assert_eq!(bytes_under(&export.join("objects")), 0);
}

#[test]
fn import_rejects_tampered_objects() {
    let sb = Sandbox::new("xfertamper");
    sb.run("t1", &["build"]);
    let export = sb.root.join("ci-cache");
    let dir = export.to_str().unwrap();
    cache_cmd(&sb, &sb.store, &["export", dir]);
    let objects = files_under(&export.join("objects"));
    let victim = objects
        .iter()
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .unwrap();
    let len = std::fs::metadata(victim).unwrap().len() as usize;
    std::fs::remove_file(victim).unwrap();
    std::fs::write(victim, vec![0u8; len]).unwrap();

    let store2 = sb.root.join("store-two");
    let text = cache_cmd(&sb, &store2, &["import", dir]);
    assert!(text.contains("invalid or incomplete"), "{text}");
    let victim_name = victim.file_name().unwrap().to_str().unwrap();
    for entry in files_under(&store2.join("entries")) {
        let json = std::fs::read_to_string(entry).unwrap();
        assert!(
            !json.contains(victim_name),
            "entry with a tampered object imported"
        );
    }
}
