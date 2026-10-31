//! Peer cache end to end: one store served over HTTP (as on a tailnet)
//! fills an empty store on a fresh checkout without compiling, an
//! unreachable peer only costs a skipped lookup, and `cache push` uploads to
//! a token-protected peer.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

mod common;

use common::{NASPEED, Sandbox};

/// `naspeed cache serve` on a loopback port; killed on drop.
struct Peer {
    child: Child,
    url: String,
}

impl Peer {
    fn start(sb: &Sandbox, store: &Path, extra: &[&str], token: Option<&str>) -> Peer {
        let mut cmd = Command::new(NASPEED);
        cmd.args(["cache", "serve", "--listen", "127.0.0.1:0"])
            .args(extra)
            .env("NASPEED_CONFIG_DIR", &sb.config)
            .env("NASPEED_STORE", store)
            .env_remove("NASPEED_REMOTE_TOKEN")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(t) = token {
            cmd.env("NASPEED_REMOTE_TOKEN", t);
        }
        let mut child = cmd.spawn().unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        // Keep draining: a closed pipe would fail the server's later prints.
        std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
        let url = line
            .split_whitespace()
            .find(|w| w.starts_with("http://"))
            .unwrap_or_else(|| panic!("no URL in {line:?}"))
            .to_owned();
        Peer { child, url }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn count(store: &Path, prefix: &str) -> usize {
    std::fs::read_to_string(store.join("usage.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with(prefix))
        .count()
}

fn entries(store: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![store.join("entries")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn served_store_fills_a_fresh_machine() {
    let sb = Sandbox::new("peer");
    sb.run("t1", &["build"]);
    let stored = entries(&sb.store);
    assert!(stored >= 10, "cold build stored {stored} entries");
    let peer = Peer::start(&sb, &sb.store, &[], None);

    // "Another machine": empty store, checkout at another path.
    let store2 = sb.root.join("store-two");
    std::fs::create_dir_all(&store2).unwrap();
    let ws2 = sb.root.join("other-checkout");
    common::copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_workspace"),
        &ws2,
    );
    let out = sb
        .cargo("t2", &["build"])
        .current_dir(&ws2)
        .env("NASPEED_STORE", &store2)
        .env("NASPEED_REMOTES", &peer.url)
        .env("NASPEED_LOG", "naspeed::remote=warn")
        .env("NASPEED_LOG_FILE", sb.root.join("wrapper.log"))
        .output()
        .unwrap();
    let wrapper_log = std::fs::read_to_string(sb.root.join("wrapper.log")).unwrap_or_default();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (hits, misses, fetched) = (
        count(&store2, "H\t"),
        count(&store2, "M\t"),
        count(&store2, "RH\t"),
    );
    eprintln!("from peer: {hits} hits ({fetched} fetched), {misses} misses");
    assert!(
        hits as f64 / (hits + misses) as f64 >= 0.95,
        "{hits} hits, {misses} misses\n{wrapper_log}"
    );
    assert!(
        fetched >= hits * 9 / 10,
        "{fetched} fetched for {hits} hits"
    );
    let run = Command::new(sb.root.join("t2/debug/sample-app"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        r#"{"text":"hello","count":3} 42 lz4=true"#
    );

    // Hits on fetched entries carry the original compile time.
    let stats = Command::new(NASPEED)
        .args(["stats", "--json"])
        .env("NASPEED_CONFIG_DIR", &sb.config)
        .env("NASPEED_STORE", &store2)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert!(json[0]["saved_ms"].as_u64().unwrap() > 0, "{json}");
    assert_eq!(json[0]["remote_hits"].as_u64().unwrap() as usize, fetched);
}

#[test]
fn unreachable_peer_is_skipped() {
    let sb = Sandbox::new("peerdown");
    // Nothing listens on port 9 (discard) on loopback.
    let started = std::time::Instant::now();
    let out = sb
        .cargo("t1", &["build"])
        .env("NASPEED_REMOTES", "http://127.0.0.1:9")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(count(&sb.store, "M\t") >= 10);
    assert_eq!(count(&sb.store, "RH\t"), 0);
    let marker = std::fs::read_dir(sb.store.join("tmp"))
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("peer-down."));
    assert!(marker, "a refused connection marks the peer down");
    eprintln!("build with dead peer took {:?}", started.elapsed());
}

#[test]
fn push_needs_the_token_and_uploads_entries() {
    let sb = Sandbox::new("peerpush");
    sb.run("t1", &["build"]);
    let stored = entries(&sb.store);
    let vps = sb.root.join("vps-store");
    std::fs::create_dir_all(&vps).unwrap();
    let peer = Peer::start(&sb, &vps, &["--allow-push"], Some("s3cret"));

    let push = |token: &str| {
        Command::new(NASPEED)
            .args(["cache", "push", &peer.url])
            .env("NASPEED_CONFIG_DIR", &sb.config)
            .env("NASPEED_STORE", &sb.store)
            .env("NASPEED_REMOTE_TOKEN", token)
            .output()
            .unwrap()
    };
    let denied = push("wrong");
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("401"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert_eq!(entries(&vps), 0);

    let ok = push("s3cret");
    let text = String::from_utf8_lossy(&ok.stdout);
    assert!(
        ok.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(text.contains(&format!("pushed {stored} entries")), "{text}");
    assert_eq!(entries(&vps), stored);
    let again = push("s3cret");
    assert!(
        String::from_utf8_lossy(&again.stdout).contains(&format!("{stored} already there")),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
}

#[test]
fn read_only_peer_refuses_push() {
    let sb = Sandbox::new("peerro");
    sb.run("t1", &["build"]);
    let ro = sb.root.join("ro-store");
    std::fs::create_dir_all(&ro).unwrap();
    let peer = Peer::start(&sb, &ro, &[], None);
    let out = Command::new(NASPEED)
        .args(["cache", "push", &peer.url])
        .env("NASPEED_CONFIG_DIR", &sb.config)
        .env("NASPEED_STORE", &sb.store)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("403"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
