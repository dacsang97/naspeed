//! `ns dev` end to end on a copy of the `ns_project` fixture: real cargo,
//! real file watching, a real HTTP server on port 18080. Everything runs in
//! one test because the scenario shares that single port.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{NASPEED, copy_dir};

const PORT: u16 = 18080;

fn http_body() -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
        .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.trim().to_string())
}

fn port_owner() -> String {
    let out = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{PORT}"), "-sTCP:LISTEN"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// `ns dev` running in the background with its combined output captured.
struct DevSession {
    child: Child,
    log: Arc<Mutex<String>>,
    ws: PathBuf,
}

impl DevSession {
    fn start(ws: &Path, store: &Path) -> DevSession {
        let mut cmd = Command::new(NASPEED);
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("NASPEED_")
                || k.starts_with("RUSTC_")
                || k == "CARGO"
            {
                cmd.env_remove(k.as_ref());
            }
        }
        let mut child = cmd
            .arg("dev")
            .current_dir(ws)
            .env("RUSTC_WRAPPER", NASPEED)
            .env("CARGO_TARGET_DIR", ws.join("target"))
            .env("NASPEED_STORE", store)
            .env("NASPEED_CONFIG_DIR", store.join("config"))
            .env("NASPEED_MIN_FREE_MEM_MB", "0")
            .env("NASPEED_NO_AUTO_GC", "1")
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
                    eprintln!("  | {line}");
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

    fn count(&self, needle: &str) -> usize {
        self.log().matches(needle).count()
    }

    fn wait_log(&self, needle: &str, n: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.count(needle) < n {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {n}x {needle:?}; log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_body(&self, want: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let body = http_body();
            if body.as_deref() == Some(want) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for body {want:?} (last {body:?}); log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Pid of the most recently started app process, from `[ns] started`.
    fn app_pid(&self) -> i32 {
        let log = self.log();
        let line = log
            .lines()
            .rfind(|l| l.contains("[ns] started"))
            .expect("no app started");
        let rest = line.split("(pid ").nth(1).unwrap();
        rest[..rest.find(')').unwrap()].parse().unwrap()
    }

    fn edit(&self, rel: &str, from: &str, to: &str) {
        let path = self.ws.join(rel);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{rel} lacks {from:?}");
        std::fs::write(&path, text.replace(from, to)).unwrap();
    }
}

impl Drop for DevSession {
    fn drop(&mut self) {
        // Never leave a server behind when an assertion fails mid-scenario.
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(15);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn dev_loop_rebuilds_restarts_and_cleans_up() {
    assert!(
        http_body().is_none() && port_owner().is_empty(),
        "port {PORT} is busy; owner:\n{}",
        port_owner()
    );
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dev-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let ws = root.join("ws");
    let store = root.join("store");
    std::fs::create_dir_all(store.join("config")).unwrap();
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project"),
        &ws,
    );
    let ns_toml = std::fs::read_to_string(ws.join("ns.toml")).unwrap();
    std::fs::write(
        ws.join("ns.toml"),
        ns_toml.replace("[dev]\n", "[dev]\nenv_file = \".env\"\n"),
    )
    .unwrap();
    std::fs::write(ws.join(".env"), "GREETING_SUFFIX=\n").unwrap();

    let dev = DevSession::start(&ws, &store);
    let first_build = Duration::from_secs(240);
    let rebuild = Duration::from_secs(120);
    dev.wait_log("[ns] started hello-server", 1, first_build);
    dev.wait_body("hello v1", Duration::from_secs(10));

    // (a) a library edit rebuilds and restarts with the new body.
    dev.edit("crates/greeter/src/lib.rs", "hello v1", "hello v2");
    dev.wait_body("hello v2", rebuild);
    dev.wait_log("[ns] started hello-server", 2, Duration::from_secs(5));

    // A non-Rust watched file (.env) only restarts.
    let builds = dev.count("; building hello-server");
    std::fs::write(ws.join(".env"), "GREETING_SUFFIX=-env\n").unwrap();
    dev.wait_body("hello v2-env", Duration::from_secs(30));
    assert_eq!(
        dev.count("; building hello-server"),
        builds,
        "a .env change must not rebuild"
    );

    // (b) a compile error is reported and the old server keeps serving.
    let pid_before = dev.app_pid();
    dev.edit(
        "crates/greeter/src/lib.rs",
        "\"hello v2\"",
        "\"hello v3\" +",
    );
    dev.wait_log("[ns] check failed", 1, rebuild);
    let log = dev.log();
    assert!(log.contains("error"), "{log}");
    assert_eq!(http_body().as_deref(), Some("hello v2-env"));
    assert_eq!(dev.app_pid(), pid_before, "no restart on a failed check");
    assert!(pid_alive(pid_before));

    // (c) fixing it restarts with the new body.
    dev.edit(
        "crates/greeter/src/lib.rs",
        "\"hello v3\" +",
        "\"hello v3\"",
    );
    dev.wait_body("hello v3-env", rebuild);
    assert_ne!(dev.app_pid(), pid_before);
    assert!(!pid_alive(pid_before), "old server must be gone");

    // An event storm (200 new files at once) causes exactly one rebuild.
    dev.wait_log("[ns] started hello-server", 4, Duration::from_secs(10));
    let (builds, starts) = (
        dev.count("; building hello-server"),
        dev.count("[ns] started"),
    );
    for i in 0..200 {
        std::fs::write(
            ws.join(format!("crates/greeter/src/storm_{i}.rs")),
            "// storm\n",
        )
        .unwrap();
    }
    dev.wait_log("[ns] started hello-server", starts + 1, rebuild);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        dev.count("; building hello-server"),
        builds + 1,
        "log:\n{}",
        dev.log()
    );
    assert_eq!(dev.count("[ns] started"), starts + 1);

    // (d) SIGINT to ns dev: it exits, the port is free, no app process is left.
    let app = dev.app_pid();
    let mut dev = dev;
    // SAFETY: signalling our own child.
    unsafe { libc::kill(dev.child.id() as i32, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = dev.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "ns dev ignored SIGINT");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "{status:?}");
    assert!(!pid_alive(app), "app pid {app} survived");
    // SAFETY: probing the app's (former) process group.
    assert!(
        unsafe { libc::kill(-app, 0) } != 0,
        "app process group survived"
    );
    assert!(
        port_owner().is_empty(),
        "port {PORT} still owned:\n{}",
        port_owner()
    );
    assert!(http_body().is_none());
    let leftover = Command::new("pgrep")
        .args(["-f", &ws.to_string_lossy()])
        .output()
        .unwrap();
    assert!(
        leftover.stdout.is_empty(),
        "leftover processes: {}",
        String::from_utf8_lossy(&leftover.stdout)
    );
    assert!(dev.log().contains("[ns] stopped"));

    drop(dev);
    let _ = std::fs::remove_dir_all(&root);
}
