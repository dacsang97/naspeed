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

pub const TEST_KEY_VERSION: &str = "naspeed-test-v2";

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

/// The static key and its labelled components (hashed values only), kept
/// with a stored pass so `ns test --explain` can say what changed.
pub struct Key {
    pub key: String,
    pub components: Vec<(String, String)>,
}

pub fn key(plan: &TestPlan, package: &PackageInputs, run: &RunContext) -> Result<Key> {
    let root = &plan.workspace_root;
    let mut parts: Vec<(String, String)> = vec![("version".into(), TEST_KEY_VERSION.into())];
    parts.push((
        "binary".into(),
        hash_file(run.binary)
            .with_context(|| format!("hash {}", run.binary.display()))?
            .to_hex()
            .to_string(),
    ));
    parts.push((
        "args".into(),
        blake3::hash(run.args.join("\0").as_bytes())
            .to_hex()
            .to_string(),
    ));
    parts.push(("package".into(), rel(run.manifest_dir, root).into_owned()));
    parts.push(("cwd".into(), rel(run.cwd, run.manifest_dir).into_owned()));
    let names = key_env_names(run.env, &plan.env)
        .into_iter()
        .map(str::to_owned)
        .chain(package.env_names.iter().cloned())
        .collect::<std::collections::BTreeSet<String>>();
    for name in names {
        let value = run.env.get(&name).map(String::as_str).unwrap_or("\0unset");
        parts.push((
            format!("env:{name}"),
            blake3::hash(value.as_bytes()).to_hex().to_string(),
        ));
    }
    for dir in &package.dirs {
        let hash = hash_input(dir, &plan.target_dir)?;
        parts.push((
            format!("dir:{}", rel(dir, root)),
            hash.unwrap_or_else(|| "\0missing".into()),
        ));
    }
    for input in &plan.inputs {
        let hash = hash_input(input, &plan.target_dir)?;
        parts.push((
            format!("input:{}", rel(input, root)),
            hash.unwrap_or_else(|| "\0missing".into()),
        ));
    }
    // Any binary of the workspace, not only the package's: an integration
    // test may run another member's binary from the profile dir.
    // `<profile>/deps/<test>-<hash>` → `<profile>/<bin>`.
    if let Some(profile) = run.binary.parent().and_then(Path::parent) {
        let bins = if plan.all_bins.is_empty() {
            &package.bins
        } else {
            &plan.all_bins
        };
        for bin in bins {
            let exe = profile.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
            let hash = match std::fs::metadata(&exe) {
                Ok(_) => hash_file(&exe)?.to_hex().to_string(),
                Err(_) => "\0missing".into(),
            };
            parts.push((format!("bin:{bin}"), hash));
        }
    }
    let mut h = KeyHasher(blake3::Hasher::new());
    for (label, value) in &parts {
        h.field(label, value);
    }
    Ok(Key {
        key: h.finish(),
        components: parts,
    })
}

/// Files of the workspace outside every member dir (gitignore-aware,
/// without `.git`, the target dir and `[test].exclude`): what a test
/// may read from the repository beyond its packages. Hashed only for runs
/// the tracer could not follow.
pub fn workspace_files_hash(plan: &TestPlan) -> Result<String> {
    let root = &plan.workspace_root;
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for glob in &plan.exclude {
        overrides.add(&format!("!{glob}"))?;
    }
    let target = plan.target_dir.clone();
    let members = plan.member_dirs.clone();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .overrides(overrides.build()?)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            let p = e.path();
            e.file_name() != ".git"
                && !p.starts_with(&target)
                && !members.iter().any(|m| p.starts_with(m))
        })
        .build();
    let mut h = KeyHasher(blake3::Hasher::new());
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                h.field("walk-error", &e.to_string());
                continue;
            }
        };
        if entry.file_type().is_some_and(|t| t.is_file()) {
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            h.field(
                &format!("f:{}", rel.to_string_lossy()),
                &hash_file(entry.path())?.to_hex(),
            );
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
            all_bins: vec!["app".into(), "tool".into()],
            member_dirs: vec![pkg.clone()],
            exclude: vec!["docs/**".into()],
            trace: false,
        };
        let package = PackageInputs {
            dirs: vec![pkg],
            bins: vec!["app".into()],
            env_names: vec!["APP_MODE".into()],
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
        .key
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
        let relocked = key_with(&plan, &package, &binary, &env);
        assert_ne!(relocked, rebuilt);

        // A variable the sources read, and another member's binary.
        env.insert("APP_MODE".into(), "x".into());
        let moded = key_with(&plan, &package, &binary, &env);
        assert_ne!(moded, relocked);
        std::fs::write(root.join("target/debug/tool"), "tool v1").unwrap();
        assert_ne!(key_with(&plan, &package, &binary, &env), moded);
    }

    #[test]
    fn workspace_files_cover_the_root_but_not_members_or_exclusions() {
        let (dir, plan, _, _) = setup();
        let root = dir.path();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("shared/v.txt"), "1").unwrap();
        let base = workspace_files_hash(&plan).unwrap();
        std::fs::write(root.join("docs/guide.md"), "edited").unwrap();
        std::fs::write(root.join("app/src/lib.rs"), "member change").unwrap();
        std::fs::write(root.join("target/debug/app"), "build output").unwrap();
        assert_eq!(workspace_files_hash(&plan).unwrap(), base);
        std::fs::write(root.join("shared/v.txt"), "2").unwrap();
        assert_ne!(workspace_files_hash(&plan).unwrap(), base);
    }

    #[test]
    fn list_and_bench_runs_are_not_cached() {
        assert!(cacheable_args(&["--quiet".into(), "my_filter".into()]));
        assert!(!cacheable_args(&["--list".into()]));
        assert!(!cacheable_args(&["--bench".into()]));
    }
}
