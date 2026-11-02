//! Multi-binary `ns dev` lifecycle against a real Cargo workspace and HTTP app.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{NASPEED, copy_dir};

const PORT: u16 = 18081;
const WAIT: Duration = Duration::from_secs(45);

fn http_body() -> Option<String> {
    let address = SocketAddr::from(([127, 0, 0, 1], PORT));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nhost: fixture\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
}

fn port_owner() -> String {
    let out = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{PORT}"), "-sTCP:LISTEN"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes the recorded child process identity.
    unsafe { libc::kill(pid, 0) == 0 }
}

struct DevSession {
    child: Child,
    log: Arc<Mutex<String>>,
    ws: PathBuf,
}

impl DevSession {
    fn start(ws: &Path, store: &Path, args: &[&str]) -> DevSession {
        let mut cmd = Command::new(NASPEED);
        cmd.env("NO_COLOR", "1");
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if (key.starts_with("CARGO_") && key != "CARGO_HOME")
                || key.starts_with("NASPEED_")
                || key.starts_with("RUSTC_")
                || key == "CARGO"
            {
                cmd.env_remove(key.as_ref());
            }
        }
        let mut child = cmd
            .arg("dev")
            .args(args)
            .current_dir(ws)
            .env("RUSTC_WRAPPER", NASPEED)
            .env("CARGO_TARGET_DIR", ws.join("target"))
            .env("NASPEED_STORE", store)
            .env("NASPEED_CONFIG_DIR", store.join("config"))
            .env("NASPEED_MIN_FREE_MEM_MB", "0")
            .env("NASPEED_NO_AUTO_GC", "1")
            .env("NS_DEV_TEST_BUILD_DELAY_MS", "900")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let log = Arc::new(Mutex::new(String::new()));
        for pipe in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let log = log.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    log.lock().unwrap().push_str(&(line + "\n"));
                }
            });
        }
        DevSession {
            child,
            log,
            ws: ws.to_path_buf(),
        }
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    fn wait_log(&self, needle: &str, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.log().matches(needle).count() < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count}x {needle:?}; log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_body(&self, needles: &[&str]) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(body) = http_body()
                && needles.iter().all(|needle| body.contains(needle))
            {
                return body;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for body containing {needles:?}; last {:?}; log:\n{}",
                http_body(),
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_worker(&self, needle: &str) -> u32 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(pid) = self.worker_pid()
                && self.log().contains(needle)
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for worker output {needle:?}; log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn api_pid(body: &str) -> u32 {
        body.split_whitespace().nth(2).unwrap().parse().unwrap()
    }

    fn worker_pid(&self) -> Option<u32> {
        self.log()
            .lines()
            .filter_map(|line| line.split_once("] worker pid "))
            .filter_map(|(_, text)| text.split_whitespace().next()?.parse().ok())
            .next_back()
    }

    fn replace(&self, relative: &str, from: &str, to: &str) {
        let path = self.ws.join(relative);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{} lacks {from:?}", path.display());
        std::fs::write(path, text.replace(from, to)).unwrap();
    }

    fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: SIGTERM is sent only to this test's direct child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(12);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            for pid in self.recorded_app_pids() {
                // SAFETY: these process-group IDs were emitted by this session's app slots.
                unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn recorded_app_pids(&self) -> Vec<u32> {
        self.log()
            .lines()
            .filter_map(|line| line.split_once(" (pid "))
            .filter_map(|(_, rest)| rest.split(')').next()?.parse().ok())
            .collect()
    }
}

impl Drop for DevSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn make_workspace(root: &Path) -> (PathBuf, PathBuf) {
    let ws = root.join("ws");
    let store = root.join("store");
    std::fs::create_dir_all(store.join("config")).unwrap();
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project"),
        &ws,
    );
    (ws, store)
}

#[test]
fn selected_bins_restart_by_dependency_and_environment_without_losing_old_apps() {
    assert!(
        http_body().is_none() && port_owner().is_empty(),
        "port {PORT} is busy; owner:\n{}",
        port_owner()
    );
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dev-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (ws, store) = make_workspace(&root);
    let mut dev = DevSession::start(
        &ws,
        &store,
        &["--no-tui", "--bin", "api", "--bin", "worker"],
    );
    let first = dev.wait_body(&["api-v1", "shared-v1", "api-only-v1"]);
    let first_api = DevSession::api_pid(&first);
    let first_worker = dev.wait_worker("worker-v1 shared-v1");

    dev.replace("crates/api-only/src/lib.rs", "api-only-v1", "api-only-v2");
    let api_only = dev.wait_body(&["api-only-v2"]);
    let api_only_pid = DevSession::api_pid(&api_only);
    assert_ne!(api_only_pid, first_api);
    assert_eq!(dev.wait_worker("worker-v1 shared-v1"), first_worker);

    dev.replace("crates/api-only/src/lib.rs", "api-only-v2", "api-only-v3");
    dev.wait_log("crates/api-only/src/lib.rs", 2, WAIT);
    std::thread::sleep(Duration::from_millis(120));
    dev.replace("crates/shared/src/lib.rs", "shared-v1", "shared-v2");
    dev.wait_log("files changed during the build; running again", 1, WAIT);
    let queued = dev.wait_body(&["api-only-v3", "shared-v2"]);
    let queued_api = DevSession::api_pid(&queued);
    let queued_worker = dev.wait_worker("worker-v1 shared-v2");
    assert_ne!(queued_api, api_only_pid);
    assert_ne!(queued_worker, first_worker);

    std::fs::write(ws.join("api.env"), "API_MARK=api-v2\n").unwrap();
    let api_env = dev.wait_body(&["api-v2", "api-only-v3", "shared-v2"]);
    let api_env_pid = DevSession::api_pid(&api_env);
    assert_ne!(api_env_pid, queued_api);
    assert_eq!(dev.wait_worker("worker-v1 shared-v2"), queued_worker);

    std::fs::write(ws.join("worker.env"), "WORKER_MARK=worker-v2\n").unwrap();
    let worker_env_pid = dev.wait_worker("worker-v2 shared-v2");
    assert_ne!(worker_env_pid, queued_worker);
    assert_eq!(
        DevSession::api_pid(&dev.wait_body(&["api-v2", "api-only-v3"])),
        api_env_pid
    );

    let api_source = ws.join("crates/api-only/src/lib.rs");
    std::fs::write(
        &api_source,
        "#[link(name = \"ns_dev_missing_fixture_library\")]\nunsafe extern \"C\" { fn ns_dev_missing_fixture_symbol() -> u8; }\npub fn value() -> &'static str { unsafe { std::hint::black_box(ns_dev_missing_fixture_symbol()); } \"api-only-v4\" }\n",
    )
    .unwrap();
    dev.wait_log("[ns] build failed:", 1, WAIT);
    let retained = dev.wait_body(&["api-v2", "shared-v2", "api-only-v3"]);
    assert_eq!(DevSession::api_pid(&retained), api_env_pid);
    assert_eq!(dev.wait_worker("worker-v2 shared-v2"), worker_env_pid);
    assert!(pid_alive(api_env_pid as i32));
    assert!(pid_alive(worker_env_pid as i32));

    dev.stop();
    assert!(
        port_owner().is_empty(),
        "port remained after shutdown: {}",
        port_owner()
    );
    let logs = ws.join("target/ns-dev");
    let api_log = std::fs::read_to_string(logs.join("api.log")).unwrap();
    let worker_log = std::fs::read_to_string(logs.join("worker.log")).unwrap();
    let build_log = std::fs::read_to_string(logs.join("build.log")).unwrap();
    assert!(api_log.contains("api pid"));
    assert!(worker_log.contains("worker pid"));
    assert!(build_log.contains("checking"));
    assert!(!format!("{api_log}{worker_log}{build_log}").contains('\x1b'));
    assert!(dev.log().contains("[api] api pid"));
    assert!(dev.log().contains("[worker] worker pid"));
    assert!(!dev.log().contains('\x1b'));

    let failed_root = root.join("failed-initial");
    let (failed_ws, failed_store) = make_workspace(&failed_root);
    let config = std::fs::read_to_string(failed_ws.join("ns.toml")).unwrap();
    std::fs::write(
        failed_ws.join("ns.toml"),
        config.replace("[dev]\n", "[dev]\nlog_dir = false\n"),
    )
    .unwrap();
    std::fs::write(
        failed_ws.join("crates/api-only/src/lib.rs"),
        "pub fn value( -> &'static str { \"broken\" }\n",
    )
    .unwrap();
    let mut failed = DevSession::start(&failed_ws, &failed_store, &["--no-tui", "--bin", "api"]);
    failed.wait_log("[ns] check failed:", 1, WAIT);
    assert!(!failed.log().contains("started api (pid"));
    assert!(http_body().is_none());
    failed.stop();
    assert!(!failed_ws.join("target/ns-dev").exists());
    assert!(
        port_owner().is_empty(),
        "failed initial check leaked port: {}",
        port_owner()
    );
}
