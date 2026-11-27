//! Native Cargo planning with immutable workspace-library actions.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NASPEED: &str = env!("CARGO_BIN_EXE_naspeed");

fn checkout(root: &Path) {
    fs::create_dir_all(root.join("core/src")).unwrap();
    fs::create_dir_all(root.join("app/src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = ['core', 'app']\nresolver = '3'\n",
    )
    .unwrap();
    fs::write(
        root.join("core/Cargo.toml"),
        "[package]\nname = 'engine-core'\nversion = '0.1.0'\nedition = '2024'\n",
    )
    .unwrap();
    fs::write(
        root.join("core/src/lib.rs"),
        "pub fn value() -> u32 { 17 }\n",
    )
    .unwrap();
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname = 'engine-app'\nversion = '0.1.0'\nedition = '2024'\n[dependencies]\nengine-core = { path = '../core' }\n",
    )
    .unwrap();
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", engine_core::value()); }\n#[test] fn check() { assert!(engine_core::value() > 0); }\n",
    )
    .unwrap();
}

fn library(root: &Path) -> PathBuf {
    let mut pending = vec![root.join("target/debug")];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(p);
            } else if p.extension().is_some_and(|e| e == "rlib")
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("libengine_core-")
            {
                return p;
            }
        }
    }
    panic!("no engine-core rlib in {}", root.display());
}

fn assert_success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn three_checkouts_reuse_one_verified_library_and_keep_edits_private() {
    let sandbox = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = sandbox.path();
    let ns = root.join("ns");
    let store = root.join("store");
    let log = root.join("compiler.log");
    fs::create_dir(&store).unwrap();
    symlink(NASPEED, &ns).unwrap();
    let run = |at: &Path, args: &[&str]| {
        let mut command = Command::new(&ns);
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("NASPEED_")
                || k.starts_with("RUSTC_")
                || k == "RUSTFLAGS"
            {
                command.env_remove(k.as_ref());
            }
        }
        assert_success(
            command
                .args(args)
                .current_dir(at)
                .env("RUSTC_WRAPPER", NASPEED)
                .env("CARGO_TARGET_DIR", at.join("target"))
                .env("CARGO_INCREMENTAL", "0")
                .env("NASPEED_CONFIG_DIR", root.join("config"))
                .env("NASPEED_STORE", &store)
                .env("NASPEED_RUST_ACTION_CACHE", "1")
                .env("NASPEED_COW_MODE", "hardlink")
                .env("NASPEED_MIN_FREE_DISK_PCT", "0")
                .env("NASPEED_MIN_FREE_MEM_MB", "0")
                .env("NASPEED_NO_AUTO_GC", "1")
                .env("NASPEED_TEST_TRACE", "0")
                .env("NASPEED_LOG", "naspeed=info")
                .env("NASPEED_LOG_FILE", &log)
                .env("NO_COLOR", "1")
                .output()
                .unwrap(),
        )
    };
    let counts = || {
        let text = fs::read_to_string(store.join("usage.log")).unwrap();
        let count = |prefix: &str| text.lines().filter(|l| l.starts_with(prefix)).count();
        (count("M\t"), count("H\t"))
    };
    let copies = [root.join("first"), root.join("second"), root.join("third")];
    for at in &copies {
        checkout(at);
        run(at, &["build", "--offline", "-p", "engine-app"]);
        let app = run(at, &["run", "--offline", "-p", "engine-app"]);
        assert_eq!(app.stdout, b"17\n");
    }
    assert_eq!(
        counts(),
        (1, 2),
        "only the first checkout compiles the library"
    );
    let first = library(&copies[0]);
    let digest = naspeed::store::content_hash::hash_file(&first)
        .unwrap()
        .to_hex()
        .to_string();
    let cas = store.join("objects").join(&digest[..2]).join(&digest);
    let meta = fs::metadata(&cas).unwrap();
    assert_eq!(meta.mode() & 0o222, 0, "CAS remains read-only");
    for at in &copies {
        let placed = fs::metadata(library(at)).unwrap();
        assert_eq!((placed.dev(), placed.ino()), (meta.dev(), meta.ino()));
    }
    assert!(
        fs::read_dir(store.join("entries"))
            .unwrap()
            .next()
            .is_none()
    );

    for at in &copies {
        run(at, &["test", "--offline", "-p", "engine-app"]);
    }
    let repeated = run(&copies[1], &["test", "--offline", "-p", "engine-app"]);
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("(cached)"));

    fs::write(
        copies[1].join("core/src/lib.rs"),
        "pub fn value() -> u32 { 42 }\n",
    )
    .unwrap();
    let edited = run(&copies[1], &["run", "--offline", "-p", "engine-app"]);
    assert_eq!(edited.stdout, b"42\n");
    assert_ne!(fs::metadata(library(&copies[1])).unwrap().ino(), meta.ino());
    assert_eq!(fs::metadata(&first).unwrap().ino(), meta.ino());
    assert_eq!(
        run(&copies[2], &["run", "--offline", "-p", "engine-app"]).stdout,
        b"17\n"
    );

    // Replace the store path without mutating inodes already used by the
    // old checkouts. The corrupt object has the correct length.
    let broken = root.join("broken");
    fs::write(&broken, vec![0; meta.len() as usize]).unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o444)).unwrap();
    fs::rename(&broken, &cas).unwrap();
    let recovery = root.join("recovery");
    checkout(&recovery);
    let recovering = run(&recovery, &["run", "--offline", "-p", "engine-app"]);
    assert_eq!(recovering.stdout, b"17\n");
    assert_eq!(
        naspeed::store::content_hash::hash_file(&cas)
            .unwrap()
            .to_hex()
            .as_str(),
        digest
    );
}
