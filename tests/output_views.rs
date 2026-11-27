//! Central output store + views: three-worktree disk acceptance, worktree
//! deletion, GC/lease protection, compiler-overwrite isolation, the
//! no-link backend, and crash-before/after-commit behavior.
//!
//! Store layout under test:
//! ```text
//! staging/<exec>/                    private per-execution dir
//! views/<project>/<view>/manifest.json  view bookkeeping
//! leases/<key>/<id>.lease            held while a run/view is live
//! worktree/.ns/view.json + paths.map shared manifest + local resolution
//! ```

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use naspeed::config::CowMode;
use naspeed::engine::action::{ActionInput, ActionSpec, ExpectedOutput, Platform, WORK_DIR_PLACEHOLDER};
use naspeed::engine::action_result::{ActionResult, Provenance, Resources};
use naspeed::engine::{output_view, staging};
use naspeed::gc::store_lru;
use naspeed::store::physical_usage;
use naspeed::store::{Store, action_cache, lease};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), CowMode::Auto).unwrap();
    (dir, s)
}

fn spec() -> ActionSpec {
    ActionSpec {
        executable: "fakecc".into(),
        toolchain_digest: blake3::hash(b"fake-toolchain").to_hex().to_string(),
        argv: vec!["fakecc".into(), "-o".into(), "bin/tool".into(), "src/tool.c".into()],
        env: BTreeMap::new(),
        platform: Platform::current(),
        work_dir: WORK_DIR_PLACEHOLDER.into(),
        inputs: vec![ActionInput {
            path: "src/tool.c".into(),
            digest: blake3::hash(b"int main(){}").to_hex().to_string(),
            size: 12,
        }],
        outputs: vec![
            ExpectedOutput { path: "bin/tool".into() },
            ExpectedOutput { path: "bin/tool.map".into() },
        ],
        cacheable: true,
    }
}

fn native() -> staging::NativeOutcome {
    staging::NativeOutcome {
        exit_status: 0,
        duration_ms: 42,
        stdout: b"compiled ok\n".to_vec(),
        stderr: Vec::new(),
        resources: Resources::default(),
        provenance: Provenance {
            producer: "local".into(),
            trust: "trusted".into(),
            origin: String::new(),
        },
    }
}

/// Run one fake compile through staging and publish it. Returns the key.
fn compile_and_commit(s: &Store, payload: &[u8]) -> (String, ActionResult) {
    let spec = spec();
    let st = staging::begin(s, &spec).unwrap();
    let bin = st.dir().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("tool"), payload).unwrap();
    std::fs::write(bin.join("tool.map"), b"map").unwrap();
    match staging::finish(st, &spec, native()).unwrap() {
        staging::Finish::Published(r) => (spec.key(), r),
        staging::Finish::Rejected { undeclared, .. } => {
            panic!("commit rejected: {undeclared:?}")
        }
    }
}

fn payload() -> Vec<u8> {
    // Large enough that manifest bytes are noise in the 50% check.
    let mut v = vec![b'x'; 256 * 1024];
    v.extend_from_slice(&[1, 2, 3, 4]);
    v
}

#[test]
fn three_worktrees_share_one_canonical_artifact() {
    let (_d, s) = store();
    let (key, result) = compile_and_commit(&s, &payload());
    let action_cache::Lookup::Hit(_, tree) = action_cache::lookup(&s, &key) else {
        panic!("expected hit");
    };
    let wts = tempfile::tempdir().unwrap();
    let mut views = Vec::new();
    for i in 0..3 {
        let wt = wts.path().join(format!("w{i}"));
        std::fs::create_dir_all(&wt).unwrap();
        let view = output_view::materialize(
            &s, &tree, &result.output_tree, &key, "proj", &wt,
        )
        .unwrap();
        views.push(view);
    }

    // Exactly one canonical copy of the payload in objects/.
    let canonical = physical_usage::tree_usage(&s.objects_dir());
    let su = physical_usage::store_usage(&s.root);
    assert_eq!(su.canonical, canonical.logical);
    // Payload object exists exactly once.
    let obj_count = walk(&s.objects_dir())
        .iter()
        .filter(|p| std::fs::metadata(p).map(|m| m.len() == payload().len() as u64).unwrap_or(false))
        .count();
    assert_eq!(obj_count, 1, "one canonical artifact");

    // Each worktree resolves all outputs to valid bytes.
    for v in &views {
        for f in &v.manifest().files {
            let p = v.resolve(&f.path).unwrap();
            assert_eq!(std::fs::metadata(&p).unwrap().len(), f.size);
        }
    }

    // Disk acceptance: three views' allocated bytes < half the 3x exposed.
    let vu = output_view::usage(&s);
    let exposed = vu.exposed;
    assert!(exposed >= 3 * payload().len() as u64);
    let allocated = vu.allocated + vu.manifests;
    assert!(
        allocated * 2 < exposed,
        "views allocated {allocated}B for {exposed}B exposed (>=50% reduction required)"
    );
    for v in views {
        v.release().release();
    }
}

#[test]
fn worktree_deletion_leaves_others_and_canonical_valid() {
    let (_d, s) = store();
    let (key, result) = compile_and_commit(&s, &payload());
    let action_cache::Lookup::Hit(_, tree) = action_cache::lookup(&s, &key) else {
        panic!("expected hit");
    };
    let wts = tempfile::tempdir().unwrap();
    let mut views = Vec::new();
    for i in 0..3 {
        let wt = wts.path().join(format!("w{i}"));
        std::fs::create_dir_all(&wt).unwrap();
        views.push(
            output_view::materialize(&s, &tree, &result.output_tree, &key, "proj", &wt)
                .unwrap(),
        );
    }
    let wt0 = wts.path().join("w0");
    output_view::drop_view(views.remove(0)).unwrap();
    // Deleted view's links and .ns are gone; canonical + other views intact.
    assert!(!wt0.join(".ns").exists());
    assert!(std::fs::symlink_metadata(wt0.join("bin/tool")).is_err());
    for v in &views {
        assert_eq!(
            std::fs::read(v.resolve("bin/tool").unwrap()).unwrap(),
            payload()
        );
    }
    let su = physical_usage::store_usage(&s.root);
    assert!(su.canonical >= payload().len() as u64);
    assert!(action_cache::lookup(&s, &key).is_hit());
    for v in views {
        v.release().release();
    }
}

#[test]
fn gc_honors_active_lease_and_dead_lease_frees() {
    let (_d, s) = store();
    let (key, result) = compile_and_commit(&s, &payload());
    let tree = std::fs::read_to_string(s.tree_path(&result.output_tree)).unwrap();
    let tree: naspeed::store::tree::Tree = serde_json::from_str(&tree).unwrap();
    let obj: Vec<String> = tree.entries.iter().map(|e| e.digest.clone()).collect();

    // A live lease on the key protects record, tree and objects even with
    // zero grace and zero age.
    let held = lease::acquire(&s, &key, lease::Purpose::View, &obj).unwrap();
    // Remove the record to prove the lease itself (guards) pins objects.
    std::fs::remove_file(s.action_path(&key)).unwrap();
    let r = store_lru::run(&s, 0, Some(Duration::ZERO), false, Duration::ZERO).unwrap();
    assert_eq!(r.actions_removed, 0);
    for d in &obj {
        assert!(s.object_path(d).exists(), "leased object {d} swept");
    }
    held.release();
    // Dead lease file: nothing protects objects now.
    let r = store_lru::run(&s, 0, Some(Duration::ZERO), false, Duration::ZERO).unwrap();
    for d in &obj {
        assert!(!s.object_path(d).exists(), "unprotected object {d} kept");
    }
    let _ = r;
}

#[test]
fn compiler_overwrite_goes_to_staging_only_cas_unchanged() {
    let (_d, s) = store();
    let (key, _result) = compile_and_commit(&s, &payload());
    let action_cache::Lookup::Hit(_, tree) = action_cache::lookup(&s, &key) else {
        panic!("expected hit");
    };
    let digest_before = tree
        .entries
        .iter()
        .find(|e| e.path == "bin/tool")
        .unwrap()
        .digest
        .clone();
    let obj_path = s.object_path(&digest_before);
    let mode = crate_mode(&obj_path);

    // A second run of the same action writes to a fresh staging dir.
    let spec = spec();
    let st = staging::begin(&s, &spec).unwrap();
    std::fs::create_dir_all(st.dir().join("bin")).unwrap();
    std::fs::write(st.dir().join("bin/tool"), b"corrupted-by-buggy-compiler").unwrap();
    std::fs::write(st.dir().join("bin/tool.map"), b"map2").unwrap();
    // Compiler tried to write where the canonical artifact would be —
    // staging means it only touched its private dir.
    let staging_bytes = std::fs::read(st.dir().join("bin/tool")).unwrap();
    match staging::finish(st, &spec, native()).unwrap() {
        staging::Finish::Published(_) => {}
        staging::Finish::Rejected { .. } => panic!("unexpected reject"),
    }
    assert_eq!(staging_bytes, b"corrupted-by-buggy-compiler");
    // Canonical bytes and digest untouched; object still read-only.
    let action_cache::Lookup::Hit(_, tree2) = action_cache::lookup(&s, &key) else {
        panic!("expected hit");
    };
    assert_eq!(
        tree2.entries.iter().find(|e| e.path == "bin/tool").unwrap().digest,
        digest_before
    );
    assert_eq!(std::fs::read(&obj_path).unwrap(), payload());
    assert_eq!(crate_mode(&obj_path), mode);
    assert!(!writable(&obj_path), "CAS object must not be writable");
}

#[test]
fn no_link_backend_resolves_canonical_paths_without_copy() {
    let (_d, s) = store();
    let (key, result) = compile_and_commit(&s, &payload());
    let action_cache::Lookup::Hit(_, tree) = action_cache::lookup(&s, &key) else {
        panic!("expected hit");
    };
    let wt = _d.path().join("wt-nolink");
    std::fs::create_dir_all(&wt).unwrap();

    // Build the manifest exactly as the direct-path backend records it.
    let mut manifest = output_view::ViewManifest {
        version: output_view::VIEW_VERSION,
        project: output_view::project_id("proj"),
        view: output_view::view_id(&output_view::project_id("proj"), &wt),
        backend: output_view::Backend::DirectPath,
        action_key: key.clone(),
        output_tree: result.output_tree.clone(),
        files: tree
            .entries
            .iter()
            .map(|e| output_view::ViewFile {
                path: e.path.clone(),
                object: e.digest.clone(),
                size: e.size,
                mode: e.mode,
            })
            .collect(),
    };
    // Write the shared manifest (no absolute paths) + local paths.map.
    let ns = wt.join(output_view::NS_DIR);
    std::fs::create_dir_all(&ns).unwrap();
    std::fs::write(ns.join("view.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
    let mut map = String::new();
    for f in &manifest.files {
        map.push_str(&format!("{}\t{}\n", f.path, s.object_path(&f.object).display()));
    }
    std::fs::write(ns.join(output_view::PATHS_MAP), &map).unwrap();

    // Resolver: direct canonical path, bytes identical, and — on unix —
    // the canonical inode itself, not a copy.
    manifest.backend = output_view::Backend::DirectPath;
    let view = output_view::resume(&s, &wt).unwrap().unwrap();
    let resolved = view.resolve("bin/tool").unwrap();
    assert_eq!(resolved, s.object_path(&tree.entries[0].digest));
    assert_eq!(std::fs::read(&resolved).unwrap(), payload());
    // No artifact copy in the worktree: only .ns metadata exists.
    let mut files = Vec::new();
    walk(&wt).iter().for_each(|p| files.push(p.clone()));
    for p in &files {
        assert!(p.starts_with(&ns), "artifact copy materialized: {}", p.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&resolved).unwrap().ino(),
            std::fs::metadata(s.object_path(&tree.entries[0].digest)).unwrap().ino()
        );
    }
    view.release().release();
    // Shared manifest contains no absolute paths.
    let shared: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(ns.join("view.json")).unwrap()).unwrap();
    let text = shared.to_string();
    assert!(!text.contains(_d.path().to_str().unwrap()), "absolute path in shared metadata");
}

#[test]
fn crash_before_and_after_commit_no_partial_result() {
    let (_d, s) = store();
    let spec = spec();

    // Crash before commit: staging dir + lease corpse left behind.
    let key = spec.key();
    let corpse_dir = s.staging_dir().join("777.123");
    std::fs::create_dir_all(&corpse_dir).unwrap();
    std::fs::write(corpse_dir.join("bin"), b"half-written").unwrap();
    let ldir = s.lease_dir(&key);
    std::fs::create_dir_all(&ldir).unwrap();
    std::fs::write(ldir.join("777.123.lease"), b"{}").unwrap();
    assert!(matches!(
        action_cache::lookup(&s, &key),
        action_cache::Lookup::Miss(action_cache::Miss::NotCached)
    ));
    // Startup recovery: lease dead (young only within grace), dir abandoned.
    let r = store_lru::run(&s, u64::MAX, None, false, Duration::ZERO).unwrap();
    assert!(r.staging_removed >= 1, "abandoned staging not swept: {r:?}");
    assert!(r.leases_removed >= 1, "dead lease not reaped: {r:?}");
    assert!(!corpse_dir.exists());
    assert!(matches!(
        action_cache::lookup(&s, &key),
        action_cache::Lookup::Miss(action_cache::Miss::NotCached)
    ));

    // Crash after commit: complete record + objects survive, no partial.
    let (key2, result) = compile_and_commit(&s, &payload());
    let r = store_lru::run(&s, u64::MAX, None, false, Duration::ZERO).unwrap();
    assert_eq!(r.objects_removed, 0, "live objects swept: {r:?}");
    match action_cache::lookup(&s, &key2) {
        action_cache::Lookup::Hit(r2, _) => {
            assert_eq!(r2.output_tree, result.output_tree);
        }
        _ => panic!("committed result not visible"),
    }
}

#[test]
fn undeclared_output_rejected_native_preserved() {
    let (_d, s) = store();
    let spec = spec();
    let st = staging::begin(&s, &spec).unwrap();
    std::fs::create_dir_all(st.dir().join("bin")).unwrap();
    std::fs::write(st.dir().join("bin/tool"), payload()).unwrap();
    std::fs::write(st.dir().join("bin/tool.map"), b"map").unwrap();
    std::fs::write(st.dir().join("sneaky.tmp"), b"undeclared write").unwrap();
    match staging::finish(st, &spec, native()).unwrap() {
        staging::Finish::Rejected { native: n, undeclared } => {
            assert_eq!(n.exit_status, 0);
            assert!(undeclared.contains(&"sneaky.tmp".into()));
        }
        staging::Finish::Published(_) => panic!("undeclared output was published"),
    }
    // Nothing was ingested or published.
    assert!(matches!(
        action_cache::lookup(&s, &spec.key()),
        action_cache::Lookup::Miss(action_cache::Miss::NotCached)
    ));
    assert_eq!(physical_usage::tree_usage(&s.objects_dir()).files, 0);
}

#[test]
fn status_reports_logical_canonical_view_physical_split() {
    let (_d, s) = store();
    let (key, result) = compile_and_commit(&s, &payload());
    let action_cache::Lookup::Hit(_, tree) = action_cache::lookup(&s, &key) else {
        panic!("expected hit");
    };
    let wtd = tempfile::tempdir().unwrap();
    let wt = wtd.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let view = output_view::materialize(&s, &tree, &result.output_tree, &key, "proj", &wt)
        .unwrap();

    let su = physical_usage::store_usage(&s.root);
    let vu = output_view::usage(&s);
    assert!(su.logical >= su.canonical);
    assert!(su.canonical >= payload().len() as u64);
    assert!(su.physical >= su.canonical);
    assert!(vu.exposed >= payload().len() as u64);
    assert_eq!(vu.count, 1);
    // Byte-split line present in `naspeed status`.
    let home = wtd.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(common::NASPEED)
        .env("NO_COLOR", "1")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("NASPEED_CONFIG_DIR", home.join("cfg"))
        .env("NASPEED_STORE", &s.root)
        .arg("status")
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "status failed: {text}");
    for needle in ["logical=", "canonical=", "physical=", "view_bytes=", "staging="] {
        assert!(text.contains(needle), "missing {needle} in status:\n{text}");
    }
    view.release().release();
}

#[test]
fn undeclared_output_rejected_on_failed_run() {
    let (_d, s) = store();
    let spec = spec();
    let st = staging::begin(&s, &spec).unwrap();
    std::fs::create_dir_all(st.dir().join("bin")).unwrap();
    std::fs::write(st.dir().join("bin/tool"), payload()).unwrap();
    // Missing tool.map + failed run: still rejected? No — failed runs
    // publish what they produced; only extras reject. Add an extra too.
    std::fs::write(st.dir().join("extra.o"), b"x").unwrap();
    let mut n = native();
    n.exit_status = 1;
    match staging::finish(st, &spec, n).unwrap() {
        staging::Finish::Rejected { .. } => {}
        staging::Finish::Published(_) => panic!("extra output on failed run published"),
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => out.extend(walk(&p)),
            _ => out.push(p),
        }
    }
    out
}

fn crate_mode(p: &Path) -> u32 {
    naspeed::platform::mode_of(&std::fs::metadata(p).unwrap())
}

#[cfg(unix)]
fn writable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o222 != 0
}
#[cfg(not(unix))]
fn writable(p: &Path) -> bool {
    !std::fs::metadata(p).unwrap().permissions().readonly()
}
