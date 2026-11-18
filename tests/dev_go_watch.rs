//! Go `ns dev` end-to-end: real Go builds, selective restarts, tests, and failure retention.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{NASPEED, copy_dir};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn body(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(300),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nhost: x\r\n\r\n").ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.trim().to_string())
}

struct Session {
    child: Child,
    log: Arc<Mutex<String>>,
    workspace: PathBuf,
}

impl Session {
    fn start(workspace: PathBuf) -> Session {
        let mut child = Command::new(NASPEED)
            .args(["dev", "--all", "--no-tui"])
            .current_dir(&workspace)
            .env("NO_COLOR", "1")
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
        Session {
            child,
            log,
            workspace,
        }
    }

    fn text(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    fn count(&self, needle: &str) -> usize {
        self.text().matches(needle).count()
    }

    fn wait_log(&self, needle: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while self.count(needle) < count {
            assert!(
                Instant::now() < deadline,
                "missing {needle:?}:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_body(&self, port: u16, path: &str, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if body(port, path).as_deref() == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing body {expected:?}:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn pid(&self, port: u16) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = body(port, "/pid").and_then(|pid| pid.parse().ok()) {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "missing pid on {port}:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn replace(&self, relative: &str, from: &str, to: &str) {
        let path = self.workspace.join(relative);
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(source.contains(from), "{} lacks {from:?}", path.display());
        std::fs::write(path, source.replace(from, to)).unwrap();
    }

    fn replace_all(&self, relative: &str, replacements: &[(&str, &str)]) {
        let path = self.workspace.join(relative);
        let mut source = std::fs::read_to_string(&path).unwrap();
        for (from, to) in replacements {
            assert!(source.contains(from), "{} lacks {from:?}", path.display());
            source = source.replace(from, to);
        }
        std::fs::write(path, source).unwrap();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
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
fn go_workspace_builds_tests_and_restarts_only_affected_mains() {
    if Command::new("go").arg("version").output().is_err() {
        return;
    }
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dev-go-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_go_work"),
        &root,
    );
    let api_port = free_port();
    let mut tool_port = free_port();
    while tool_port == api_port {
        tool_port = free_port();
    }
    let config = std::fs::read_to_string(root.join("ns.toml")).unwrap();
    std::fs::write(
        root.join("ns.toml"),
        config
            .replace("19082", &api_port.to_string())
            .replace("19083", &tool_port.to_string()),
    )
    .unwrap();

    let session = Session::start(root.clone());
    session.wait_body(api_port, "/", "hello go v1");
    session.wait_log("started tool", 1);
    let api1 = session.pid(api_port);
    let tool1 = session.pid(tool_port);

    session.replace(
        "svc/internal/greet/greeting.txt",
        "hello go v1",
        "hello go v2",
    );
    session.wait_body(api_port, "/", "hello go v2");
    let api2 = session.pid(api_port);
    assert_ne!(api1, api2);
    assert_eq!(session.pid(tool_port), tool1);

    session.replace_all(
        "svc/cmd/api/main.go",
        &[
            (
                "\"example.test/svc/internal/greet\"",
                "\"example.test/svc/internal/extra\"\n\t\"example.test/svc/internal/greet\"",
            ),
            (
                "fmt.Fprint(w, greet.Text())",
                "fmt.Fprint(w, greet.Text(), \" \", extra.Value())",
            ),
        ],
    );
    session.wait_body(api_port, "/", "hello go v2 extra v1");
    let api3 = session.pid(api_port);
    assert_ne!(api2, api3);

    session.replace("svc/internal/extra/extra.go", "extra v1", "extra v2");
    session.wait_body(api_port, "/", "hello go v2 extra v2");
    let api4 = session.pid(api_port);
    assert_ne!(api3, api4);
    assert_eq!(session.pid(tool_port), tool1);

    std::fs::create_dir(root.join("dynamic")).unwrap();
    std::fs::write(
        root.join("dynamic/go.mod"),
        "module example.test/dynamic\n\ngo 1.25\n",
    )
    .unwrap();
    std::fs::write(
        root.join("dynamic/value.go"),
        "package dynamic\nfunc Value() string { return \"dynamic v1\" }\n",
    )
    .unwrap();
    let workspace_starts = session.count("started api");
    let workspace_tool_starts = session.count("started tool");
    session.replace("go.work", "\t./tools", "\t./tools\n\t./dynamic");
    session.wait_log("started api", workspace_starts + 1);
    session.wait_log("started tool", workspace_tool_starts + 1);
    let tool2 = session.pid(tool_port);
    session.replace_all(
        "svc/cmd/api/main.go",
        &[
            (
                "\"example.test/svc/internal/extra\"",
                "\"example.test/dynamic\"\n\t\"example.test/svc/internal/extra\"",
            ),
            (
                "fmt.Fprint(w, greet.Text(), \" \", extra.Value())",
                "fmt.Fprint(w, greet.Text(), \" \", extra.Value(), \" \", dynamic.Value())",
            ),
        ],
    );
    session.wait_body(api_port, "/", "hello go v2 extra v2 dynamic v1");
    let api5 = session.pid(api_port);

    session.replace("dynamic/value.go", "dynamic v1", "dynamic v2");
    session.wait_body(api_port, "/", "hello go v2 extra v2 dynamic v2");
    let api6 = session.pid(api_port);
    assert_ne!(api5, api6);
    assert_eq!(session.pid(tool_port), tool2);

    session.replace(
        "tools/cmd/tool/main.go",
        "package main",
        "package main\n// tool edit",
    );
    session.wait_log("started tool", workspace_tool_starts + 2);
    assert_ne!(session.pid(tool_port), tool2);
    assert_eq!(session.pid(api_port), api6);

    let starts = session.count("started api");
    let tests = session.count("tests passed");
    session.replace(
        "svc/internal/greet/greet_test.go",
        "func TestText",
        "// test-only edit\nfunc TestText",
    );
    session.wait_log("tests passed", tests + 1);
    assert_eq!(session.count("started api"), starts);
    assert_eq!(session.pid(api_port), api6);

    let helper_tests = session.count("tests passed");
    session.replace(
        "svc/internal/testutil/testutil.go",
        "func NonEmpty",
        "// test helper edit\nfunc NonEmpty",
    );
    session.wait_log("tests passed", helper_tests + 1);
    assert_eq!(session.count("started api"), starts);
    assert_eq!(session.pid(api_port), api6);

    session.replace(
        "svc/internal/greet/greet.go",
        "func Text() string",
        "func Text() broken",
    );
    session.wait_log("build failed", 1);
    assert_eq!(session.pid(api_port), api6);
    assert_eq!(
        body(api_port, "/").as_deref(),
        Some("hello go v2 extra v2 dynamic v2")
    );

    session.replace(
        "svc/internal/greet/greet.go",
        "func Text() broken",
        "func Text() string",
    );
    session.wait_log("started api", starts + 1);
    assert_ne!(session.pid(api_port), api6);

    drop(session);
    let _ = std::fs::remove_dir_all(root);
}
