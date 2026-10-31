//! `ns release` end to end on a temp copy of `tests/fixtures/ns_project`:
//! the cross matrix in snapshot mode (archives, checksums, file formats,
//! reproducibility, metadata.json) and `--publish` against a local mock of
//! the GitHub REST API. Cross tests skip with a printed reason when a rustup
//! target or host tool is missing.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use common::{NASPEED, copy_dir};

const MATRIX: &[(&str, &str, &[&str])] = &[
    (
        "x86_64-unknown-linux-musl",
        "tar.gz",
        &["ELF 64-bit", "x86-64"],
    ),
    (
        "aarch64-unknown-linux-musl",
        "tar.gz",
        &["ELF 64-bit", "aarch64"],
    ),
    (
        "universal2-apple-darwin",
        "tar.gz",
        &["Mach-O universal binary"],
    ),
    ("x86_64-pc-windows-msvc", "zip", &["PE32+", "x86-64"]),
];

struct Ws {
    root: PathBuf,
    dir: PathBuf,
}

impl Ws {
    fn new(name: &str) -> Ws {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("release-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("ws");
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project"),
            &dir,
        );
        Ws { root, dir }
    }

    /// `naspeed release <args>` in the workspace with the test harness's own
    /// cargo/proxy/GitHub variables removed and git confined to `root`.
    fn ns(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(NASPEED);
        // Plain log lines: tests match on their text.
        cmd.env("NO_COLOR", "1");
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy().into_owned();
            let proxy = k.to_ascii_lowercase().ends_with("_proxy");
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("RUSTC_")
                || k.starts_with("GITHUB_")
                || k == "SOURCE_DATE_EPOCH"
                || proxy
            {
                cmd.env_remove(&k);
            }
        }
        cmd.arg("release")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .envs(envs.iter().copied());
        cmd.output().unwrap()
    }

    fn git(&self, args: &[&str]) {
        git_in(&self.dir, args);
    }

    fn write(&self, rel: &str, content: &str) {
        std::fs::write(self.dir.join(rel), content).unwrap();
    }

    /// Set the workspace version in Cargo.toml and Cargo.lock.
    fn set_version(&self, version: &str) {
        for file in ["Cargo.toml", "Cargo.lock"] {
            let path = self.dir.join(file);
            let text = std::fs::read_to_string(&path).unwrap();
            let edited = text.replace("version = \"0.3.1\"", &format!("version = \"{version}\""));
            assert_ne!(text, edited, "{file}");
            std::fs::write(path, edited).unwrap();
        }
    }

    /// Commit everything and tag `tag`, with `remote.git` as `origin`.
    fn git_repo(&self, tag: &str) -> PathBuf {
        let remote = self.root.join("remote.git");
        git_in(
            &self.root,
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        std::fs::write(self.dir.join(".gitignore"), "target/\ndist/\n").unwrap();
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "initial fixture"]);
        self.git(&["tag", "v0.3.0"]);
        std::fs::write(self.dir.join("NOTES.txt"), "second\n").unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "add release notes"]);
        self.git(&["tag", "-a", tag, "-m", tag]);
        self.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        remote
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=ns test",
            "-c",
            "user.email=ns@test.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn host_triple() -> String {
    let out = Command::new("rustc").arg("-vV").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
        .unwrap()
}

fn installed_targets() -> Vec<String> {
    let out = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    out.map(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// Reason to skip the cross matrix, if any.
fn matrix_skip_reason() -> Option<String> {
    let installed = installed_targets();
    let needed = [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ];
    if let Some(t) = needed.iter().find(|t| !installed.iter().any(|i| i == *t)) {
        return Some(format!("rustup target {t} is not installed"));
    }
    for tool in ["lipo", "file", "shasum"] {
        if Command::new("which")
            .arg(tool)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return Some(format!("{tool} is not on PATH"));
        }
    }
    None
}

fn archives(dist: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dist)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tar.gz") || n.ends_with(".zip"))
        .map(|n| (n.clone(), std::fs::read(dist.join(&n)).unwrap()))
        .collect()
}

/// Every file below `dir` (relative path → bytes).
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
                files.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    files
}

/// The first of `targets` that rustup has not installed.
fn missing_target(targets: &[&str]) -> Option<String> {
    let installed = installed_targets();
    targets
        .iter()
        .find(|t| !installed.iter().any(|i| i == *t))
        .map(|t| format!("rustup target {t} is not installed"))
}

fn sha256_hex(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

fn assert_metadata_contract(dist: &Path, version: &str) {
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dist.join("metadata.json")).unwrap()).unwrap();
    let keys: Vec<&str> = meta
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 8, "{keys:?}");
    assert_eq!(meta["project"], "hello-server");
    assert_eq!(meta["version"], version);
    assert!(meta["tag"].is_null() && meta["snapshot"] == true);
    assert_eq!(
        meta["prerelease"], true,
        "snapshot versions have a `-` part"
    );
    assert!(meta["commit"].is_null(), "no git in the snapshot copy");
    assert_eq!(meta["source_date_epoch"], 0);
    let artifacts = meta["artifacts"].as_array().unwrap();
    let count = |k: &str| artifacts.iter().filter(|a| a["kind"] == k).count();
    assert_eq!(
        (
            count("binary"),
            count("archive"),
            count("sbom"),
            count("checksums")
        ),
        (4, 4, 1, 1)
    );
    assert_eq!(artifacts.len(), 10, "no signature without cosign + OIDC");
    for a in artifacts {
        let obj = a.as_object().unwrap();
        assert_eq!(obj.len(), 6, "{a}");
        let path = dist.join(a["path"].as_str().unwrap());
        assert_eq!(a["sha256"].as_str().unwrap(), sha256_hex(&path), "{a}");
        match a["kind"].as_str().unwrap() {
            "checksums" => {
                assert!(a["target"].is_null() && a["bin"].is_null() && a["build"].is_null())
            }
            "sbom" => {
                assert!(a["target"].is_null(), "{a}");
                assert_eq!(a["build"], "hello-server");
                assert_eq!(a["path"], format!("hello-server-{version}.spdx.json"));
            }
            _ => {
                assert_eq!(a["bin"], "hello-server");
                assert_eq!(a["build"], "hello-server");
                assert!(MATRIX.iter().any(|(t, _, _)| a["target"] == *t), "{a}");
            }
        }
    }
}

#[test]
fn snapshot_matrix_is_complete_verifiable_and_reproducible() {
    if let Some(reason) = matrix_skip_reason() {
        eprintln!("skipped: {reason}");
        return;
    }
    let ws = Ws::new("matrix");
    let dist = ws.dir.join("dist");
    let version = "0.3.1-SNAPSHOT-nogit";

    let out = ws.ns(&["--snapshot", "--jobs", "2"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("signing skipped:"), "{}", text(&out));

    for (target, ext, kinds) in MATRIX {
        let name = format!("hello-server-{version}-{target}.{ext}");
        assert!(dist.join(&name).is_file(), "missing {name}");
        let exe = if target.contains("windows") {
            "hello-server.exe"
        } else {
            "hello-server"
        };
        let described = Command::new("file")
            .arg(dist.join(target).join(exe))
            .output()
            .unwrap();
        let described = String::from_utf8_lossy(&described.stdout);
        for k in *kinds {
            assert!(described.contains(k), "{target}: {described}");
        }
    }
    let top = format!("hello-server-{version}-x86_64-unknown-linux-musl");
    let listing = Command::new("tar")
        .arg("tzvf")
        .arg(dist.join(format!("{top}.tar.gz")))
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing.contains(&format!("{top}/hello-server"))
            && listing.contains(&format!("{top}/README.md")),
        "{listing}"
    );
    assert!(
        listing.contains("rwxr-xr-x") && listing.contains("rw-r--r--"),
        "{listing}"
    );

    let sums = std::fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    assert_eq!(sums.lines().count(), 5, "archives and the SBOM: {sums}");
    let check = Command::new("shasum")
        .args(["-a", "256", "-c", "SHA256SUMS"])
        .current_dir(&dist)
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", text(&check));
    assert_metadata_contract(&dist, version);

    let first = archives(&dist);
    let out = ws.ns(&["--snapshot"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let second = archives(&dist);
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>()
    );
    for (name, bytes) in &first {
        assert!(second[name] == *bytes, "{name} differs between runs");
    }
    assert!(dist.join(".ns-dist").is_file());
}

#[test]
fn refuses_foreign_dist_dir_and_names_missing_targets() {
    let ws = Ws::new("guards");
    let host = host_triple();
    std::fs::create_dir_all(ws.dir.join("dist")).unwrap();
    std::fs::write(ws.dir.join("dist/keep.txt"), "mine").unwrap();
    let out = ws.ns(&["--snapshot", "--target", &host], &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains(".ns-dist"), "{}", text(&out));
    assert!(
        ws.dir.join("dist/keep.txt").is_file(),
        "foreign file deleted"
    );

    let absent = "riscv64gc-unknown-linux-gnu";
    if installed_targets().iter().any(|t| t == absent) {
        eprintln!("skipped missing-target check: {absent} is installed");
        return;
    }
    let out = ws.ns(&["--snapshot", "--target", absent], &[]);
    assert!(!out.status.success());
    let msg = text(&out);
    assert!(
        msg.contains(absent) && msg.contains("rustup target add"),
        "{msg}"
    );
}

#[test]
fn release_without_snapshot_needs_a_matching_tag() {
    let ws = Ws::new("tag");
    let host = host_triple();
    let out = ws.ns(&["--target", &host], &[]);
    assert!(text(&out).contains("--snapshot"), "{}", text(&out));
    ws.git_repo("v9.9.9");
    let out = ws.ns(&["--target", &host], &[]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("does not match package version 0.3.1"),
        "{}",
        text(&out)
    );
}

/// `{os}_{arch}` names of the targets used below.
fn go_os_arch(target: &str) -> &'static str {
    match target {
        "x86_64-unknown-linux-musl" => "linux_amd64",
        "aarch64-unknown-linux-musl" => "linux_arm64",
        "aarch64-apple-darwin" => "darwin_arm64",
        "x86_64-apple-darwin" => "darwin_amd64",
        "x86_64-unknown-linux-gnu" => "linux_amd64",
        "aarch64-unknown-linux-gnu" => "linux_arm64",
        other => panic!("no os/arch name for {other} in this test"),
    }
}

const MULTI_BUILD_TOML: &str = r#"
[release]
targets = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "@HOST@"]
before = ["mkdir -p gen && echo \"built $NS_VERSION\" > gen/BUILD-INFO.txt"]
checksum = "checksums.txt"
archive = { name = "{binary}_{version}_{os}_{arch}", files = ["README.md", "gen/BUILD-INFO.txt"] }

[[release.builds]]
bin = "hello-server"

[[release.builds]]
id = "shout"
bin = "hello-server"
binary = "hello-server-shout"
features = ["shout"]
"#;

#[test]
fn two_builds_of_one_bin_ship_side_by_side_with_sboms_and_checksums() {
    let host = host_triple();
    let targets = [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        host.as_str(),
    ];
    if let Some(reason) = missing_target(&targets) {
        eprintln!("skipped: {reason}");
        return;
    }
    let ws = Ws::new("multi");
    ws.write("ns.toml", &MULTI_BUILD_TOML.replace("@HOST@", &host));
    let dist = ws.dir.join("dist");
    let version = "0.3.1-SNAPSHOT-nogit";

    let out = ws.ns(&["--snapshot", "--jobs", "3"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        std::fs::read_to_string(ws.dir.join("gen/BUILD-INFO.txt")).unwrap(),
        format!("built {version}\n"),
        "before hook ran with NS_VERSION"
    );

    for target in targets {
        for binary in ["hello-server", "hello-server-shout"] {
            assert!(
                dist.join(target).join(binary).is_file(),
                "{target}/{binary}"
            );
            let top = format!("{binary}_{version}_{}", go_os_arch(target));
            let archive = dist.join(format!("{top}.tar.gz"));
            assert!(archive.is_file(), "missing {top}.tar.gz");
            let listing = Command::new("tar")
                .arg("tzf")
                .arg(&archive)
                .output()
                .unwrap();
            let listing = String::from_utf8_lossy(&listing.stdout);
            for entry in [binary, "README.md", "gen/BUILD-INFO.txt"] {
                assert!(listing.contains(&format!("{top}/{entry}\n")), "{listing}");
            }
        }
    }

    // The host binaries run: both report the release version, and the
    // feature really changed the second build.
    let plain = dist.join(&host).join("hello-server");
    let shout = dist.join(&host).join("hello-server-shout");
    for (bin, name) in [(&plain, "hello-server"), (&shout, "hello-server")] {
        let out = Command::new(bin).arg("--version").output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("{name} {version}")
        );
    }
    assert_ne!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&shout).unwrap()
    );

    assert!(!dist.join("SHA256SUMS").exists());
    let sums = std::fs::read_to_string(dist.join("checksums.txt")).unwrap();
    assert_eq!(sums.lines().count(), 8, "6 archives + 2 SBOMs: {sums}");
    let check = Command::new("shasum")
        .args(["-a", "256", "-c", "checksums.txt"])
        .current_dir(&dist)
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", text(&check));

    for binary in ["hello-server", "hello-server-shout"] {
        let name = format!("{binary}-{version}.spdx.json");
        assert!(sums.contains(&format!("  {name}\n")), "{sums}");
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dist.join(&name)).unwrap()).unwrap();
        assert_eq!(doc["spdxVersion"], "SPDX-2.3");
        assert_eq!(doc["name"], format!("{binary}-{version}"));
        assert_eq!(doc["creationInfo"]["created"], "1970-01-01T00:00:00Z");
        let packages = doc["packages"].as_array().unwrap();
        let names: Vec<&str> = packages
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["greeter", "hello-server"], "{doc}");
        assert_eq!(
            packages[0]["externalRefs"][0]["referenceLocator"],
            "pkg:cargo/greeter@0.3.1"
        );
        let depends = doc["relationships"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["relationshipType"] == "DEPENDS_ON")
            .count();
        assert_eq!(depends, 1, "hello-server DEPENDS_ON greeter");
    }

    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dist.join("metadata.json")).unwrap()).unwrap();
    let artifacts = meta["artifacts"].as_array().unwrap();
    let shout_archives: Vec<&serde_json::Value> = artifacts
        .iter()
        .filter(|a| a["kind"] == "archive" && a["build"] == "shout")
        .collect();
    assert_eq!(shout_archives.len(), 3);
    assert!(
        shout_archives
            .iter()
            .all(|a| a["bin"] == "hello-server-shout")
    );
    assert!(
        artifacts
            .iter()
            .any(|a| a["kind"] == "sbom" && a["build"] == "shout")
    );
    assert!(
        artifacts
            .iter()
            .any(|a| a["kind"] == "checksums" && a["path"] == "checksums.txt")
    );

    let first = tree(&dist);
    let out = ws.ns(&["--snapshot", "--jobs", "1"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let second = tree(&dist);
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>()
    );
    for (name, bytes) in &first {
        assert!(second[name] == *bytes, "{name} differs between runs");
    }
}

#[test]
fn failing_before_hook_aborts_before_any_build() {
    let ws = Ws::new("hook");
    let host = host_triple();
    ws.write(
        "ns.toml",
        "[release]\nbefore = [\"echo ran > first.txt\", \"exit 7\", \"touch never.txt\"]\n",
    );
    let out = ws.ns(&["--snapshot", "--target", &host], &[]);
    assert!(!out.status.success());
    let msg = text(&out);
    assert!(msg.contains("before hook `exit 7` failed"), "{msg}");
    assert!(ws.dir.join("first.txt").is_file());
    assert!(
        !ws.dir.join("never.txt").exists(),
        "later hooks must not run"
    );
    assert!(
        !ws.dir.join("dist").join(&host).exists(),
        "no build after a failed hook"
    );

    ws.write(
        "ns.toml",
        "[release]\narchive = { name = \"{binary}-{goos}\" }\n",
    );
    let out = ws.ns(&["--snapshot", "--target", &host], &[]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("unknown placeholder {goos}"),
        "{}",
        text(&out)
    );
}

#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// Minimal GitHub REST mock: one request per connection, answers 201.
fn mock_github() -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, upload_base) = (seen.clone(), base.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            );
            let mut headers = BTreeMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let len: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let reply = if path == "/repos/o/r/releases" {
                serde_json::json!({
                    "id": 1,
                    "html_url": "https://github.example/o/r/releases/tag/v0.3.1",
                    "upload_url": format!("{upload_base}/uploads/repos/o/r/releases/1/assets{{?name,label}}"),
                })
            } else {
                serde_json::json!({"state": "uploaded"})
            };
            log.lock().unwrap().push(Request {
                method,
                path,
                headers,
                body,
            });
            let reply = reply.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (base, seen)
}

#[test]
fn publish_creates_release_and_uploads_assets_on_mock_api() {
    let ws = Ws::new("publish");
    let host = host_triple();
    if !installed_targets().contains(&host) {
        eprintln!("skipped: host rustup target {host} is not installed");
        return;
    }
    let token = "ghp_test_token_never_printed_123";
    let (api, seen) = mock_github();
    let envs = [("GITHUB_TOKEN", token), ("NS_GITHUB_API_URL", api.as_str())];
    let args = [
        "--publish",
        "--draft",
        "--repo",
        "o/r",
        "--target",
        host.as_str(),
    ];

    ws.git_repo("v0.3.1");
    std::fs::write(ws.dir.join("dirty.txt"), "x").unwrap();
    let out = ws.ns(&args, &envs);
    assert!(
        !out.status.success() && text(&out).contains("not clean"),
        "{}",
        text(&out)
    );
    std::fs::remove_file(ws.dir.join("dirty.txt")).unwrap();

    let out = ws.ns(&args, &envs);
    assert!(
        !out.status.success() && text(&out).contains("not on remote"),
        "{}",
        text(&out)
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing is sent before the checks pass"
    );

    ws.git(&["push", "-q", "origin", "main", "v0.3.1"]);
    let out = ws.ns(&args, &envs);
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(!all.contains(token), "token leaked into output");
    assert!(
        all.contains("url=https://github.example/o/r/releases/tag/v0.3.1"),
        "{all}"
    );

    let reqs = seen.lock().unwrap().clone();
    assert_eq!(
        reqs.len(),
        4,
        "create + archive + SBOM + SHA256SUMS: {reqs:?}"
    );
    for r in &reqs {
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {token}").as_str())
        );
    }
    let create: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(reqs[0].path, "/repos/o/r/releases");
    assert_eq!(create["tag_name"], "v0.3.1");
    assert_eq!(create["draft"], true);
    let body = create["body"].as_str().unwrap();
    assert!(
        body.contains("add release notes") && !body.contains("initial fixture"),
        "changelog since v0.3.0: {body}"
    );

    let dist = ws.dir.join("dist");
    let archive = format!("hello-server-0.3.1-{host}.tar.gz");
    assert_eq!(
        reqs[1].path,
        format!("/uploads/repos/o/r/releases/1/assets?name={archive}")
    );
    assert_eq!(reqs[1].body, std::fs::read(dist.join(&archive)).unwrap());
    assert_eq!(
        reqs[2].path,
        "/uploads/repos/o/r/releases/1/assets?name=hello-server-0.3.1.spdx.json"
    );
    assert_eq!(
        reqs[3].path,
        "/uploads/repos/o/r/releases/1/assets?name=SHA256SUMS"
    );
    assert_eq!(create["prerelease"], false);
    assert!(body.starts_with("## Changelog\n* "), "{body}");

    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dist.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["version"], "0.3.1");
    assert_eq!(meta["commit"].as_str().map(str::len), Some(40));
    assert!(
        meta["source_date_epoch"].as_u64().unwrap() > 0,
        "HEAD commit time"
    );
}

#[test]
fn publish_rc_tag_is_prerelease_draft_with_filtered_changelog() {
    let ws = Ws::new("publish-rc");
    let host = host_triple();
    if let Some(reason) = missing_target(&[host.as_str()]) {
        eprintln!("skipped: {reason}");
        return;
    }
    ws.set_version("0.3.1-rc.1");
    ws.write(
        "ns.toml",
        "[release]\nsbom = \"none\"\ndraft = true\nprerelease = \"auto\"\n\
         changelog = { sort = \"asc\", exclude = [\"^docs:\"] }\n",
    );
    let remote = ws.root.join("remote.git");
    git_in(
        &ws.root,
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    ws.write(".gitignore", "target/\ndist/\n");
    ws.git(&["init", "-q", "-b", "main"]);
    ws.git(&["add", "-A"]);
    ws.git(&["commit", "-q", "-m", "initial fixture"]);
    ws.git(&["tag", "v0.3.0"]);
    for (file, subject) in [
        ("a.txt", "feat: greet louder"),
        ("b.txt", "docs: explain flags"),
        ("c.txt", "fix: trailing newline"),
    ] {
        ws.write(file, subject);
        ws.git(&["add", "-A"]);
        ws.git(&["commit", "-q", "-m", subject]);
    }
    ws.git(&["tag", "-a", "v0.3.1-rc.1", "-m", "rc"]);
    ws.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    ws.git(&["push", "-q", "origin", "main", "v0.3.1-rc.1"]);

    let token = "ghp_rc_token_never_printed_456";
    let (api, seen) = mock_github();
    let out = ws.ns(
        &["--publish", "--repo", "o/r", "--target", host.as_str()],
        &[("GITHUB_TOKEN", token), ("NS_GITHUB_API_URL", api.as_str())],
    );
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(!all.contains(token), "token leaked into output");

    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 3, "create + archive + SHA256SUMS: {reqs:?}");
    let create: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(create["tag_name"], "v0.3.1-rc.1");
    assert_eq!(create["draft"], true, "draft comes from ns.toml");
    assert_eq!(
        create["prerelease"], true,
        "auto: the tag has a pre-release part"
    );
    let body = create["body"].as_str().unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3, "{body}");
    assert_eq!(lines[0], "## Changelog");
    assert!(
        lines[1].starts_with("* ") && lines[1].ends_with(" feat: greet louder"),
        "{body}"
    );
    assert!(
        lines[2].ends_with(" fix: trailing newline"),
        "asc = oldest first: {body}"
    );
    assert!(
        !body.contains("docs:") && !body.contains("initial fixture"),
        "{body}"
    );

    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ws.dir.join("dist/metadata.json")).unwrap()).unwrap();
    assert_eq!(meta["tag"], "v0.3.1-rc.1");
    assert_eq!(
        (meta["snapshot"].as_bool(), meta["prerelease"].as_bool()),
        (Some(false), Some(true))
    );
}
