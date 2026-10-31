//! `ns test` end to end on the ns_project fixture: an unchanged run replays,
//! an edit re-runs exactly the binaries that depend on it, failures are
//! never cached and `--no-cache` runs everything.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

use common::{NASPEED, Sandbox};

struct Proj {
    sb: Sandbox,
    dir: PathBuf,
}

impl Proj {
    fn new(name: &str) -> Proj {
        let sb = Sandbox::new(name);
        let dir = sb.root.join("proj");
        common::copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project"),
            &dir,
        );
        Proj { sb, dir }
    }

    fn ns_test(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(NASPEED);
        // Plain log lines: tests match on their text.
        cmd.env("NO_COLOR", "1");
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("NASPEED_")
                || k.starts_with("RUSTC_")
            {
                cmd.env_remove(k.as_ref());
            }
        }
        cmd.arg("test")
            .args(args)
            .current_dir(&self.dir)
            .env("NASPEED_CONFIG_DIR", &self.sb.config)
            .env("NASPEED_STORE", &self.sb.store)
            .env("NASPEED_NO_AUTO_GC", "1")
            .output()
            .unwrap()
    }

    /// (runs, replays) recorded so far.
    fn counts(&self) -> (usize, usize) {
        let text = std::fs::read_to_string(self.sb.store.join("usage.log")).unwrap_or_default();
        let n = |p: &str| text.lines().filter(|l| l.starts_with(p)).count();
        (n("TR\t"), n("TH\t"))
    }

    fn edit(&self, rel: &str, from: &str, to: &str) {
        let path = self.dir.join(rel);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{rel} lacks {from:?}");
        std::fs::write(&path, text.replace(from, to)).unwrap();
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn unchanged_tests_replay_and_edits_rerun_dependents() {
    let p = Proj::new("testcache");
    let first = p.ns_test(&["--offline"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let (runs, replays) = p.counts();
    // greeter's unit tests and hello-server's (bin) unit tests.
    assert_eq!((runs, replays), (2, 0), "{}", stderr(&first));

    let second = p.ns_test(&["--offline"]);
    assert!(second.status.success());
    assert_eq!(p.counts(), (2, 2));
    assert_eq!(stderr(&second).matches("(cached)").count(), 2);
    // The replay is the original libtest output.
    assert!(String::from_utf8_lossy(&second.stdout).contains("test result: ok. 1 passed"));

    // A library edit re-runs it and the binary depending on it.
    p.edit("crates/greeter/src/lib.rs", "hello v1", "hello v2");
    assert!(p.ns_test(&["--offline"]).status.success());
    assert_eq!(p.counts(), (4, 2));

    // A binary-only edit leaves the library's result cached.
    p.edit(
        "apps/hello-server/src/main.rs",
        "fn main()",
        "// edited\nfn main()",
    );
    let out = p.ns_test(&["--offline"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(p.counts(), (5, 3), "{}", stderr(&out));

    // `--no-cache` runs everything and records nothing.
    let out = p.ns_test(&["--no-cache", "--offline"]);
    assert!(out.status.success());
    assert!(!stderr(&out).contains("(cached)"));
    assert_eq!(p.counts(), (5, 3));

    // Test filters are part of the key.
    assert!(p.ns_test(&["--offline", "--", "greeting"]).status.success());
    assert_eq!(p.counts(), (7, 3));
}

#[test]
fn failures_are_never_cached() {
    let p = Proj::new("testcachefail");
    p.edit(
        "crates/greeter/src/lib.rs",
        "assert!(!super::greeting().is_empty());",
        "assert!(super::greeting().is_empty(), \"deliberate failure\");",
    );
    for _ in 0..2 {
        let out = p.ns_test(&["--offline", "-p", "greeter"]);
        assert!(!out.status.success());
        assert!(!stderr(&out).contains("(cached)"));
    }
    assert_eq!(p.counts(), (0, 0));
}

#[test]
fn a_project_runner_is_kept_and_its_tests_run_uncached() {
    let p = Proj::new("testcacherunner");
    let host = String::from_utf8(Command::new("rustc").arg("-vV").output().unwrap().stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_owned))
        .unwrap();
    std::fs::create_dir_all(p.dir.join(".cargo")).unwrap();
    // `env <binary> <args>` runs the test binary unchanged.
    std::fs::write(
        p.dir.join(".cargo/config.toml"),
        format!("[target.{host}]\nrunner = [\"env\", \"NS_RUNNER_USED=1\"]\n"),
    )
    .unwrap();
    for _ in 0..2 {
        let out = p.ns_test(&["--offline"]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("has its own target runner"),
            "{}",
            stderr(&out)
        );
        assert!(!stderr(&out).contains("(cached)"));
    }
    assert_eq!(p.counts(), (0, 0));
}
