//! `ns test` notices what a test depends on beyond Cargo's graph: an env
//! var it reads, a workspace file outside every package, another member's
//! binary it runs, plus the usual dependency data and code. Checked with the
//! runtime tracer and with it switched off (declared/static inputs only).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

use common::{NASPEED, Sandbox};

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// `core` (lib + data file), `tool` (bin, no relation), `app` (tests that
/// read an env var, a root file, run `tool`, read `core`'s data, call `core`).
fn workspace(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"core\", \"app\", \"tool\"]\n",
    );
    for name in ["core", "tool"] {
        write(
            &root.join(format!("{name}/Cargo.toml")),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
    }
    write(
        &root.join("core/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    );
    write(&root.join("core/data/value.txt"), "core-data-v1\n");
    write(
        &root.join("tool/src/main.rs"),
        "fn main() { println!(\"tool-v1\"); }\n",
    );
    write(&root.join("shared/value.txt"), "shared-v1\n");
    write(
        &root.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
    );
    write(
        &root.join("app/src/lib.rs"),
        r#"#[cfg(test)]
mod tests {
    use std::path::Path;
    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
    }
    #[test]
    fn env_var() {
        assert_eq!(std::env::var("PROBE_MODE").unwrap_or_default(), "good");
    }
    #[test]
    fn root_file() {
        let v = std::fs::read_to_string(root().join("shared/value.txt")).unwrap();
        assert_eq!(v.trim(), "shared-v1");
    }
    #[test]
    fn other_member_bin() {
        let out = std::process::Command::new(root().join("target/debug/tool")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "tool-v1");
    }
    #[test]
    fn dep_data_file() {
        let v = std::fs::read_to_string(root().join("core/data/value.txt")).unwrap();
        assert_eq!(v.trim(), "core-data-v1");
    }
    #[test]
    fn dep_code() {
        assert_eq!(core::answer(), 42);
    }
}
"#,
    );
}

struct Probe {
    sb: Sandbox,
    dir: PathBuf,
    traced: bool,
}

impl Probe {
    fn new(name: &str, traced: bool) -> Probe {
        let sb = Sandbox::new(name);
        let dir = sb.root.join("probe");
        workspace(&dir);
        let p = Probe { sb, dir, traced };
        p.cargo(&["build", "-q", "-p", "tool"]);
        p
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("NASPEED_")
                || k.starts_with("RUSTC_")
            {
                cmd.env_remove(k.as_ref());
            }
        }
        cmd.current_dir(&self.dir)
            .env("NO_COLOR", "1")
            .env("NASPEED_CONFIG_DIR", &self.sb.config)
            .env("NASPEED_STORE", &self.sb.store)
            .env("NASPEED_NO_AUTO_GC", "1")
            .env("NASPEED_TEST_TRACE", if self.traced { "1" } else { "0" });
        cmd
    }

    fn cargo(&self, args: &[&str]) {
        let out = self
            .command("cargo")
            .args(args)
            .arg("--offline")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn ns_test(&self, mode: &str) -> Output {
        self.command(NASPEED)
            .args(["test", "--explain", "-p", "app", "--offline"])
            .env("PROBE_MODE", mode)
            .output()
            .unwrap()
    }

    /// Replayed from the cache: passed without running.
    fn replays(&self) -> bool {
        let out = self.ns_test("good");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        err.contains("(cached)")
    }

    /// Passes, replayed or not (a rebuilt binary need not be byte-identical).
    fn passes(&self) {
        let out = self.ns_test("good");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Runs and fails (a stale pass would have been replayed instead).
    fn reruns_and_fails(&self, mode: &str) -> String {
        let out = self.ns_test(mode);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "replayed a stale pass:\n{err}");
        assert!(!err.contains("(cached)"), "{err}");
        err
    }

    fn set(&self, rel: &str, text: &str) {
        write(&self.dir.join(rel), text);
    }
}

fn every_dependency_change_reruns(traced: bool) {
    let p = Probe::new(if traced { "tracked" } else { "untracked" }, traced);
    assert!(!p.replays(), "first run runs");
    assert!(p.replays(), "unchanged run replays");

    let why = p.reruns_and_fails("bad");
    assert!(why.contains("PROBE_MODE"), "{why}");
    assert!(p.replays());

    p.set("shared/value.txt", "shared-v2\n");
    let why = p.reruns_and_fails("good");
    if traced {
        assert!(why.contains("shared/value.txt changed"), "{why}");
    } else {
        assert!(
            why.contains("workspace files outside the packages"),
            "{why}"
        );
    }
    // Back to the state of the stored pass: it applies again.
    p.set("shared/value.txt", "shared-v1\n");
    assert!(p.replays());

    p.set("tool/src/main.rs", "fn main() { println!(\"tool-v2\"); }\n");
    p.cargo(&["build", "-q", "-p", "tool"]);
    let why = p.reruns_and_fails("good");
    assert!(why.contains("bin:tool"), "{why}");
    p.set("tool/src/main.rs", "fn main() { println!(\"tool-v1\"); }\n");
    p.cargo(&["build", "-q", "-p", "tool"]);
    p.passes();

    p.set("core/data/value.txt", "core-data-v2\n");
    p.reruns_and_fails("good");
    p.set("core/data/value.txt", "core-data-v1\n");
    assert!(p.replays());

    p.set("core/src/lib.rs", "pub fn answer() -> u32 { 41 }\n");
    p.reruns_and_fails("good");
    p.set("core/src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    p.passes();
    assert!(p.replays());

    // A workspace file no test reads: only the tracer knows that.
    p.set("NOTES.md", "unrelated\n");
    assert_eq!(p.replays(), traced);
}

#[test]
#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
fn traced_runs_rerun_on_every_dependency_change() {
    every_dependency_change_reruns(true);
}

#[test]
fn untraced_runs_rerun_on_every_dependency_change() {
    every_dependency_change_reruns(false);
}
