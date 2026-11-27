//! Phase-4 graph service integration: lifecycle, fallback, reload
//! semantics, and no-op plan timing on both benchmark repositories.
//!
//! `src/engine/mod.rs` is owned by a concurrent phase, so the engine
//! modules are pulled in with `#[path]` (they are written to compile both
//! as crate modules and as test-root modules). `dev::go_dev` and friends
//! are private to the dev command, so they are included the same way;
//! `project::` is shimmed onto the real public modules. The inner
//! `#[cfg(test)]` suites of the included files (protocol golden vectors,
//! digest-memo races) run as part of this target too.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod project {
    pub use naspeed::project::NsConfig;
    pub mod workspace_graph {
        pub use naspeed::project::workspace_graph::*;
    }
}

mod dev;

#[path = "../src/engine/graph.rs"]
mod graph;
#[path = "../src/engine/digest_memo.rs"]
mod digest_memo;
#[path = "../src/engine/service.rs"]
mod service;
#[path = "../src/engine/protocol.rs"]
mod protocol;

use digest_memo::DigestMemo;
use graph::{ActionId, ActionKind, EngineGraph, Plan};
use protocol::{ErrorCode, Message};
use service::{Client, DEFAULT_IDLE, in_process_plan, plan_or_fallback, serve, try_acquire};

const NO_OP_BUDGET: Duration = Duration::from_millis(500);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn have_go() -> bool {
    std::process::Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn go_bench_repo() -> Option<PathBuf> {
    let path = std::env::var_os("NASPEED_GO_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/evan1/Documents/Code/seller-ai/sale-agent-go"));
    (path.join("go.mod").is_file() && have_go()).then_some(path)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), dst).unwrap();
        }
    }
}

fn load(root: &Path, store: &Path) -> (EngineGraph, DigestMemo) {
    let graph = EngineGraph::load(root, store).expect("load graph");
    let mut memo = DigestMemo::new();
    graph.prime(&mut memo);
    (graph, memo)
}

fn names(actions: &BTreeSet<ActionId>) -> BTreeSet<String> {
    actions.iter().map(ToString::to_string).collect()
}

fn expect_actions(actions: &BTreeSet<ActionId>, want: &[&str]) {
    let got = names(actions);
    let want: BTreeSet<String> = want.iter().map(|s| s.to_string()).collect();
    assert_eq!(got, want);
}

// ---------------------------------------------------------------- no-op

#[test]
fn noop_plan_is_fast_and_invalidates_nothing_rust() {
    let store = tempfile::tempdir().unwrap();
    let (graph, mut memo) = load(&fixture("ns_project"), store.path());
    let start = Instant::now();
    let plan = graph.plan(&[], &mut memo);
    let elapsed = start.elapsed();
    eprintln!("[timing] ns_project noop plan: {elapsed:?}");
    assert!(plan.is_noop(), "unexpected invalidations: {}", names(&plan.actions).into_iter().collect::<Vec<_>>().join(","));
    assert!(elapsed <= NO_OP_BUDGET, "no-op plan took {elapsed:?}");
}

#[test]
fn noop_plan_is_fast_and_invalidates_nothing_go() {
    if !have_go() {
        return;
    }
    let store = tempfile::tempdir().unwrap();
    let (graph, mut memo) = load(&fixture("ns_go_work"), store.path());
    let start = Instant::now();
    let plan = graph.plan(&[], &mut memo);
    let elapsed = start.elapsed();
    eprintln!("[timing] ns_go_work noop plan: {elapsed:?}");
    assert!(plan.is_noop());
    assert!(elapsed <= NO_OP_BUDGET, "no-op plan took {elapsed:?}");
}

/// The acceptance number: no-op plan on both benchmark repositories,
/// in-process and via the local service socket.
#[test]
fn noop_plan_timing_both_repositories() {
    struct Row {
        repo: String,
        language: String,
        load_ms: u128,
        in_process_plan_ms: u128,
        service_plan_ms: Option<u128>,
    }
    let mut rows = Vec::new();
    let naspeed_repo = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    for repo in [naspeed_repo, fixture("ns_project")]
        .into_iter()
        .chain(go_bench_repo())
    {
        let store = tempfile::tempdir().unwrap();
        let load_start = Instant::now();
        let (graph, mut memo) = load(&repo, store.path());
        let load_ms = load_start.elapsed().as_millis();
        let plan_start = Instant::now();
        let plan = graph.plan(&[], &mut memo);
        let in_ms = plan_start.elapsed().as_millis();
        assert!(plan.is_noop());
        assert!(
            plan_start.elapsed() <= NO_OP_BUDGET,
            "{} no-op plan took {}ms",
            repo.display(),
            in_ms
        );
        rows.push(Row {
            repo: repo.display().to_string(),
            language: format!("{:?}", graph.language),
            load_ms,
            in_process_plan_ms: in_ms,
            service_plan_ms: None,
        });
    }

    // Service-mode plan over the socket on the smallest repo.
    let store = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let store_path = store.path().to_path_buf();
    let handle = std::thread::spawn({
        let store_path = store_path.clone();
        move || serve(&store_path, Duration::from_secs(30), &AtomicBool::new(false))
    });
    wait_for_socket(store.path());
    let repo = fixture("ns_project");
    let mut client = Client::connect(store.path()).unwrap();
    client.plan_for(&repo, &[]).unwrap(); // warm load
    let start = Instant::now();
    let plan = client.plan_for(&repo, &[]).unwrap();
    let svc_ms = start.elapsed().as_millis();
    assert!(plan.is_noop());
    assert!(start.elapsed() <= NO_OP_BUDGET);
    client.shutdown().unwrap();
    let _ = handle.join();
    let _ = stop.store(false, Ordering::Relaxed);

    rows.push(Row {
        repo: repo.display().to_string(),
        language: "Rust (service)".to_string(),
        load_ms: 0,
        in_process_plan_ms: 0,
        service_plan_ms: Some(svc_ms),
    });

    let mut artifact = String::from("{\n  \"scenarios\": [\n");
    for (i, row) in rows.iter().enumerate() {
        eprintln!(
            "[timing] {} ({}): load={}ms in-process-plan={}ms service-plan={:?}ms",
            row.repo, row.language, row.load_ms, row.in_process_plan_ms, row.service_plan_ms
        );
        artifact.push_str(&format!(
            "    {{\"repo\": {:?}, \"language\": {:?}, \"load_ms\": {}, \"in_process_plan_ms\": {}, \"service_plan_ms\": {}}}{}\n",
            row.repo,
            row.language,
            row.load_ms,
            row.in_process_plan_ms,
            row.service_plan_ms.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            if i + 1 == rows.len() { "" } else { "," }
        ));
    }
    artifact.push_str("  ]\n}\n");
    let out = std::env::var_os("NASPEED_TIMING_ARTIFACT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/naspeed-graph-timing.json"));
    std::fs::write(&out, &artifact).unwrap();
    eprintln!("[timing] wrote {}", out.display());
}

fn wait_for_socket(store: &Path) {
    let sock = service::service_dir(store).join("graph.sock");
    for _ in 0..200 {
        if sock.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("service socket never appeared at {}", sock.display());
}

// ------------------------------------------------------------- affected

#[test]
fn leaf_and_core_edits_invalidate_exact_sets() {
    let store = tempfile::tempdir().unwrap();
    let root = fixture("ns_project");
    let (graph, mut memo) = load(&root, store.path());

    // Leaf: the binary's own crate — nothing else depends on it.
    let plan = graph.plan(&[root.join("apps/hello-server/src/main.rs")], &mut memo);
    assert!(!plan.reload);
    expect_actions(
        &plan.actions,
        &[
            "P:hello-server",
            "C:hello-server",
            "T:hello-server",
            "L:hello-server",
            "B:hello-server",
        ],
    );

    // Core: the shared lib — the dependent binary recompiles, relinks,
    // retests; the lib itself does not produce a binary.
    let plan = graph.plan(&[root.join("crates/greeter/src/lib.rs")], &mut memo);
    assert!(!plan.reload);
    expect_actions(
        &plan.actions,
        &[
            "P:greeter",
            "C:greeter",
            "T:greeter",
            "P:hello-server",
            "C:hello-server",
            "T:hello-server",
            "L:hello-server",
            "B:hello-server",
        ],
    );
}

#[test]
fn manifest_change_forces_full_reload() {
    let store = tempfile::tempdir().unwrap();
    let root = fixture("ns_project");
    let (graph, mut memo) = load(&root, store.path());
    let plan = graph.plan(&[root.join("Cargo.toml")], &mut memo);
    assert!(plan.reload);
    assert_eq!(plan.actions, graph.all_actions());
}

#[test]
fn rename_delete_new_package_converge_to_reload() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ws");
    copy_tree(&fixture("ns_project"), &root);
    let store = tempfile::tempdir().unwrap();
    let (graph, mut memo) = load(&root, store.path());

    // New package forming: unowned Cargo.toml/.rs under the root → reload.
    let newpkg = root.join("crates/newpkg");
    std::fs::create_dir_all(newpkg.join("src")).unwrap();
    std::fs::write(newpkg.join("Cargo.toml"), "[package]\nname = \"newpkg\"\n").unwrap();
    let plan = graph.plan(&[newpkg.join("Cargo.toml")], &mut memo);
    assert!(plan.reload, "new manifest must reload");
    // Sources inside the new dir are foreign-module content (its
    // Cargo.toml isn't a known member yet) → ignored until the reload
    // the manifest itself already triggered.
    let plan = graph.plan(&[newpkg.join("src/lib.rs")], &mut memo);
    assert!(!plan.reload, "foreign-module source is ignored");
    // An unowned .rs outside any package dir may be a new package → reload.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/newfile.rs"), "pub fn n() {}\n").unwrap();
    let plan = graph.plan(&[root.join("src/newfile.rs")], &mut memo);
    assert!(plan.reload, "unowned .rs must reload");

    // Deleted manifest → rescan catches it → reload.
    std::fs::remove_file(root.join("crates/greeter/Cargo.toml")).unwrap();
    let plan = graph.plan(&[], &mut memo);
    assert!(plan.reload, "deleted member manifest must reload");
}

#[test]
fn go_semantics_survive_normalization() {
    if !have_go() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ws");
    copy_tree(&fixture("ns_go_work"), &root);
    let store = tempfile::tempdir().unwrap();
    let (graph, mut memo) = load(&root, store.path());

    // Compile change on greet: api (dependent) recompiles+relinks;
    // the unrelated tool package is untouched.
    let plan = graph.plan(&[root.join("svc/internal/greet/greet.go")], &mut memo);
    assert!(!plan.reload);
    let got = names(&plan.actions);
    assert!(got.contains("C:example.test/svc/internal/greet"));
    assert!(got.contains("C:example.test/svc/cmd/api"));
    assert!(got.contains("L:api") && got.contains("B:api"));
    assert!(!got.iter().any(|a| a.contains("tools")), "{got:?}");

    // Test-only dep change: testutil is only imported by tests, so only
    // test actions light up — no compile/link outside testutil itself.
    let plan = graph.plan(&[root.join("svc/internal/testutil/testutil.go")], &mut memo);
    let got = names(&plan.actions);
    assert!(got.contains("C:example.test/svc/internal/testutil"));
    assert!(got.contains("T:example.test/svc/internal/greet"));
    assert!(got.contains("T:example.test/svc/cmd/api"));
    assert!(!got.contains("C:example.test/svc/internal/greet"), "{got:?}");
    assert!(!got.contains("L:api"), "{got:?}");

    // A test file change invalidates the test action, not the compile.
    let plan = graph.plan(&[root.join("svc/internal/greet/greet_test.go")], &mut memo);
    let got = names(&plan.actions);
    // A test-input change invalidates tests up the test-dep closure
    // (api's tests link greet's tests) but no compile actions.
    assert!(got.contains("T:example.test/svc/internal/greet"));
    assert!(got.contains("T:example.test/svc/cmd/api"));
    assert!(!got.iter().any(|a| a.starts_with('C')), "{got:?}");

    // Unlisted embed input is a compile input.
    let plan = graph.plan(&[root.join("svc/internal/greet/greeting.txt")], &mut memo);
    assert!(names(&plan.actions).contains("C:example.test/svc/internal/greet"));
}

// ------------------------------------------------------------- fallback

#[test]
fn fallback_when_service_absent_matches_in_process() {
    let store = tempfile::tempdir().unwrap();
    let root = fixture("ns_project");
    let changes = vec![root.join("apps/hello-server/src/main.rs")];
    let via_fallback = plan_or_fallback(&root, store.path(), &changes).unwrap();
    let in_process = in_process_plan(&root, store.path(), &changes).unwrap();
    assert_eq!(via_fallback, in_process);
}

#[test]
fn service_plan_matches_in_process_plan() {
    let store = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let handle = std::thread::spawn({
        let store = store.path().to_path_buf();
        move || serve(&store, Duration::from_secs(30), &AtomicBool::new(false))
    });
    wait_for_socket(store.path());
    let root = fixture("ns_project");
    let changes = vec![root.join("crates/greeter/src/lib.rs")];
    let mut client = Client::connect(store.path()).unwrap();
    let via_service = client.plan_for(&root, &changes).unwrap();
    let in_process = in_process_plan(&root, store.path(), &changes).unwrap();
    assert_eq!(via_service, in_process);
    assert!(!via_service.is_noop());
    client.shutdown().unwrap();
    let _ = handle.join();
    let _ = stop;
}

/// A dead owner leaves its socket file behind: connecting fails fast and
/// the plan still comes back, identical, from the in-process path.
#[test]
fn crashed_service_falls_back_with_same_plan() {
    let store = tempfile::tempdir().unwrap();
    let dir = service::service_dir(store.path());
    std::fs::create_dir_all(&dir).unwrap();
    {
        // Live owner, then "crash": drop the listener without cleanup.
        let _listener = std::os::unix::net::UnixListener::bind(dir.join("graph.sock")).unwrap();
    }
    let root = fixture("ns_project");
    let changes = vec![root.join("crates/greeter/src/lib.rs")];
    let plan = plan_or_fallback(&root, store.path(), &changes).unwrap();
    assert_eq!(plan, in_process_plan(&root, store.path(), &changes).unwrap());
}

/// A service speaking a different protocol version gets an Error on the
/// client's Hello and the client silently plans in-process instead.
#[test]
fn version_mismatched_service_falls_back_with_same_plan() {
    let store = tempfile::tempdir().unwrap();
    let dir = service::service_dir(store.path());
    std::fs::create_dir_all(&dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(dir.join("graph.sock")).unwrap();
    let fake = std::thread::spawn(move || {
        // Answer every connection with a foreign HelloReply.
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 64];
            if stream.read(&mut buf).is_ok() {
                let reply = Message::HelloReply {
                    version: protocol::PROTOCOL_VERSION + 1,
                    flags: 0,
                };
                let _ = stream.write_all(&protocol::encode_frame(&reply));
            }
        }
    });
    let root = fixture("ns_project");
    let changes = vec![root.join("crates/greeter/src/lib.rs")];
    assert!(Client::connect(store.path()).is_err(), "wrong-version service must be refused");
    let plan = plan_or_fallback(&root, store.path(), &changes).unwrap();
    assert_eq!(plan, in_process_plan(&root, store.path(), &changes).unwrap());
    drop(fake);
}

// ------------------------------------------------------------ lifecycle

#[test]
fn one_owner_only_and_deterministic_plans() {
    let store = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let handle = std::thread::spawn({
        let store = store.path().to_path_buf();
        move || serve(&store, Duration::from_secs(30), &AtomicBool::new(false))
    });
    wait_for_socket(store.path());

    // A second owner cannot take the lock while the service runs.
    assert!(try_acquire(store.path()).unwrap().is_none());
    // A second serve() in this process fails the same way.
    assert!(serve(store.path(), Duration::from_millis(1), &stop).is_err());

    let root = fixture("ns_project");
    let changes = vec![root.join("apps/hello-server/src/main.rs")];
    let mut a = Client::connect(store.path()).unwrap();
    let mut b = Client::connect(store.path()).unwrap();
    let pa = a.plan_for(&root, &changes).unwrap();
    let pb = b.plan_for(&root, &changes).unwrap();
    assert_eq!(pa, pb);
    a.shutdown().unwrap();
    let _ = handle.join();
}

#[test]
fn idle_exit_releases_lock_and_socket() {
    let store = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let sock = service::service_dir(store.path()).join("graph.sock");
    let handle = std::thread::spawn({
        let store = store.path().to_path_buf();
        move || serve(&store, Duration::from_millis(300), &AtomicBool::new(false))
    });
    wait_for_socket(store.path());
    handle.join().unwrap().unwrap();
    // Stale-lock recovery: the owner is gone, the socket is gone, and a
    // fresh owner can take over immediately.
    assert!(!sock.exists(), "socket not cleaned up on idle exit");
    let owner = try_acquire(store.path()).unwrap();
    assert!(owner.is_some(), "stale lock after idle exit");
    drop(owner);
    let _ = stop;
}

#[test]
fn paths_outside_roots_are_rejected() {
    let store = tempfile::tempdir().unwrap();
    let handle = std::thread::spawn({
        let store = store.path().to_path_buf();
        move || serve(&store, Duration::from_secs(30), &AtomicBool::new(false))
    });
    wait_for_socket(store.path());
    let root = fixture("ns_project");
    let mut client = Client::connect(store.path()).unwrap();
    client.plan_for(&root, &[]).unwrap();
    // Sneak a /etc path into the journal: the service answers Error and
    // hangs up that connection.
    let err = client
        .plan_for(&root, &[PathBuf::from("/etc/passwd")])
        .unwrap_err();
    eprintln!("outside-root rejection: {err}");
    client.shutdown().ok();
    let _ = handle.join();
}

#[test]
fn rapid_same_size_edit_rehashes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.rs");
    std::fs::write(&path, "pub fn a() -> u8 { 1 } //").unwrap();
    let mut memo = DigestMemo::new();
    let first = memo.digest(&path).unwrap();
    std::fs::write(&path, "pub fn a() -> u8 { 2 } //").unwrap(); // same size
    let second = memo.digest(&path).unwrap();
    assert_ne!(first, second);
}

// ------------------------------------------------------------- watcher

#[test]
fn watcher_journal_records_normalized_batches() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(root.join("target-junk"), "x").unwrap();
    let (bus, _rx) = dev::log_bus::LogBus::channel();
    let filter = dev::file_watcher::WatchFilter::new(
        &root,
        &[PathBuf::from("src")],
        &[],
        &[],
    )
    .unwrap();
    let watcher = dev::file_watcher::FileWatcher::start(&root, &[PathBuf::from("src")], filter, bus)
        .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() { changed() }\n").unwrap();
    let batch = watcher.next_batch(Duration::from_secs(10)).unwrap();
    assert!(batch.iter().any(|p| p.ends_with("src/lib.rs")), "{batch:?}");
    let journal = watcher.drain_journal();
    assert_eq!(journal.len(), 1);
    assert!(journal[0].paths.contains(&PathBuf::from("src/lib.rs")));
    assert!(watcher.drain_journal().is_empty());
}




