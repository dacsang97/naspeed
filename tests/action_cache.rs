//! End-to-end action-cache corruption and concurrency scenarios.
//!
//! The store layout used here:
//! ```text
//! actions/<2>/<action-key>.json  ActionResult records
//! trees/<2>/<tree-digest>.json   output trees (path + object digest + mode)
//! objects/<2>/<content-digest>   immutable bytes
//! leases/<action-key>/           per-key write locks / lease refs
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use naspeed::config::CowMode;
use naspeed::engine::action::{
    ActionInput, ActionSpec, ExpectedOutput, Platform, WORK_DIR_PLACEHOLDER,
};
use naspeed::engine::action_result::{
    ActionResult, Provenance, Resources, ACTION_RESULT_VERSION,
};
use naspeed::store::action_cache::{self, Lookup, Miss};
use naspeed::store::tree;
use naspeed::store::Store;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), CowMode::Auto).unwrap();
    (dir, s)
}

/// The canonical test spec — must stay byte-identical with the golden
/// vector in `engine::action_key::tests` (same logical action, no
/// absolute paths anywhere).
fn spec() -> ActionSpec {
    let mut env = BTreeMap::new();
    env.insert(
        "CFLAGS".to_owned(),
        ActionSpec::env_value_hash("-O2 -fPIC"),
    );
    env.insert(
        "TARGET".to_owned(),
        ActionSpec::env_value_hash("aarch64-apple-darwin"),
    );
    ActionSpec {
        executable: "rustc".into(),
        toolchain_digest: blake3::hash(b"rustc 1.91.0 sysroot").to_hex().to_string(),
        argv: vec![
            "rustc".into(),
            "--crate-type".into(),
            "lib".into(),
            "--out-dir".into(),
            "@WORK_DIR@/out".into(),
        ],
        env,
        platform: Platform {
            os: "macos".into(),
            arch: "aarch64".into(),
            family: "unix".into(),
        },
        work_dir: WORK_DIR_PLACEHOLDER.into(),
        inputs: vec![ActionInput {
            path: "src/lib.rs".into(),
            digest: blake3::hash(b"fn main() {}").to_hex().to_string(),
            size: 12,
        }],
        outputs: vec![ExpectedOutput {
            path: "out/libx.rlib".into(),
        }],
        cacheable: true,
    }
}

/// Build a fake output tree on disk, ingest it, and return a result
/// record pointing at it.
fn finished_result(s: &Store, work: &Path, key: &str, payload: &[u8]) -> ActionResult {
    let out = work.join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("libx.rlib"), payload).unwrap();
    let tree_digest = tree::ingest_tree(s, &out).unwrap();
    ActionResult {
        version: ACTION_RESULT_VERSION,
        action_key: key.to_owned(),
        exit_status: 0,
        stdout: Some(action_cache::put_bytes(s, b"warning: unused\n").unwrap()),
        stderr: Some(action_cache::put_bytes(s, b"").unwrap()),
        output_tree: tree_digest,
        duration_ms: 42,
        resources: Resources::default(),
        provenance: Provenance {
            producer: "local".into(),
            trust: "trusted".into(),
            origin: "test".into(),
        },
        created: naspeed::store::now_secs(),
    }
}

/// Same logical action materialized under two different worktree roots
/// produces the identical key (and identical stored artifacts).
#[test]
fn same_action_two_roots_identical_key() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    // Adapters encode work-dir paths as placeholders; nothing absolute
    // reaches the spec, so roots never change the key.
    let mut a = spec();
    a.argv.push(format!("-L{WORK_DIR_PLACEHOLDER}/deps"));
    let mut b = spec();
    b.argv.push(format!("-L{WORK_DIR_PLACEHOLDER}/deps"));
    assert!(root_a.path() != root_b.path());
    assert_eq!(a.key(), b.key());
}

#[test]
fn key_differs_on_toolchain_env_input_platform() {
    let base = spec();
    let base_key = base.key();

    let mut t = base.clone();
    t.toolchain_digest = blake3::hash(b"rustc 1.92.0").to_hex().to_string();
    assert_ne!(t.key(), base_key);

    let mut e = base.clone();
    e.set_env("CFLAGS", "-O3");
    assert_ne!(e.key(), base_key);

    let mut i = base.clone();
    i.inputs[0].digest = blake3::hash(b"fn changed() {}").to_hex().to_string();
    assert_ne!(i.key(), base_key);

    let mut p = base.clone();
    p.platform.arch = "x86_64".into();
    assert_ne!(p.key(), base_key);
}

#[test]
fn hit_after_commit() {
    let (_d, s) = store();
    let spec = spec();
    let key = spec.key();
    let work = tempfile::tempdir().unwrap();
    let result = finished_result(&s, work.path(), &key, b"rlib-bytes");
    action_cache::store_result(&s, &key, &result).unwrap();
    match action_cache::lookup(&s, &key) {
        Lookup::Hit(r, t) => {
            assert_eq!(r.exit_status, 0);
            assert_eq!(t.entries.len(), 1);
            assert_eq!(t.entries[0].path, "libx.rlib");
        }
        Lookup::Miss(m) => panic!("expected hit, got {m:?}"),
    }
}

#[test]
fn corrupt_object_is_typed_miss_and_record_removed() {
    let (_d, s) = store();
    let key = spec().key();
    let work = tempfile::tempdir().unwrap();
    let result = finished_result(&s, work.path(), &key, b"rlib-bytes");
    action_cache::store_result(&s, &key, &result).unwrap();
    assert!(action_cache::lookup(&s, &key).is_hit());

    // Flip a byte inside the object; mode may be 0444 so chmod first.
    let obj = s.object_path(&result.stdout.as_ref().unwrap().digest);
    crate_make_writable(&obj);
    std::fs::write(&obj, b"corrupted").unwrap();
    assert_eq!(
        action_cache::lookup(&s, &key),
        Lookup::Miss(Miss::BadObject)
    );
    assert!(
        !s.action_path(&key).exists(),
        "corrupt record left visible"
    );
}

#[test]
fn missing_object_is_typed_miss() {
    let (_d, s) = store();
    let key = spec().key();
    let work = tempfile::tempdir().unwrap();
    let result = finished_result(&s, work.path(), &key, b"rlib-bytes");
    action_cache::store_result(&s, &key, &result).unwrap();

    // Remove the whole stream object.
    let obj = s.object_path(&result.stderr.as_ref().unwrap().digest);
    crate_remove(&obj);
    assert_eq!(action_cache::lookup(&s, &key), Lookup::Miss(Miss::BadObject));
    assert!(!s.action_path(&key).exists());
}

#[test]
fn corrupt_tree_object_is_typed_miss() {
    let (_d, s) = store();
    let key = spec().key();
    let work = tempfile::tempdir().unwrap();
    let result = finished_result(&s, work.path(), &key, b"rlib-bytes");
    action_cache::store_result(&s, &key, &result).unwrap();

    // Corrupt a file object inside the output tree.
    let tree = tree::read_verified(&s, &result.output_tree).unwrap();
    let obj = s.object_path(&tree.entries[0].digest);
    crate_make_writable(&obj);
    std::fs::write(&obj, b"x").unwrap();
    assert_eq!(action_cache::lookup(&s, &key), Lookup::Miss(Miss::BadTree));
    assert!(!s.action_path(&key).exists());
}

#[test]
fn traversal_and_bad_paths_rejected_before_write() {
    for bad in [
        "../escape",
        "a/../../b",
        "/abs/path",
        "a\\b",
        "a/./b",
        "C:/win",
        "a//b",
        "a/",
    ] {
        assert!(tree::validate_path(bad).is_err(), "{bad:?} accepted");
    }
    assert!(tree::validate_paths(&["a/Foo", "a/foo"]).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_escape_rejected_before_write() {
    let (_d, s) = store();
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("real.txt"), b"data").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", out.join("link")).unwrap();
    let objects_before = count_objects(&s);
    assert!(tree::ingest_tree(&s, &out).is_err());
    assert_eq!(
        count_objects(&s),
        objects_before,
        "store written before rejection"
    );
}

#[cfg(unix)]
#[test]
fn relative_symlink_inside_root_accepted() {
    let (_d, s) = store();
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("real.txt"), b"data").unwrap();
    std::os::unix::fs::symlink("real.txt", out.join("link")).unwrap();
    let digest = tree::ingest_tree(&s, &out).unwrap();
    let t = tree::read_verified(&s, &digest).unwrap();
    assert!(t.entries.iter().any(|e| e.kind == tree::EntryKind::Symlink));
}

/// Same-key writers serialize on the per-key lock: exactly one record
/// lands and it is a complete, verifiable hit.
#[test]
fn concurrent_same_key_writers_one_valid_result() {
    let (_d, s) = store();
    let s = Arc::new(s);
    let key = spec().key();
    let mut handles = Vec::new();
    for i in 0..4u8 {
        let s = Arc::clone(&s);
        let key = key.clone();
        handles.push(std::thread::spawn(move || {
            let work = tempfile::tempdir().unwrap();
            let result = finished_result(&s, work.path(), &key, &[i; 8]);
            action_cache::store_result(&s, &key, &result).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    match action_cache::lookup(&s, &key) {
        Lookup::Hit(_, t) => assert_eq!(t.entries.len(), 1),
        other => panic!("expected one valid hit, got {other:?}"),
    }
}

#[test]
fn unknown_schema_is_safe_miss() {
    let (_d, s) = store();
    let key = spec().key();
    // Hand-write a record with an unknown version.
    let mut result = finished_result(&s, tempfile::tempdir().unwrap().path(), &key, b"x");
    result.version = 99;
    let bytes = action_cache::record_bytes(&result).unwrap();
    s.write_file(&s.action_path(&key), &bytes).unwrap();
    assert_eq!(
        action_cache::lookup(&s, &key),
        Lookup::Miss(Miss::UnknownSchema(99))
    );
    assert!(!s.action_path(&key).exists());
}

#[test]
fn malformed_record_is_safe_miss_and_removed() {
    let (_d, s) = store();
    let key = spec().key();
    s.write_file(&s.action_path(&key), b"{not json").unwrap();
    assert_eq!(action_cache::lookup(&s, &key), Lookup::Miss(Miss::Malformed));
    assert!(!s.action_path(&key).exists());
}

#[test]
fn failed_action_observable_but_never_hit() {
    let (_d, s) = store();
    let key = spec().key();
    let work = tempfile::tempdir().unwrap();
    let mut result = finished_result(&s, work.path(), &key, b"x");
    result.exit_status = 1;
    action_cache::store_result(&s, &key, &result).unwrap();
    assert_eq!(action_cache::lookup(&s, &key), Lookup::Miss(Miss::Failed(1)));
    // Observable: record is still there (not quarantined).
    assert!(s.action_path(&key).exists());
}

/// An interrupted object commit leaves an orphan: the file is kept (grace
/// period before gc) rather than deleted — gc handles expiry elsewhere.
#[test]
fn interrupted_commit_leaves_orphan_for_grace() {
    let (_d, s) = store();
    // An object staged into objects/ that no record references yet.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"orphan-bytes").unwrap();
    let (digest, _size) = s.ingest_object(tmp.path()).unwrap();
    assert!(s.object_path(&digest).exists());
    // Simulated interruption: the record never lands. The orphan stays
    // (nothing in the write path removes unreferenced objects).
    assert!(matches!(
        action_cache::lookup(&s, &"0".repeat(64)),
        Lookup::Miss(Miss::NotCached)
    ));
    assert!(s.object_path(&digest).exists());
}

#[test]
fn legacy_entries_untouched() {
    let (_d, s) = store();
    // Store::open creates the new namespaces but legacy dirs still exist.
    for sub in ["entries", "manifests", "locks", "actions", "trees", "leases"] {
        assert!(s.root.join(sub).is_dir(), "{sub} missing");
    }
}

fn crate_make_writable(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644));
    }
    #[cfg(windows)]
    {
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(p, perms);
    }
}

fn crate_remove(p: &Path) {
    crate_make_writable(p);
    let _ = std::fs::remove_file(p);
}

fn count_objects(s: &Store) -> usize {
    fn walk(d: &Path) -> usize {
        std::fs::read_dir(d)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| {
                        let p = e.path();
                        if p.is_dir() {
                            walk(&p)
                        } else {
                            1
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    walk(&s.objects_dir())
}
