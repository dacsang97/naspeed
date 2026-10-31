//! `ns image` against a real `registry:2` on localhost:5055 (Docker via
//! OrbStack) with distroless `:nonroot` from gcr.io: push a two-platform
//! index with Dockerfile-style settings and an SBOM, run both platforms,
//! inspect the config and the index, fetch the `sha256-<hex>.sbom` artifact,
//! check a repeat push copies nothing and a push within the same registry
//! mounts base layers instead of copying them. Skips (with the reason
//! printed) when Docker or the network is unavailable.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{NASPEED, copy_dir};

const REGISTRY: &str = "localhost:5055";
const CONTAINER: &str = "ns-test-registry";
const REPO: &str = "localhost:5055/ns-fixture/hello-server";
const TARGETS: [(&str, &str); 2] = [
    ("linux/amd64", "x86_64-unknown-linux-musl"),
    ("linux/arm64", "aarch64-unknown-linux-musl"),
];

/// Run with a deadline; `None` when it could not start or timed out (killed).
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<Output> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn docker(args: &[&str], timeout: Duration) -> Option<Output> {
    run_with_timeout(Command::new("docker").args(args), timeout)
}

/// Starting a container can take minutes on a slow Docker VM.
const CONTAINER_TIMEOUT: Duration = Duration::from_secs(600);

fn docker_ok(args: &[&str]) -> String {
    let out = docker(args, CONTAINER_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "docker {args:?} did not finish within {}s (slow Docker VM?)",
            CONTAINER_TIMEOUT.as_secs()
        )
    });
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "docker {args:?} failed: {text}");
    text
}

fn docker_ready() -> bool {
    docker(
        &["info", "--format", "{{.ServerVersion}}"],
        Duration::from_secs(20),
    )
    .is_some_and(|o| o.status.success())
}

/// Docker reachable, starting OrbStack once if needed.
fn ensure_docker() -> Result<(), String> {
    if docker_ready() {
        return Ok(());
    }
    let started = run_with_timeout(
        Command::new("orbctl").arg("start"),
        Duration::from_secs(180),
    );
    if !started.is_some_and(|o| o.status.success()) {
        return Err("docker is unavailable and `orbctl start` failed".into());
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if docker_ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    Err("docker did not become ready after `orbctl start`".into())
}

/// Body of a registry API GET via curl, with an `Accept` header.
fn registry_get(path: &str, accept: &str) -> Vec<u8> {
    let out = run_with_timeout(
        Command::new("curl").args([
            "-sf",
            "--max-time",
            "30",
            "-H",
            &format!("Accept: {accept}"),
            &format!("http://{REGISTRY}/v2/{path}"),
        ]),
        Duration::from_secs(40),
    )
    .unwrap_or_else(|| panic!("GET {path} timed out"));
    assert!(out.status.success(), "GET {path} failed");
    out.stdout
}

/// HTTP status of `url` via curl (`000` when unreachable).
fn http_status(url: &str) -> String {
    run_with_timeout(
        Command::new("curl").args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "15",
            url,
        ]),
        Duration::from_secs(20),
    )
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .unwrap_or_else(|| "000".into())
}

/// Removes the registry container (started with --rm) and pulled images,
/// also when an assertion fails.
struct RegistryGuard;

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", CONTAINER], CONTAINER_TIMEOUT);
        for tag in ["0.3.1", "latest"] {
            let _ = docker(&["rmi", "-f", &format!("{REPO}:{tag}")], CONTAINER_TIMEOUT);
        }
    }
}

fn start_registry() -> RegistryGuard {
    // A container with this fixed name can only be a leftover of this test.
    let _ = docker(&["rm", "-f", CONTAINER], CONTAINER_TIMEOUT);
    if std::net::TcpListener::bind("127.0.0.1:5055").is_err() {
        panic!(
            "port 5055 is in use by another process; free it (lsof -i :5055) before running this test"
        );
    }
    let guard = RegistryGuard;
    docker_ok(&[
        "run",
        "-d",
        "--rm",
        "--name",
        CONTAINER,
        "-p",
        "5055:5000",
        "registry:2",
    ]);
    let deadline = Instant::now() + Duration::from_secs(60);
    while http_status(&format!("http://{REGISTRY}/v2/")) != "200" {
        assert!(
            Instant::now() < deadline,
            "registry on {REGISTRY} did not come up"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    guard
}

/// `cargo build --release --target <t>` for both targets, outside the test's cargo env.
fn build_fixture(workspace: &Path, target_dir: &Path) {
    for (_, target) in TARGETS {
        let mut cmd = Command::new("cargo");
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("RUSTC_")
                || k.starts_with("NASPEED_")
            {
                cmd.env_remove(k.as_ref());
            }
        }
        let out = cmd
            .args(["build", "--release", "--target", target])
            .current_dir(workspace)
            .env("CARGO_TARGET_DIR", target_dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cargo build --target {target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

struct ImageRun {
    stdout: String,
    stderr: String,
}

impl ImageRun {
    /// `<repo>@sha256:…` printed last on stdout.
    fn digest_ref(&self) -> &str {
        self.stdout.lines().last().unwrap_or_default()
    }

    /// Value of `<n> <label>` in the `base layers: …; new blobs: …` summary.
    fn count(&self, label: &str) -> u64 {
        let line = self
            .stderr
            .lines()
            .find(|l| l.contains("base layers:"))
            .expect("push summary line");
        let section = if label.starts_with("new ") {
            line.split("new blobs:").nth(1)
        } else {
            line.split(';').next()
        };
        let label = label.trim_start_matches("new ");
        let words: Vec<&str> = section
            .unwrap()
            .split(|c: char| c == ',' || c == ':' || c.is_whitespace())
            .filter(|w| !w.is_empty())
            .collect();
        let pos = words
            .iter()
            .position(|w| *w == label)
            .unwrap_or_else(|| panic!("{label} in {line}"));
        words[pos - 1].parse().unwrap()
    }
}

fn ns_image(workspace: &Path, store: &Path, extra: &[&str], binaries: &[PathBuf]) -> ImageRun {
    let mut cmd = Command::new(NASPEED);
    // Plain log lines: tests match on their text.
    cmd.env("NO_COLOR", "1");
    cmd.arg("image").args(extra);
    for ((platform, _), path) in TARGETS.iter().zip(binaries) {
        cmd.arg("--binary")
            .arg(format!("{platform}={}", path.display()));
    }
    let out = cmd
        .current_dir(workspace)
        .env("NASPEED_STORE", store)
        .env("NASPEED_CONFIG_DIR", store.join("config"))
        .env("SOURCE_DATE_EPOCH", "1700000000")
        .output()
        .unwrap();
    let run = ImageRun {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    assert!(
        out.status.success(),
        "ns image {extra:?} failed:\n{}{}",
        run.stdout,
        run.stderr
    );
    run
}

#[test]
fn pushes_runnable_multi_arch_image_to_local_registry() {
    if let Err(reason) = ensure_docker() {
        eprintln!("SKIP pushes_runnable_multi_arch_image_to_local_registry: {reason}");
        return;
    }
    if http_status("https://gcr.io/v2/") == "000" {
        eprintln!(
            "SKIP pushes_runnable_multi_arch_image_to_local_registry: gcr.io unreachable (offline?)"
        );
        return;
    }
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ns-image-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = root.join("ws");
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project"),
        &workspace,
    );
    // Shared across runs so repeated test runs do not rebuild from scratch.
    let target_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ns-image-fixture-target");
    build_fixture(&workspace, &target_dir);
    let binaries: Vec<PathBuf> = TARGETS
        .iter()
        .map(|(_, t)| target_dir.join(t).join("release/hello-server"))
        .collect();

    // Dockerfile-style runtime settings (cf. EXPOSE/ENV/CMD) and an SBOM.
    std::fs::write(
        workspace.join("ns.toml"),
        r#"[image]
name = "localhost:5000/ns-fixture/hello-server"
cmd = ["--port", "8080"]
env = ["GREETING_SUFFIX=!"]
ports = ["8080"]
volumes = ["/data"]
labels = { "com.example.suite" = "ns-image" }
"#,
    )
    .unwrap();
    let sbom_file = workspace.join("hello-server-0.3.1.spdx.json");
    let sbom_doc = br#"{"spdxVersion":"SPDX-2.3","name":"hello-server-0.3.1","packages":[]}"#;
    std::fs::write(&sbom_file, sbom_doc).unwrap();
    let sbom_arg = sbom_file.to_str().unwrap();

    let _registry = start_registry();

    // Dry run: layout written, nothing reaches the registry.
    let dry = ns_image(
        &workspace,
        &store,
        &["--registry", REGISTRY, "--sbom", sbom_arg],
        &binaries,
    );
    assert!(workspace.join("dist/oci/index.json").is_file());
    assert!(dry.stderr.contains("would push"), "{}", dry.stderr);
    assert!(dry.stderr.contains("would push SBOM"), "{}", dry.stderr);
    let catalog = Command::new("curl")
        .args(["-s", &format!("http://{REGISTRY}/v2/_catalog")])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&catalog.stdout).contains(r#""repositories":[]"#));

    let first = ns_image(
        &workspace,
        &store,
        &["--push", "--registry", REGISTRY, "--sbom", sbom_arg],
        &binaries,
    );
    assert_eq!(
        first.digest_ref(),
        dry.digest_ref(),
        "dry run and push build identical bytes"
    );
    assert!(
        first.digest_ref().starts_with(&format!("{REPO}@sha256:")),
        "{}",
        first.stdout
    );
    assert!(first.count("copied") > 0, "{}", first.stderr);
    assert_eq!(first.count("new uploaded"), 4, "{}", first.stderr);

    for (platform, _) in TARGETS {
        let out = docker_ok(&[
            "run",
            "--rm",
            "--pull",
            "always",
            "--platform",
            platform,
            &format!("{REPO}:0.3.1"),
            "--version",
        ]);
        assert!(
            out.lines().any(|l| l.trim() == "hello-server 0.3.1"),
            "{platform}: {out}"
        );
    }
    // The last run pulled linux/arm64; the config is the same on both.
    let config: serde_json::Value = serde_json::from_str(&docker_ok(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config}}",
        &format!("{REPO}:0.3.1"),
    ]))
    .unwrap();
    assert_eq!(config["User"], "65532:65532", "{config}");
    assert_eq!(
        config["Entrypoint"],
        serde_json::json!(["/usr/bin/hello-server"]),
        "{config}"
    );
    assert_eq!(
        config["Cmd"],
        serde_json::json!(["--port", "8080"]),
        "{config}"
    );
    assert!(config["ExposedPorts"].get("8080/tcp").is_some(), "{config}");
    assert!(config["Volumes"].get("/data").is_some(), "{config}");
    assert!(
        config["Env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "GREETING_SUFFIX=!"),
        "{config}"
    );
    let labels = &config["Labels"];
    assert_eq!(labels["org.opencontainers.image.title"], "hello-server");
    assert_eq!(
        labels["org.opencontainers.image.created"],
        "2023-11-14T22:13:20Z"
    );
    assert_eq!(labels["com.example.suite"], "ns-image");

    // SBOM artifact: tagged sha256-<index hex>.sbom, subject = the index.
    let index_digest = first.digest_ref().split_once('@').unwrap().1.to_string();
    let sbom_tag = format!("sha256-{}.sbom", index_digest.trim_start_matches("sha256:"));
    assert!(first.stderr.contains(&sbom_tag), "{}", first.stderr);
    let sbom_manifest: serde_json::Value = serde_json::from_slice(&registry_get(
        &format!("ns-fixture/hello-server/manifests/{sbom_tag}"),
        "application/vnd.oci.image.manifest.v1+json",
    ))
    .unwrap();
    assert_eq!(sbom_manifest["artifactType"], "text/spdx+json");
    assert_eq!(
        sbom_manifest["config"]["mediaType"],
        "application/vnd.oci.empty.v1+json"
    );
    assert_eq!(sbom_manifest["subject"]["digest"], index_digest.as_str());
    assert_eq!(
        sbom_manifest["subject"]["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    let layer = &sbom_manifest["layers"][0];
    assert_eq!(layer["mediaType"], "text/spdx+json");
    let blob = registry_get(
        &format!(
            "ns-fixture/hello-server/blobs/{}",
            layer["digest"].as_str().unwrap()
        ),
        "*/*",
    );
    assert_eq!(blob, sbom_doc);

    let inspect = docker_ok(&["buildx", "imagetools", "inspect", &format!("{REPO}:latest")]);
    assert!(
        inspect.contains("linux/amd64") && inspect.contains("linux/arm64"),
        "{inspect}"
    );

    // Same binaries again: every blob is already there, nothing is uploaded.
    let second = ns_image(
        &workspace,
        &store,
        &["--push", "--registry", REGISTRY, "--sbom", sbom_arg],
        &binaries,
    );
    assert_eq!(second.digest_ref(), first.digest_ref());
    assert_eq!(second.count("copied"), 0, "{}", second.stderr);
    assert_eq!(second.count("new uploaded"), 0, "{}", second.stderr);
    assert!(second.count("already") > 0, "{}", second.stderr);
    assert!(
        !second.stderr.contains("fetching base layer"),
        "{}",
        second.stderr
    );

    // Base in the same registry: layers are mounted, not copied.
    std::fs::write(
        workspace.join("ns.toml"),
        "[image]\nname = \"localhost:5055/ns-fixture/mounted\"\nbase = \"localhost:5055/ns-fixture/hello-server:0.3.1\"\n",
    )
    .unwrap();
    let mounted = ns_image(&workspace, &store, &["--push", "--tag", "m1"], &binaries);
    assert!(mounted.count("mounted") > 0, "{}", mounted.stderr);
    assert_eq!(mounted.count("copied"), 0, "{}", mounted.stderr);

    let _ = std::fs::remove_dir_all(&root);
}
