//! End-to-end: real cargo + rustc + registry crates through the naspeed
//! wrapper, store and target dirs on the same volume (under
//! `CARGO_TARGET_TMPDIR`, i.e. this crate's target dir).
//!
//! Needs the sample workspace's dependencies in the local registry
//! (`cargo fetch` once); every build runs `--offline`.

use std::collections::HashSet;
use std::process::Command;

mod common;

use common::Sandbox;

#[test]
fn second_target_dir_hits_and_stays_fresh() {
    let sb = Sandbox::new("hit");
    sb.run("t1", &["build"]);
    let (h1, m1) = sb.usage();
    assert!(h1.is_empty(), "store started empty");
    assert!(
        m1.len() >= 10,
        "registry crates should be stored, got {}",
        m1.len()
    );

    sb.run("t2", &["build"]);
    let (h2, m2) = sb.usage();
    let new_hits = h2.len();
    let new_misses = m2.len() - m1.len();
    let rate = new_hits as f64 / (new_hits + new_misses) as f64;
    assert!(
        rate >= 0.95,
        "hit rate {rate} ({new_hits} hits, {new_misses} misses)"
    );

    // Third build: cargo must consider every unit fresh.
    let out = sb.run("t2", &["build", "-v"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Compiling"),
        "rebuilt something:\n{stderr}"
    );
    assert!(stderr.contains("Fresh"), "{stderr}");

    let run = Command::new(sb.binary("t2")).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        r#"{"text":"hello","count":3} 42 lz4=true"#
    );

    // Restored registry artifacts are clones: almost no private bytes.
    #[cfg(target_os = "macos")]
    {
        let deps1 = naspeed::store::physical_usage::tree_usage(&sb.root.join("t1/debug/deps"));
        let restored: u64 = std::fs::read_dir(sb.root.join("t2/debug/deps"))
            .unwrap()
            .flatten()
            .filter(|e| naspeed::wrapper::extern_identity::is_placed(&e.path()))
            .map(|e| {
                naspeed::store::physical_usage::file_usage(&e.path())
                    .private
                    .unwrap_or(0)
            })
            .sum();
        assert!(
            restored < deps1.logical / 10,
            "restored files use {restored} private bytes vs {} logical deps",
            deps1.logical
        );
    }

    // Rebuilding over restored (read-only object clones) files must work:
    // CFLAGS changes lz4-sys's key and reruns its build script.
    let out = sb
        .cargo("t2", &["build"])
        .env("CFLAGS", "-O1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn concurrent_builds_compile_each_key_once() {
    let sb = Sandbox::new("concurrent");
    let mut a = sb.cargo("c1", &["build"]).spawn().unwrap();
    let mut b = sb.cargo("c2", &["build"]).spawn().unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let (hits, misses) = sb.usage();
    let mut seen = HashSet::new();
    let dup: Vec<_> = misses.iter().filter(|k| !seen.insert(k.as_str())).collect();
    assert!(dup.is_empty(), "keys compiled twice: {dup:?}");
    assert!(!hits.is_empty());
}

#[test]
fn missing_store_passes_through() {
    let sb = Sandbox::new("nostore");
    let out = sb
        .cargo("t", &["build"])
        .env("NASPEED_STORE", sb.root.join("unplugged"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!sb.store.join("usage.log").exists());
    assert!(sb.binary("t").is_file());
}

#[test]
fn slots_limit_concurrent_rustc() {
    let sb = Sandbox::new("slots");
    let log = sb.root.join("wrapper.log");
    let mut children = Vec::new();
    for t in ["s1", "s2", "s3"] {
        children.push(
            sb.cargo(t, &["build"])
                .env("NASPEED_SLOTS", "2")
                .env("NASPEED_LOG", "naspeed=debug")
                .env("NASPEED_LOG_FILE", &log)
                .spawn()
                .unwrap(),
        );
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let max = max_concurrent(&std::fs::read_to_string(&log).unwrap());
    assert!(
        (1..=2).contains(&max),
        "{max} rustc ran at once with 2 slots"
    );
}

/// Replay "rustc start/end" events with pids and return peak concurrency.
fn max_concurrent(log: &str) -> usize {
    let mut events: Vec<(String, i32)> = Vec::new();
    for line in log.lines() {
        // tracing line: "<timestamp> DEBUG ... rustc start pid=…"
        let ts = line.split_whitespace().next().unwrap_or("").to_owned();
        if line.contains("rustc start") {
            events.push((ts, 1));
        } else if line.contains("rustc end") {
            events.push((ts, -1));
        }
    }
    // Ends sort before starts at equal timestamps.
    events.sort();
    let mut cur = 0i32;
    let mut max = 0i32;
    for (_, d) in events {
        cur += d;
        max = max.max(cur);
    }
    max as usize
}

/// Precondition for `cow_mode = "hardlink"`: when rustc rebuilds a crate
/// it must replace its outputs (new inode), never write into the existing
/// file, or a hardlinked store object would be changed through the target.
#[test]
#[cfg(unix)]
fn rustc_rebuild_replaces_output_inode() {
    use std::os::unix::fs::MetadataExt;
    let sb = Sandbox::new("inode");
    sb.run("t", &["build"]);
    let deps = sb.root.join("t/debug/deps");
    let rlib = || {
        std::fs::read_dir(&deps)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with("liblz4_sys-") && n.ends_with(".rlib")
            })
            .unwrap()
    };
    let before = rlib();
    let ino = std::fs::metadata(&before).unwrap().ino();
    // CFLAGS reruns lz4-sys's build script, so rustc rebuilds the same unit.
    let out = sb
        .cargo("t", &["build"])
        .env("CFLAGS", "-O1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = rlib();
    assert_eq!(before, after, "same unit hash, same file name");
    assert_ne!(
        std::fs::metadata(&after).unwrap().ino(),
        ino,
        "rustc wrote in place"
    );
}

/// lz4-sys compiles C in its build script; a second target dir must reuse
/// that run (OUT_DIR + directives) instead of compiling again.
#[test]
#[cfg(unix)]
fn build_script_run_is_reused_across_target_dirs() {
    let sb = Sandbox::new("bsrun");
    let log = sb.root.join("naspeed.log");
    let logged = |target: &str, extra: &[(&str, &str)]| {
        let mut cmd = sb.cargo(target, &["build"]);
        cmd.env("NASPEED_LOG", "naspeed=info")
            .env("NASPEED_LOG_FILE", &log);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let count = |needle: &str| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .matches(needle)
            .count()
    };

    logged("t1", &[]);
    let stored = count("build script stored");
    assert!(stored >= 1, "lz4-sys run should be stored");
    assert_eq!(count("build script hit"), 0);

    let misses_after_t1 = sb.usage().1.len();
    logged("t2", &[]);
    assert!(
        count("build script hit") >= stored,
        "second target dir must reuse runs"
    );
    assert_eq!(count("build script stored"), stored);
    // lz4_sys's rlib is keyed on its OUT_DIR, which was just restored with
    // t2's paths: it must still hit, like every other registry unit.
    assert_eq!(sb.usage().1.len(), misses_after_t1, "new misses in t2");
    let run = Command::new(sb.binary("t2")).output().unwrap();
    assert!(String::from_utf8_lossy(&run.stdout).contains("lz4=true"));

    // The restored OUT_DIR holds the C library, and the launcher replaced the
    // build-script binary.
    let build = sb.root.join("t2/debug/build");
    let lib = std::fs::read_dir(&build)
        .unwrap()
        .flatten()
        .map(|e| e.path().join("out/liblz4.a"))
        .find(|p| p.is_file());
    assert!(lib.is_some(), "liblz4.a restored into OUT_DIR");
    let launcher = std::fs::read_dir(&build).unwrap().flatten().any(|e| {
        naspeed::wrapper::build_script_run::is_launcher(&e.path().join("build-script-build"))
    });
    assert!(launcher);

    // A changed C flag is a different key: the script really runs again.
    logged("t2", &[("CFLAGS", "-O1")]);
    assert!(count("build script stored") > stored);
    let run = Command::new(sb.binary("t2")).output().unwrap();
    assert!(String::from_utf8_lossy(&run.stdout).contains("lz4=true"));
}

/// Workspace crates: a second checkout at another path reuses `greeting`;
/// an edit that changes behaviour misses and the new behaviour shows up.
#[test]
fn workspace_crate_is_shared_across_checkouts_and_edits_miss() {
    let sb = Sandbox::new("wscache");
    let ws2 = sb.root.join("checkout-two-at-another-path");
    common::copy_dir(&sb.workspace, &ws2);
    let log = sb.root.join("naspeed.log");
    let build = |dir: &std::path::Path, target: &str| {
        let out = sb
            .cargo(target, &["build"])
            .current_dir(dir)
            .env("NASPEED_LOG", "naspeed=info")
            .env("NASPEED_LOG_FILE", &log)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Command::new(sb.root.join(target).join("debug/sample-app"))
            .output()
            .unwrap()
    };
    let greeting_hits = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(": hit crate_name=greeting"))
            .count()
    };

    let first = build(&sb.workspace, "a");
    assert_eq!(greeting_hits(), 0);
    let second = build(&ws2, "b");
    assert_eq!(first.stdout, second.stdout);
    assert!(
        greeting_hits() >= 1,
        "greeting should hit in the second checkout"
    );

    // Rename the field: a stale greeting rlib would not even compile
    // against the edited main.rs, and the JSON key must change.
    for f in ["greeting/src/lib.rs", "src/main.rs"] {
        let p = ws2.join(f);
        let text = std::fs::read_to_string(&p)
            .unwrap()
            .replace("text", "message");
        std::fs::write(&p, text).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let hits_before = greeting_hits();
    let edited = build(&ws2, "b");
    assert_eq!(greeting_hits(), hits_before, "edited crate must not hit");
    assert!(
        String::from_utf8_lossy(&edited.stdout).starts_with(r#"{"message":"hello""#),
        "{}",
        String::from_utf8_lossy(&edited.stdout)
    );
}
