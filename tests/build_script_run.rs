//! The build-script run cache must never store or serve an entry whose
//! replayed `cargo:` output advertises files under `OUT_DIR` that the
//! captured tree does not contain — the "empty capture" seen in CI, where
//! one launcher emptied `OUT_DIR` between another's run and its snapshot.
//!
//! These tests drive `naspeed __build-script` directly (what the launcher
//! execs) instead of a full cargo build: a registry-shaped manifest dir, a
//! unit-shaped `OUT_DIR` and the cargo env a build script sees.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

use common::{NASPEED, Sandbox};

fn write(path: &Path, content: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms).unwrap();
}

/// The package the launcher pretends to run for: a fake registry source
/// (`<CARGO_HOME>/registry/src/<index>/<pkg>-<ver>`), which is what makes
/// its run cacheable.
fn manifest_dir(sb: &Sandbox) -> PathBuf {
    sb.workspace
        .join("cargo-home/registry/src/index.crates.io-0123456789abcdef/gen_dep-1.0.0")
}

fn fake_package(sb: &Sandbox) {
    write(
        &manifest_dir(sb).join("Cargo.toml"),
        "[package]\nname = \"gen_dep\"\nversion = \"1.0.0\"\n",
        0o644,
    );
    write(&manifest_dir(sb).join(".cargo-ok"), "{}", 0o644);
}

fn out_dir(sb: &Sandbox) -> PathBuf {
    sb.root
        .join("target/debug/build/gen_dep-0123456789abcdef/out")
}

/// `naspeed __build-script <script> <hash>` with the env cargo gives a
/// build-script run. Extra `envs` land in the tier-1 key.
fn command(sb: &Sandbox, script: &Path, envs: &[(&str, &str)]) -> Command {
    // Cargo creates `out/` before running the script; the store lookup
    // resolves the volume through it.
    std::fs::create_dir_all(out_dir(sb)).unwrap();
    let mut cmd = Command::new(NASPEED);
    cmd.arg("__build-script")
        .arg(script)
        .arg("0000000000000000000000000000000000000000000000000000000000000000")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env(
            "RUSTC",
            std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()),
        )
        .env("OUT_DIR", out_dir(sb))
        .env("CARGO_MANIFEST_DIR", manifest_dir(sb))
        .env("CARGO_HOME", sb.workspace.join("cargo-home"))
        .env("CARGO_PKG_NAME", "gen_dep")
        .env("TARGET", "test-target")
        .env("HOST", "test-target")
        .env("PROFILE", "debug")
        .env("NASPEED_CONFIG_DIR", &sb.config)
        .env("NASPEED_STORE", &sb.store)
        .env("NASPEED_REMOTES", "")
        .env("NASPEED_MIN_FREE_DISK_PCT", "0")
        .env("NASPEED_NO_AUTO_GC", "1")
        .envs(envs.iter().map(|(k, v)| (*k, *v)));
    for opt in ["RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO"] {
        if let Ok(v) = std::env::var(opt) {
            cmd.env(opt, v);
        }
    }
    cmd
}

/// Run the launcher to completion; returns its exit code.
fn run_once(sb: &Sandbox, script: &Path, envs: &[(&str, &str)]) -> i32 {
    let out = command(sb, script, envs).output().unwrap();
    assert!(
        out.status.success(),
        "build-script run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.status.code().unwrap_or(-1)
}

/// Wait until `path` exists; panics after `timeout`.
fn wait_for(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn entry_files(sb: &Sandbox) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for shard in std::fs::read_dir(sb.store.join("entries"))
        .unwrap()
        .flatten()
    {
        for entry in std::fs::read_dir(shard.path()).unwrap().flatten() {
            files.push(entry.path());
        }
    }
    files.sort();
    files
}

/// Every file/dir name any stored entry captured under `OUT_DIR`.
fn captured_names(sb: &Sandbox) -> Vec<String> {
    let mut names = Vec::new();
    for f in entry_files(sb) {
        let entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        for field in ["outputs", "texts"] {
            for o in entry[field].as_array().into_iter().flatten() {
                names.push(o["name"].as_str().unwrap_or_default().to_string());
            }
        }
        for d in entry["dirs"].as_array().into_iter().flatten() {
            names.push(d.as_str().unwrap_or_default().to_string());
        }
        for s in entry["symlinks"].as_array().into_iter().flatten() {
            names.push(s[0].as_str().unwrap_or_default().to_string());
        }
    }
    names
}

/// A stored run that advertises `OUT_DIR` paths it does not contain is
/// poison: replaying it hands cargo paths to files that are not there
/// (the mime_guess incident: `MIME_TYPES_GENERATED_PATH` pointed at a
/// `mime_types_generated.rs` the restore never wrote). The launcher must
/// drop the entry and run the script for real.
#[test]
fn poisoned_entry_is_dropped_and_the_script_reruns() {
    let sb = Sandbox::new("bs-poison");
    fake_package(&sb);
    let real = sb.root.join("real");
    write(
        &real,
        "#!/bin/sh\n\
         echo \"data\" > \"$OUT_DIR/gen.rs\"\n\
         echo \"cargo:rustc-env=G_PATH=$OUT_DIR/gen.rs\"\n",
        0o755,
    );
    assert_eq!(run_once(&sb, &real, &[]), 0);

    // Corrupt the stored entry into the shape the bug produced: output
    // advertising `gen.rs`, a capture containing nothing at all.
    let files = entry_files(&sb);
    let [ref entry] = files[..] else {
        panic!("expected exactly one stored entry")
    };
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entry).unwrap()).unwrap();
    for field in ["outputs", "texts", "dirs", "symlinks"] {
        json[field] = serde_json::json!([]);
    }
    std::fs::write(entry, serde_json::to_vec(&json).unwrap()).unwrap();
    std::fs::remove_dir_all(out_dir(&sb)).unwrap();

    // The corrupt entry must not be served: the run re-executes the script
    // and `gen.rs` exists again (it would be missing after a bad replay).
    assert_eq!(run_once(&sb, &real, &[]), 0);
    assert!(out_dir(&sb).join("gen.rs").is_file());
}

/// Even when a run itself produces the unfaithful capture, the entry must
/// not be stored: a `cargo:` line advertising an `OUT_DIR` path that is
/// absent when the script exits means the capture does not match the
/// output being replayed.
#[test]
fn capture_advertising_missing_paths_is_not_stored() {
    let sb = Sandbox::new("bs-uncovered");
    fake_package(&sb);
    let real = sb.root.join("real");
    write(
        &real,
        "#!/bin/sh\n\
         mkdir -p \"$OUT_DIR\"\n\
         echo \"cargo:rustc-env=GHOST=$OUT_DIR/ghost.rs\"\n",
        0o755,
    );
    assert_eq!(run_once(&sb, &real, &[]), 0);
    assert!(
        entry_files(&sb).is_empty(),
        "a run advertising paths it did not leave behind must not be cached"
    );
}

/// Two launchers can reach the same `OUT_DIR` concurrently — an orphaned
/// launcher whose cargo was killed overlapping the restarted build, or
/// two invocations whose environments give them different tier-1 keys
/// (the key lock does not serialize them). `OUT_DIR` mutation must be
/// serialized so a restore cannot empty the dir between a run and its
/// snapshot.
#[test]
fn concurrent_launchers_on_one_out_dir_do_not_corrupt_the_capture() {
    let sb = Sandbox::new("bs-race");
    fake_package(&sb);
    let out = out_dir(&sb);
    let started = sb.root.join("a-started");
    let go = sb.root.join("go");

    // B's script leaves `b.rs`; A's leaves `a.rs`, then blocks on `go` so
    // a concurrent restore can land between its run and its snapshot.
    // Distinct `CARGO_FEATURE_*` values → distinct tier-1 keys.
    let script_a = sb.root.join("script-a");
    write(
        &script_a,
        &format!(
            "#!/bin/sh\n\
             mkdir -p \"$OUT_DIR\"\n\
             echo \"data\" > \"$OUT_DIR/a.rs\"\n\
             echo started > \"{}\"\n\
             while [ ! -f \"{}\" ]; do sleep 0.05; done\n\
             echo \"cargo:rustc-env=A_PATH=$OUT_DIR/a.rs\"\n",
            started.display(),
            go.display()
        ),
        0o755,
    );
    let script_b = sb.root.join("script-b");
    write(
        &script_b,
        "#!/bin/sh\n\
         echo \"data\" > \"$OUT_DIR/b.rs\"\n\
         echo \"cargo:rustc-env=B_PATH=$OUT_DIR/b.rs\"\n",
        0o755,
    );

    // Seed B's entry (the same env the concurrent invocation replays).
    let env_b: &[(&str, &str)] = &[("CARGO_FEATURE_B", "1")];
    assert_eq!(run_once(&sb, &script_b, env_b), 0);
    assert!(out.join("b.rs").is_file());

    // A starts and writes `a.rs`; B then replays its stored run into the
    // same OUT_DIR while A is parked before its snapshot.
    let env_a: &[(&str, &str)] = &[("CARGO_FEATURE_A", "1")];
    let mut a_child = command(&sb, &script_a, env_a)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&started, Duration::from_secs(10));
    let mut b_child = command(&sb, &script_b, env_b).spawn().unwrap();

    // Unserialized, B's restore finishes quickly and empties `a.rs` away.
    // Serialized, B blocks on the unit-dir lock until A is done — give it
    // long enough to have restored, then release A either way.
    let deadline = Instant::now() + Duration::from_secs(10);
    while b_child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    write(&go, "go", 0o644);

    assert!(a_child.wait().unwrap().success());
    assert!(b_child.wait().unwrap().success());
    assert!(
        out.join("b.rs").is_file(),
        "B's restore ran last and must have left its tree"
    );

    // B replayed its stored run rather than running the script again.
    let usage = std::fs::read_to_string(sb.store.join("usage.log")).unwrap();
    assert!(
        usage.lines().any(|l| l.starts_with('H')),
        "B was expected to hit its seeded entry:\n{usage}"
    );

    // A's snapshot must contain the file its own script wrote, not the
    // tree a concurrent restore swapped in mid-run.
    let names = captured_names(&sb);
    assert!(
        names.iter().any(|n| n == "a.rs"),
        "A was snapshotted after B emptied OUT_DIR: its entry holds {names:?}"
    );
}
