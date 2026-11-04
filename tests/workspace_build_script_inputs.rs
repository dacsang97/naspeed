//! A workspace crate whose proc-macro reads files rustc does not list
//! (`sqlx::migrate!` and `migrations/`). The crate's build script declares
//! them with `rerun-if-*`, so cargo recompiles it; the wrapper must not answer
//! that recompile with an artifact built from the old inputs.

use std::path::Path;
use std::process::Command;

mod common;

use common::Sandbox;

const WORKSPACE_MANIFEST: &str = r#"[workspace]
members = ["app", "storage", "embed"]
resolver = "2"
"#;
const EMBED_MANIFEST: &str = r#"[package]
name = "embed"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
proc-macro = true
"#;
const EMBED_LIB: &str = r#"
use proc_macro::TokenStream;

/// Same blind spot as `sqlx::migrate!`: both reads are invisible to rustc.
#[proc_macro]
pub fn summary(_: TokenStream) -> TokenStream {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let count = std::fs::read_dir(format!("{root}/migrations")).unwrap().count();
    let tag = std::env::var("EMBED_TAG").unwrap_or_default();
    format!("\"{count}:{tag}\"").parse().unwrap()
}
"#;
const STORAGE_MANIFEST: &str = r#"[package]
name = "storage"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
embed = { path = "../embed" }
"#;
const STORAGE_BUILD: &str = r#"
fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-env-changed=EMBED_TAG");
}
"#;
const APP_MANIFEST: &str = r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
storage = { path = "../storage" }
"#;

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn write_workspace(root: &Path) {
    write(
        root,
        &[
            ("Cargo.toml", WORKSPACE_MANIFEST),
            ("embed/Cargo.toml", EMBED_MANIFEST),
            ("embed/src/lib.rs", EMBED_LIB),
            ("storage/Cargo.toml", STORAGE_MANIFEST),
            ("storage/build.rs", STORAGE_BUILD),
            (
                "storage/src/lib.rs",
                "pub const SUMMARY: &str = embed::summary!();\n",
            ),
            (
                "storage/migrations/0001_init.sql",
                "create table a (id int);\n",
            ),
            ("app/Cargo.toml", APP_MANIFEST),
            (
                "app/src/main.rs",
                "fn main() { print!(\"{}\", storage::SUMMARY); }\n",
            ),
        ],
    );
}

struct Harness {
    sandbox: Sandbox,
    log: std::path::PathBuf,
}

impl Harness {
    /// Build `dir` into `target` and return what the binary printed.
    fn build_and_run(&self, dir: &Path, target: &str, tag: Option<&str>) -> String {
        let mut cargo = self.sandbox.cargo(target, &["build"]);
        cargo
            .current_dir(dir)
            .env("NASPEED_LOG", "naspeed=info")
            .env("NASPEED_LOG_FILE", &self.log);
        match tag {
            Some(tag) => cargo.env("EMBED_TAG", tag),
            None => cargo.env_remove("EMBED_TAG"),
        };
        let out = cargo.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bin = self.sandbox.root.join(target).join("debug/app");
        String::from_utf8(Command::new(bin).output().unwrap().stdout).unwrap()
    }

    fn storage_hits(&self) -> usize {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(": hit crate_name=storage"))
            .count()
    }
}

fn settle_mtime() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

#[test]
fn declared_build_script_inputs_are_part_of_the_key() {
    let sandbox = Sandbox::new("build-script-inputs");
    let ws = sandbox.root.join("checkout");
    write_workspace(&ws);
    let log = sandbox.root.join("naspeed.log");
    let h = Harness { sandbox, log };

    assert_eq!(h.build_and_run(&ws, "a", None), "1:");
    assert_eq!(h.storage_hits(), 0);

    // New file in the declared directory, no storage source change.
    settle_mtime();
    let added = ws.join("storage/migrations/0002_more.sql");
    std::fs::write(&added, "create table b (id int);\n").unwrap();
    assert_eq!(
        h.build_and_run(&ws, "a", None),
        "2:",
        "the added file must reach the binary"
    );
    assert_eq!(h.storage_hits(), 0, "changed inputs must not hit");

    // Same inputs as the first build: the cache still works.
    settle_mtime();
    std::fs::remove_file(&added).unwrap();
    assert_eq!(h.build_and_run(&ws, "a", None), "1:");
    assert_eq!(h.storage_hits(), 1, "the original inputs must hit");

    // A declared variable is an input too.
    assert_eq!(h.build_and_run(&ws, "a", Some("blue")), "1:blue");
    assert_eq!(h.storage_hits(), 1, "a changed variable must not hit");

    // Declared inputs must not tie the key to the checkout's path.
    let other = h.sandbox.root.join("checkout-two-at-another-path");
    write_workspace(&other);
    assert_eq!(h.build_and_run(&other, "b", None), "1:");
    assert_eq!(h.storage_hits(), 2, "another checkout must share the entry");
}
