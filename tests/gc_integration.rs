//! `gc`, `gc --targets`, `gc --dedup` on real target dirs.

mod common;

use std::path::Path;

use common::Sandbox;

fn fresh(sb: &Sandbox, target: &str, args: &[&str]) {
    let mut all = vec!["build", "-v"];
    all.extend_from_slice(args);
    let out = sb.run(target, &all);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("Compiling"), "rebuilt after gc:\n{stderr}");
}

fn units(profile: &Path, pkg: &str) -> usize {
    std::fs::read_dir(profile.join(".fingerprint"))
        .unwrap()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.rsplit_once('-')
                .is_some_and(|(p, h)| p == pkg && h.len() == 16)
        })
        .count()
}

#[test]
fn dedup_shares_identical_build_output_without_rebuild() {
    let sb = Sandbox::new("dedup");
    sb.run("w1", &["build"]);
    sb.run("w2", &["build"]);
    let find_table = |t: &str| {
        let build = sb.root.join(t).join("debug/build");
        std::fs::read_dir(&build)
            .unwrap()
            .flatten()
            .map(|e| e.path().join("out/table.bin"))
            .find(|p| p.is_file())
            .unwrap()
    };
    let t2 = find_table("w2");
    let dry = sb.naspeed(&["gc", "--dedup", "--dry-run"]);
    if dry.contains("does not support clones") {
        // ext4, overlayfs, bind mounts: nothing to share, nothing touched.
        fresh(&sb, "w2", &[]);
        return;
    }
    assert!(dry.contains("would free"), "{dry}");
    let out = sb.naspeed(&["gc", "--dedup"]);
    assert!(out.contains("replaced"), "{out}");
    #[cfg(target_os = "macos")]
    assert_eq!(
        naspeed::store::physical_usage::private_size(&t2),
        Some(0),
        "{out}"
    );
    let _ = t2;
    fresh(&sb, "w1", &[]);
    fresh(&sb, "w2", &[]);
}

#[test]
fn targets_sweep_removes_old_feature_variant_only() {
    let sb = Sandbox::new("sweep");
    sb.run("t", &["build"]);
    sb.run("t", &["build", "--features", "greeting/loud"]);
    let profile = sb.root.join("t/debug");
    // Features change the lib, build-script compile and run units: 2 × 3.
    assert_eq!(units(&profile, "greeting"), 6);
    let out = sb.naspeed(&["gc", "--targets"]);
    assert!(out.contains("targets:"), "{out}");
    assert_eq!(units(&profile, "greeting"), 3, "{out}");
    fresh(&sb, "t", &["--features", "greeting/loud"]);
}

#[test]
fn targets_sweep_skips_dir_cargo_is_using() {
    let sb = Sandbox::new("busy");
    sb.run("t", &["build"]);
    sb.run("t", &["build", "--features", "greeting/loud"]);
    let profile = sb.root.join("t/debug");
    let before = units(&profile, "greeting");
    let lock_path = profile.join(".cargo-lock");
    let _cargo =
        naspeed::store::key_lock::lock(&lock_path, naspeed::store::key_lock::Mode::Exclusive)
            .unwrap();
    let out = sb.naspeed(&["gc", "--targets"]);
    assert!(out.contains("skipped 1 profile dirs"), "{out}");
    assert_eq!(units(&profile, "greeting"), before);
}

#[test]
fn store_lru_respects_cap_and_builds_stay_correct() {
    let sb = Sandbox::new("lru");
    sb.run("t1", &["build"]);
    let out = sb.naspeed(&["gc", "--max-size-gb", "0.0005"]);
    assert!(out.contains("removed"), "{out}");
    let objects = naspeed::store::physical_usage::tree_usage(&sb.store.join("objects"));
    assert!(objects.logical <= 512 * 1024 + 64 * 1024 * 1024, "{out}");
    let used = objects.logical;
    assert!(
        used <= (0.0005 * 1024.0 * 1024.0 * 1024.0) as u64,
        "store {used} bytes above cap: {out}"
    );
    sb.run("t2", &["build"]);
    let run = std::process::Command::new(sb.binary("t2"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&run.stdout).contains("lz4=true"));
}

#[test]
fn age_trim_evicts_unused_entries_under_the_cap() {
    let sb = Sandbox::new("age");
    sb.run("t1", &["build"]);
    // Far under the size cap, nothing is old: nothing goes.
    let out = sb.naspeed(&["gc", "--max-age-days", "14"]);
    assert!(out.contains(" 0 entries and"), "{out}");
    // Everything counts as unused once the limit is (almost) zero.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let out = sb.naspeed(&["gc", "--max-age-days", "0.00001"]);
    assert!(!out.contains(" 0 entries and"), "{out}");
    let entries = naspeed::store::physical_usage::tree_usage(&sb.store.join("entries"));
    assert_eq!(entries.files, 0, "{out}");
    // Builds still work from an empty store.
    sb.run("t2", &["build"]);
}
