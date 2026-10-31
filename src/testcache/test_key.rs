//! Key of one test-binary run: everything that can change its result as far
//! as naspeed can see — the binary, its arguments, the environment the tests
//! are known to read, and the files they could open (package trees, the
//! package's binaries, `Cargo.lock`, `[test].inputs`).
//!
//! Reads outside those inputs (network, `/etc`, the
//! clock) are not tracked; `ns test --no-cache` or `[test].cache = false`
//! opts out.

use std::path::Path;

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use super::test_plan::{PackageInputs, TestPlan};
use crate::store::content_hash::hash_file;
use crate::wrapper::Env;
use crate::wrapper::cache_key::KeyHasher;

pub const TEST_KEY_VERSION: &str = "naspeed-test-v1";

/// libtest flags whose runs are not test results.
const UNCACHEABLE_ARGS: &[&str] = &["--list", "--bench", "--help", "-h"];

pub fn cacheable_args(args: &[String]) -> bool {
    !args.iter().any(|a| UNCACHEABLE_ARGS.contains(&a.as_str()))
}

/// Env vars that join the key: package identity, libtest's own knobs and
/// whatever `[test].env` names.
fn key_env_names<'a>(env: &'a Env, extra: &'a [String]) -> Vec<&'a str> {
    let mut names: Vec<&str> = env
        .keys()
        .map(String::as_str)
        .filter(|n| {
            n.starts_with("CARGO_PKG_") || n.starts_with("RUST_TEST_") || *n == "RUST_MIN_STACK"
        })
        .chain(extra.iter().map(String::as_str))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Hash a file or a gitignore-aware directory tree (hidden files included,
/// `.git` and the target dir skipped). `None` when the path does not exist.
pub fn hash_input(path: &Path, target_dir: &Path) -> Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if !meta.is_dir() {
        return Ok(Some(hash_file(path)?.to_hex().to_string()));
    }
    let mut h = KeyHasher(blake3::Hasher::new());
    let target_dir = target_dir.to_path_buf();
    let walker = WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        // A symlinked fixture dir is hashed by content (ignore detects loops).
        .follow_links(true)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| e.file_name() != ".git" && !e.path().starts_with(&target_dir))
        .build();
    for entry in walker {
        // A broken link or a loop changes the key instead of disabling it.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                h.field("walk-error", &e.to_string());
                continue;
            }
        };
        let rel = entry.path().strip_prefix(path).unwrap_or(entry.path());
        let rel = rel.to_string_lossy();
        match entry.file_type() {
            Some(t) if t.is_file() => {
                h.field(&format!("f:{rel}"), &hash_file(entry.path())?.to_hex());
            }
            Some(t) if t.is_symlink() => {
                let target = std::fs::read_link(entry.path())?;
                h.field(&format!("l:{rel}"), &target.to_string_lossy());
            }
            _ => {}
        }
    }
    Ok(Some(h.finish()))
}

fn rel<'a>(path: &'a Path, root: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy()
}

pub struct RunContext<'a> {
    pub binary: &'a Path,
    pub args: &'a [String],
    pub env: &'a Env,
    pub cwd: &'a Path,
    pub manifest_dir: &'a Path,
}

pub fn key(plan: &TestPlan, package: &PackageInputs, run: &RunContext) -> Result<String> {
    let root = &plan.workspace_root;
    let mut h = KeyHasher(blake3::Hasher::new());
    h.field("version", TEST_KEY_VERSION);
    h.field(
        "binary",
        &hash_file(run.binary)
            .with_context(|| format!("hash {}", run.binary.display()))?
            .to_hex(),
    );
    for (i, arg) in run.args.iter().enumerate() {
        h.field(&format!("arg{i}"), arg);
    }
    h.field("package", &rel(run.manifest_dir, root));
    h.field("cwd", &rel(run.cwd, run.manifest_dir));
    for name in key_env_names(run.env, &plan.env) {
        h.field(
            &format!("env:{name}"),
            run.env.get(name).map(String::as_str).unwrap_or("\0unset"),
        );
    }
    for dir in &package.dirs {
        let hash = hash_input(dir, &plan.target_dir)?;
        h.field(
            &format!("dir:{}", rel(dir, root)),
            hash.as_deref().unwrap_or("\0missing"),
        );
    }
    for input in &plan.inputs {
        let hash = hash_input(input, &plan.target_dir)?;
        h.field(
            &format!("input:{}", rel(input, root)),
            hash.as_deref().unwrap_or("\0missing"),
        );
    }
    // `<profile>/deps/<test>-<hash>` → `<profile>/<bin>`.
    if let Some(profile) = run.binary.parent().and_then(Path::parent) {
        for bin in &package.bins {
            let exe = profile.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
            let hash = match std::fs::metadata(&exe) {
                Ok(_) => hash_file(&exe)?.to_hex().to_string(),
                Err(_) => "\0missing".into(),
            };
            h.field(&format!("bin:{bin}"), &hash);
        }
    }
    Ok(h.finish())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn setup() -> (tempfile::TempDir, TestPlan, PackageInputs, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let pkg = root.join("app");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        std::fs::write(pkg.join("src/lib.rs"), "fn a() {}").unwrap();
        std::fs::write(pkg.join(".gitignore"), "scratch/\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "lock").unwrap();
        let binary = root.join("target/debug/deps/app-1234");
        std::fs::write(&binary, "test binary").unwrap();
        std::fs::write(root.join("target/debug/app"), "bin v1").unwrap();
        let plan = TestPlan {
            store: root.join("store"),
            workspace_root: root.clone(),
            target_dir: root.join("target"),
            env: vec!["DATABASE_URL".into()],
            inputs: vec![root.join("Cargo.lock")],
            packages: Default::default(),
        };
        let package = PackageInputs {
            dirs: vec![pkg],
            bins: vec!["app".into()],
        };
        (dir, plan, package, binary)
    }

    fn key_with(plan: &TestPlan, package: &PackageInputs, binary: &Path, env: &Env) -> String {
        let manifest_dir = plan.workspace_root.join("app");
        key(
            plan,
            package,
            &RunContext {
                binary,
                args: &["--quiet".to_string()],
                env,
                cwd: &manifest_dir,
                manifest_dir: &manifest_dir,
            },
        )
        .unwrap()
    }

    #[test]
    fn key_tracks_sources_bins_env_and_ignores_volatile_things() {
        let (dir, plan, package, binary) = setup();
        let root = dir.path();
        let mut env = Env::new();
        env.insert("CARGO_PKG_NAME".into(), "app".into());
        env.insert("PWD".into(), "/somewhere".into());
        let base = key_with(&plan, &package, &binary, &env);

        // Volatile env and gitignored scratch files do not matter.
        env.insert("PWD".into(), "/elsewhere".into());
        std::fs::create_dir_all(root.join("app/scratch")).unwrap();
        std::fs::write(root.join("app/scratch/x"), "junk").unwrap();
        assert_eq!(key_with(&plan, &package, &binary, &env), base);

        // A declared env var does.
        env.insert("DATABASE_URL".into(), "postgres://x".into());
        let with_db = key_with(&plan, &package, &binary, &env);
        assert_ne!(with_db, base);

        // So do sources, fixtures, the package's binaries and Cargo.lock.
        std::fs::write(root.join("app/src/lib.rs"), "fn b() {}").unwrap();
        let edited = key_with(&plan, &package, &binary, &env);
        assert_ne!(edited, with_db);
        std::fs::write(root.join("target/debug/app"), "bin v2").unwrap();
        let rebuilt = key_with(&plan, &package, &binary, &env);
        assert_ne!(rebuilt, edited);
        std::fs::write(root.join("Cargo.lock"), "lock2").unwrap();
        assert_ne!(key_with(&plan, &package, &binary, &env), rebuilt);
    }

    #[test]
    fn list_and_bench_runs_are_not_cached() {
        assert!(cacheable_args(&["--quiet".into(), "my_filter".into()]));
        assert!(!cacheable_args(&["--list".into()]));
        assert!(!cacheable_args(&["--bench".into()]));
    }
}
