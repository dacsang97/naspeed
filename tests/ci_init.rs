//! `ns ci init` on the `ns_project` fixture: the three workflows are valid
//! for actionlint, only main pushes can save the cache, permissions match
//! each workflow's job, image steps follow `[image].name`, existing files
//! are only replaced with `--force`, and the output is deterministic.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

use common::NASPEED;

const MAIN_PUSH_GUARD: &str = "if: github.event_name == 'push' && github.ref == 'refs/heads/main'";
const FILES: [&str; 3] = ["ns-ci.yml", "ns-build.yml", "ns-release.yml"];

struct Project {
    root: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        let root =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        common::copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project"),
            &root,
        );
        Project { root }
    }

    /// Replace the fixture's `[image]` section (`None` drops it).
    fn set_image(&self, name: Option<&str>) {
        let toml = self.root.join("ns.toml");
        let text = std::fs::read_to_string(&toml).unwrap();
        let mut text = text.split("[image]").next().unwrap().to_owned();
        if let Some(name) = name {
            text.push_str(&format!("[image]\nname = \"{name}\"\n"));
        }
        std::fs::write(&toml, text).unwrap();
    }

    fn init(&self, extra: &[&str]) -> Output {
        Command::new(NASPEED)
            .env("NO_COLOR", "1")
            .args(["ci", "init"])
            .args(extra)
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn path(&self, file: &str) -> PathBuf {
        self.root.join(".github/workflows").join(file)
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn ok(out: &Output) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    text
}

/// Steps of every job as text blocks: a step starts at `      - ` and runs
/// while lines stay indented deeper than the dash (or are blank).
fn steps(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in yaml.lines() {
        if line.starts_with("      - ") {
            out.extend(current.replace(format!("{line}\n")));
        } else if line.starts_with("        ") || line.trim().is_empty() {
            if let Some(step) = current.as_mut() {
                step.push_str(line);
                step.push('\n');
            }
        } else {
            out.extend(current.take());
        }
    }
    out.extend(current);
    out
}

/// Non-comment lines, trimmed: what GitHub actually evaluates.
fn code(yaml: &str) -> Vec<&str> {
    yaml.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .collect()
}

fn actionlint() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?).join("go/bin/actionlint");
    if home.is_file() {
        return Some(home);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("actionlint"))
        .find(|p| p.is_file())
}

fn assert_actionlint(p: &Project) {
    let Some(lint) = actionlint() else {
        eprintln!("skipped: actionlint not installed (~/go/bin or PATH)");
        return;
    };
    let out = Command::new(lint)
        .args(FILES.map(|f| p.path(f)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "actionlint:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn all_workflows_pass_actionlint() {
    // ghcr (one job), another registry (separate Linux job) and no image.
    for (name, image) in [
        ("ci-lint-ghcr", Some("ghcr.io/owner/hello-server")),
        (
            "ci-lint-other",
            Some("registry.example.com:5000/team/hello"),
        ),
        ("ci-lint-none", None),
    ] {
        let p = Project::new(name);
        p.set_image(image);
        let text = ok(&p.init(&[]));
        for file in FILES {
            assert!(
                text.contains("wrote workflow")
                    && text.contains(&format!("path={}", p.path(file).display())),
                "{text}"
            );
        }
        assert_actionlint(&p);
    }
}

#[test]
fn only_main_pushes_save_the_cache() {
    let p = Project::new("ci-cache");
    let text = ok(&p.init(&[]));
    assert!(text.contains("must be published"), "{text}");

    for file in FILES {
        let yaml = p.read(file);
        let steps = steps(&yaml);
        let writers: Vec<&String> = steps
            .iter()
            .filter(|s| s.contains("actions/cache/save") || s.contains("ns cache export"))
            .collect();
        let expected = if file == "ns-release.yml" { 0 } else { 2 };
        assert_eq!(writers.len(), expected, "{file}: {steps:#?}");
        for step in &writers {
            assert!(
                step.contains(MAIN_PUSH_GUARD),
                "{file} unguarded cache write:\n{step}"
            );
        }
        for step in steps.iter().filter(|s| s.contains("actions/cache/restore")) {
            assert!(
                !step.contains("if:"),
                "{file}: restore must always run:\n{step}"
            );
        }
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.contains("actions/cache/restore"))
                .count(),
            1,
            "{file}"
        );
    }

    let ci = p.read("ns-ci.yml");
    assert!(ci.contains("\n  pull_request:\n"));
    assert!(ci.contains("cargo clippy --locked --all-targets -- -D warnings"));
    assert!(ci.contains("ns test --locked"));
    let build = p.read("ns-build.yml");
    assert!(
        build.contains("on:\n  push:\n    branches: [main]\n\n"),
        "main only"
    );
    assert!(!build.contains("pull_request") && !build.contains("tags:"));
    assert!(build.contains("run: ns release --snapshot"));
    assert!(build.contains("1 build variant: hello-server"));
    let release = p.read("ns-release.yml");
    assert!(release.contains("on:\n  push:\n    tags: ['v*']\n\n"));
    assert!(release.contains("run: ns release --publish\n"));
    assert!(release.contains("sigstore/cosign-installer@"));
}

#[test]
fn permissions_per_workflow() {
    let p = Project::new("ci-perms");
    p.set_image(Some("ghcr.io/owner/hello-server"));
    ok(&p.init(&[]));
    for file in ["ns-ci.yml", "ns-build.yml"] {
        let yaml = p.read(file);
        assert!(
            yaml.contains("\npermissions:\n  contents: read\n\n"),
            "{file}"
        );
        assert!(
            !code(&yaml).iter().any(|l| l.ends_with(": write")),
            "{file}"
        );
    }
    let release = p.read("ns-release.yml");
    assert!(
        release.contains(
            "\npermissions:\n  contents: write\n  packages: write\n  id-token: write\n\n"
        )
    );
    // ghcr.io: the push stays in the release job, authenticated by GITHUB_TOKEN.
    assert!(release.contains("run: ns image --push"));
    assert!(!release.contains("docker/login-action"));
    assert!(!release.contains("\n  image:\n"));
}

#[test]
fn other_registries_push_from_a_linux_job() {
    let p = Project::new("ci-registry");
    // The fixture's own image name points at a local registry.
    let text = ok(&p.init(&[]));
    assert!(text.contains("REGISTRY_PASSWORD"), "{text}");
    let release = p.read("ns-release.yml");
    let (release_job, image_job) = release.split_once("\n  image:\n").expect("image job");
    assert!(!release_job.contains("run: ns image"));
    assert!(release_job.contains("actions/upload-artifact@"));
    assert!(image_job.contains("needs: release"));
    assert!(image_job.contains("runs-on: ubuntu-latest"));
    assert!(image_job.contains("permissions:\n      contents: read\n"));
    assert!(image_job.contains("actions/download-artifact@"));
    assert!(image_job.contains("docker/login-action@"));
    assert!(image_job.contains("registry: localhost:5000"));
    assert!(image_job.contains("run: ns image --push"));
}

#[test]
fn no_image_steps_without_image_name() {
    let p = Project::new("ci-noimage");
    p.set_image(None);
    ok(&p.init(&[]));
    for file in FILES {
        let yaml = p.read(file);
        assert!(!yaml.contains("run: ns image"), "{file}");
        assert!(!yaml.contains("docker/login-action"), "{file}");
        assert!(!yaml.contains("artifact@"), "{file}");
    }
    assert!(p.read("ns-release.yml").contains("ns release --publish"));
}

#[test]
fn each_existing_workflow_needs_force() {
    let p = Project::new("ci-force");
    ok(&p.init(&[]));
    let originals = FILES.map(|f| p.read(f));
    for file in FILES {
        std::fs::write(p.path(file), "# hand edited\n").unwrap();
        let others: Vec<&str> = FILES.into_iter().filter(|f| *f != file).collect();
        for other in &others {
            std::fs::remove_file(p.path(other)).unwrap();
        }

        let refused = p.init(&[]);
        assert!(!refused.status.success(), "{file}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("--force") && stderr.contains(file),
            "{stderr}"
        );
        assert_eq!(p.read(file), "# hand edited\n");
        for other in &others {
            assert!(!p.path(other).exists(), "nothing written when refusing");
        }

        ok(&p.init(&["--force"]));
        for (f, original) in FILES.iter().zip(&originals) {
            assert_eq!(&p.read(f), original, "{f}");
        }
    }
}

#[test]
fn output_is_deterministic() {
    let a = Project::new("ci-det");
    ok(&a.init(&[]));
    let first = FILES.map(|f| a.read(f));
    ok(&a.init(&["--force"]));
    assert_eq!(FILES.map(|f| a.read(f)), first);
}
