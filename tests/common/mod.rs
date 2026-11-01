//! Shared sandbox for integration tests: real cargo + rustc + registry
//! crates through the naspeed wrapper, store and target dirs on the same
//! volume (under `CARGO_TARGET_TMPDIR`).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const NASPEED: &str = env!("CARGO_BIN_EXE_naspeed");

pub struct Sandbox {
    pub root: PathBuf,
    pub store: PathBuf,
    pub config: PathBuf,
    pub workspace: PathBuf,
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            if e.file_name() != "target" {
                copy_dir(&e.path(), &dst);
            }
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

impl Sandbox {
    pub fn new(name: &str) -> Sandbox {
        let root =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let config = root.join("config");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_workspace"),
            &workspace,
        );
        Sandbox {
            root,
            store,
            config,
            workspace,
        }
    }

    /// `cargo` with naspeed as wrapper and a clean env: the test harness's
    /// own CARGO_* variables must not leak into the inner build.
    pub fn cargo(&self, target: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new("cargo");
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if (k.starts_with("CARGO_") && k != "CARGO_HOME")
                || k.starts_with("NASPEED_")
                || k.starts_with("RUSTC_")
            {
                cmd.env_remove(k.as_ref());
            }
        }
        cmd.args(args)
            .arg("--offline")
            .current_dir(&self.workspace)
            .env("RUSTC_WRAPPER", NASPEED)
            .env("CARGO_TARGET_DIR", self.root.join(target))
            .env("NASPEED_CONFIG_DIR", &self.config)
            .env("NASPEED_STORE", &self.store)
            .env("NASPEED_MIN_FREE_MEM_MB", "0")
            // Tests read usage.log directly; a background gc would fold it.
            .env("NASPEED_NO_AUTO_GC", "1")
            // No detached rustc outliving a test's target dir; the warm-up
            // has its own test.
            .env("NASPEED_WARM_INCREMENTAL", "0");
        cmd
    }

    pub fn run(&self, target: &str, args: &[&str]) -> Output {
        let out = self.cargo(target, args).output().unwrap();
        assert!(
            out.status.success(),
            "cargo {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// (hits, misses) recorded in the store's usage log so far.
    pub fn usage(&self) -> (Vec<String>, Vec<String>) {
        let text = std::fs::read_to_string(self.store.join("usage.log")).unwrap_or_default();
        let mut hits = Vec::new();
        let mut misses = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.first() {
                Some(&"H") => hits.push(f[2].to_owned()),
                Some(&"M") => misses.push(f[2].to_owned()),
                _ => {}
            }
        }
        (hits, misses)
    }

    /// Run a naspeed subcommand against this sandbox's store and config.
    pub fn naspeed(&self, args: &[&str]) -> String {
        let out = Command::new(NASPEED)
            .env("NO_COLOR", "1")
            .args(args)
            .env("NASPEED_CONFIG_DIR", &self.config)
            .env("NASPEED_STORE", &self.store)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "naspeed {args:?} failed: {text}");
        text
    }

    pub fn binary(&self, target: &str) -> PathBuf {
        self.root
            .join(target)
            .join("debug")
            .join(format!("sample-app{}", std::env::consts::EXE_SUFFIX))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
