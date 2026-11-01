// The choice of key inputs follows sccache `src/compiler/rust.rs`
// (`generate_hash_key`), Copyright 2016 Mozilla Foundation, Apache-2.0.
// Differences: source files are hashed per package dir instead of running
// rustc for dep-info, paths are replaced by placeholders so keys match
// across target dirs, and `--extern` artifacts use their stored identity.

//! Two-tier cache key.
//!
//! Tier 1 covers everything known before compiling. Tier 2 adds the current
//! values of the `# env-dep:` variables rustc reported the first time the
//! crate was built (stored in the tier-1 manifest).

use std::path::Path;

use anyhow::{Context, Result};

use super::Env;
use super::cacheability::{Cacheable, SourceKind};
use super::extern_identity;
use super::rustc_args::RustcInvocation;
use super::toolchain_fingerprint::Toolchain;
use crate::store::content_hash::{hash_dir, hash_dir_normalized, hash_file};
use crate::store::path_placeholders::{PROFILE_TOKEN, Placeholders};

/// Bump whenever the inputs or their encoding change.
pub const KEY_VERSION: &str = "naspeed-key-v4";

/// Arguments that only carry paths or presentation; their file inputs are
/// hashed separately.
const DROPPED_FLAGS: &[&str] = &[
    "--out-dir",
    "--diagnostic-width",
    "--color",
    "-L",
    "--extern",
];

/// `CARGO_*` variables that never change rustc output.
const DROPPED_CARGO_ENV: &[&str] = &[
    "CARGO_MAKEFLAGS",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
    "CARGO_HOME",
    // Cargo never passes -C incremental to registry crates.
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_INCREMENTAL",
];
const DROPPED_CARGO_ENV_PREFIXES: &[&str] = &[
    "CARGO_REGISTRIES_",
    "CARGO_REGISTRY_",
    "CARGO_HTTP_",
    "CARGO_NET_",
    "CARGO_TERM_",
];

/// Variables outside `CARGO_*` that change object files but do not appear
/// in `# env-dep:` (e.g. `MACOSX_DEPLOYMENT_TARGET` changes `minos`).
const EXTRA_ENV: &[&str] = &[
    "RUSTFLAGS",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "SDKROOT",
    "ZERO_AR_DATE",
];
// `CC*`/`CFLAGS*`/`AR*` are deliberately absent: rustc does not read them
// for libraries or proc-macros. C code reaches an rlib only through a build
// script's `OUT_DIR` (hashed in `tier1`) and the build-script run key.

/// File content lookups, behind a trait so key logic is testable without a
/// real registry.
pub trait Contents {
    fn source_tree(&self, root: &Path, kind: &SourceKind) -> Result<String>;
    fn extern_identity(&self, path: &Path) -> Result<String>;
    fn dir(&self, path: &Path) -> Result<String>;
    fn file(&self, path: &Path) -> Result<String>;
}

pub struct FsContents<'a> {
    /// `<store>/memo`; `None` disables the source-tree memo.
    pub memo_dir: Option<&'a Path>,
    /// Normalise target-dir paths inside text files when hashing dirs, so an
    /// `OUT_DIR` restored into another target dir hashes the same.
    pub placeholders: Option<&'a Placeholders>,
}

impl Contents for FsContents<'_> {
    fn source_tree(&self, root: &Path, kind: &SourceKind) -> Result<String> {
        // Registry and git checkouts are immutable once `.cargo-ok` exists,
        // so its size/mtime is a valid memo stamp.
        let marker = std::fs::metadata(root.join(".cargo-ok"))
            .ok()
            .and_then(|m| {
                let mtime = m
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?;
                Some(format!("{}:{}", m.len(), mtime.as_nanos()))
            });
        let memo = match (self.memo_dir, &marker) {
            (Some(dir), Some(_)) => Some(
                dir.join("src").join(
                    blake3::hash(root.as_os_str().as_encoded_bytes())
                        .to_hex()
                        .as_str(),
                ),
            ),
            _ => None,
        };
        if let (Some(memo), Some(marker)) = (&memo, &marker)
            && let Ok(text) = std::fs::read_to_string(memo)
            && let Some((stamp, hash)) = text.split_once(' ')
            && stamp == marker
        {
            return Ok(hash.trim().to_owned());
        }
        let skip: &[&str] = match kind {
            SourceKind::Registry => &[".cargo-ok", "target"],
            SourceKind::Git => &[".cargo-ok", ".git", "target"],
            // Local sources are keyed file by file in tier 2.
            SourceKind::Local => anyhow::bail!("local sources have no tree hash"),
        };
        let hash = hash_dir(root, skip)
            .with_context(|| format!("hash {}", root.display()))?
            .to_hex()
            .to_string();
        if let (Some(memo), Some(marker)) = (memo, marker)
            && let Some(parent) = memo.parent()
        {
            let _ = std::fs::create_dir_all(parent);
            let tmp = parent.join(format!(".tmp.{}", std::process::id()));
            if std::fs::write(&tmp, format!("{marker} {hash}")).is_ok() {
                let _ = std::fs::rename(&tmp, &memo);
            }
        }
        Ok(hash)
    }

    fn extern_identity(&self, path: &Path) -> Result<String> {
        extern_identity::identity(path).with_context(|| format!("identity {}", path.display()))
    }

    fn dir(&self, path: &Path) -> Result<String> {
        let hash = match self.placeholders {
            Some(ph) => hash_dir_normalized(path, &|text| ph.encode(text)),
            None => hash_dir(path, &[]),
        };
        Ok(hash
            .with_context(|| format!("hash {}", path.display()))?
            .to_hex()
            .to_string())
    }

    fn file(&self, path: &Path) -> Result<String> {
        Ok(hash_file(path)
            .with_context(|| format!("hash {}", path.display()))?
            .to_hex()
            .to_string())
    }
}

pub struct KeyContext<'a> {
    pub inv: &'a RustcInvocation,
    pub cacheable: &'a Cacheable,
    pub cwd: &'a Path,
    pub env: &'a Env,
    pub toolchain: &'a Toolchain,
    pub placeholders: &'a Placeholders,
}

/// Labelled fields separated so that no two field lists hash alike.
pub struct KeyHasher(pub blake3::Hasher);

impl KeyHasher {
    pub fn field(&mut self, label: &str, value: &str) {
        self.0.update(label.as_bytes());
        self.0.update(b"\x1f");
        self.0.update(value.as_bytes());
        self.0.update(b"\x1e");
    }

    pub fn finish(self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

fn keep_cargo_env(name: &str) -> bool {
    name.starts_with("CARGO_")
        && !DROPPED_CARGO_ENV.contains(&name)
        && !DROPPED_CARGO_ENV_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
}

fn keep_extra_env(name: &str) -> bool {
    EXTRA_ENV.contains(&name)
}

pub fn tier1(ctx: &KeyContext, contents: &dyn Contents) -> Result<String> {
    let ph = ctx.placeholders;
    let mut h = KeyHasher(blake3::Hasher::new());
    h.field("version", KEY_VERSION);
    h.field("rustc", &ctx.toolchain.version_verbose);
    // Content identity, not the install path: keys match across home dirs.
    h.field("driver", &ctx.toolchain.driver_hash);

    for token in &ctx.inv.tokens {
        if DROPPED_FLAGS.contains(&token.flag.as_str()) {
            continue;
        }
        // The incremental dir is a cache of rustc's own; outputs do not
        // depend on it (only local crates ever get one).
        if token.flag == "-C"
            && token
                .value
                .as_deref()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.starts_with("incremental="))
        {
            continue;
        }
        let value = token
            .value
            .as_deref()
            .map(|v| ph.encode(&v.to_string_lossy()))
            .unwrap_or_default();
        h.field(&format!("arg{}", token.flag), &value);
    }

    match ctx.cacheable.source_kind {
        // Mutable sources: tier 2 hashes the files rustc read last time.
        SourceKind::Local => h.field("source", "local"),
        ref kind => h.field(
            "source",
            &contents.source_tree(&ctx.cacheable.source_root, kind)?,
        ),
    }

    let mut externs = ctx.inv.externs.clone();
    externs.sort();
    for (name, path) in &externs {
        let identity = match path {
            Some(path) => contents.extern_identity(&ctx.cwd.join(path))?,
            None => String::from("-"),
        };
        h.field(&format!("extern:{name}"), &identity);
    }

    // Build script output: generated sources and native libs bundled into
    // the rlib. Only dirs inside this profile dir; system dirs are not ours.
    let mut native_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(out_dir) = ctx.env.get("OUT_DIR").map(|d| ctx.cwd.join(d))
        && out_dir.is_dir()
    {
        native_dirs.push(out_dir);
    }
    for (kind, path) in &ctx.inv.link_paths {
        let path = ctx.cwd.join(path);
        if matches!(kind.as_str(), "native" | "all" | "framework")
            && ph
                .encode(&path.to_string_lossy())
                .starts_with(PROFILE_TOKEN)
            && path.is_dir()
            && !native_dirs.contains(&path)
        {
            native_dirs.push(path);
        }
    }
    native_dirs.sort();
    for dir in &native_dirs {
        h.field(
            &format!("dir:{}", ph.encode(&dir.to_string_lossy())),
            &contents.dir(dir)?,
        );
    }
    for (kind, name) in &ctx.inv.link_libs {
        if kind != "static" {
            continue;
        }
        for (_, dir) in &ctx.inv.link_paths {
            let dir = ctx.cwd.join(dir);
            if native_dirs.contains(&dir) {
                continue;
            }
            for file in [
                format!("lib{name}.a"),
                format!("{name}.lib"),
                format!("{name}.a"),
            ] {
                let candidate = dir.join(&file);
                if candidate.is_file() {
                    h.field(&format!("staticlib:{name}"), &contents.file(&candidate)?);
                }
            }
        }
    }

    for (name, value) in ctx.env {
        if keep_cargo_env(name) || keep_extra_env(name) {
            h.field(&format!("env:{name}"), &ph.encode(value));
        }
    }

    Ok(h.0.finalize().to_hex().to_string())
}

/// Prefix of manifest entries that are source files (local crates).
pub const FILE_DEP: &str = "file:";

/// Tier 2: tier 1 plus the current (placeholder-encoded) value of every
/// variable the crate read through `env!`/`option_env!`, and for local
/// crates the current content of every source file rustc read last time
/// (`file:<path>`, relative to `cwd` or placeholder-encoded).
///
/// Direct mode: rustc's set of read files is a
/// function of the content of the files it read, so if every one of them
/// is unchanged the compile is unchanged; any edit that pulls in a new
/// file (`mod x;`, `include!`) changes a listed file and misses.
pub fn tier2(tier1: &str, deps: &[String], env: &Env, ph: &Placeholders, cwd: &Path) -> String {
    let mut deps: Vec<&String> = deps.iter().collect();
    deps.sort();
    deps.dedup();
    let mut h = KeyHasher(blake3::Hasher::new());
    h.field("version", KEY_VERSION);
    h.field("tier1", tier1);
    for dep in deps {
        if let Some(path) = dep.strip_prefix(FILE_DEP) {
            let content = hash_file(&cwd.join(ph.decode(path)))
                .map(|x| x.to_hex().to_string())
                .unwrap_or_else(|_| "\0missing".into());
            h.field(dep, &content);
            continue;
        }
        // Windows names are case-insensitive and `Env` keys are upper-cased
        // there, while rustc records the name as the source spelled it.
        let name = if cfg!(windows) {
            dep.to_uppercase()
        } else {
            dep.clone()
        };
        match env.get(name.as_str()) {
            Some(value) => h.field(&format!("env-dep:{dep}"), &ph.encode(value)),
            None => h.field(&format!("env-dep-unset:{dep}"), ""),
        }
    }
    h.0.finalize().to_hex().to_string()
}

/// Source files from the first rule of a `.d` file (`target: a.rs b.rs`),
/// with `\ ` unescaped. Paths are as rustc wrote them (relative to its cwd
/// or absolute).
pub fn dep_info_files(dep_info: &str) -> Vec<String> {
    let Some(line) = dep_info.lines().next() else {
        return Vec::new();
    };
    let Some((_, deps)) = line.split_once(": ") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = deps.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.sort();
    out.dedup();
    out
}

/// Variable names from `# env-dep:NAME[=value]` lines of a `.d` file.
pub fn env_dep_names(dep_info: &str) -> Vec<String> {
    let mut names: Vec<String> = dep_info
        .lines()
        .filter_map(|l| l.strip_prefix("# env-dep:"))
        .map(|rest| rest.split_once('=').map_or(rest, |(n, _)| n).to_owned())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::super::cacheability::{check, tests::fixture};
    use super::super::rustc_args::parse;
    use super::*;
    use std::path::PathBuf;

    /// Content is a function of the placeholder-encoded path, so the same
    /// crate in two target dirs sees identical inputs, as it would after
    /// naspeed restored its deps from one store object.
    struct FakeContents {
        ph: Placeholders,
        extern_salt: String,
    }

    impl Contents for FakeContents {
        fn source_tree(&self, root: &Path, _: &SourceKind) -> Result<String> {
            Ok(format!("src:{}", self.ph.encode(&root.to_string_lossy())))
        }
        fn extern_identity(&self, path: &Path) -> Result<String> {
            Ok(format!(
                "ext:{}{}",
                self.ph.encode(&path.to_string_lossy()),
                self.extern_salt
            ))
        }
        fn dir(&self, path: &Path) -> Result<String> {
            Ok(format!("dir:{}", self.ph.encode(&path.to_string_lossy())))
        }
        fn file(&self, path: &Path) -> Result<String> {
            Ok(format!("file:{}", path.display()))
        }
    }

    fn toolchain(name: &str) -> Toolchain {
        Toolchain {
            sysroot: PathBuf::from(format!("/home/dev/.rustup/toolchains/{name}")),
            version_verbose: "rustc 1.98.0 (88d9e12ae 2026-08-18)\nhost: aarch64-apple-darwin\n"
                .into(),
            driver_hash: blake3::hash(name.as_bytes()).to_hex().to_string(),
        }
    }

    fn key_with(
        name: &str,
        edit: impl FnOnce(&mut Vec<std::ffi::OsString>, &mut Env),
        tc: &str,
    ) -> String {
        let (mut args, mut env, cwd) = fixture(name);
        edit(&mut args, &mut env);
        let home = Path::new("/home/dev/.cargo");
        let inv = parse(&args);
        let c = check(Path::new("rustc"), &inv, &cwd, &env, home, false).expect("cacheable");
        let ph = Placeholders::new(&c.profile_dir, home);
        let contents = FakeContents {
            ph: ph.clone(),
            extern_salt: String::new(),
        };
        let ctx = KeyContext {
            inv: &inv,
            cacheable: &c,
            cwd: &cwd,
            env: &env,
            toolchain: &toolchain(tc),
            placeholders: &ph,
        };
        let t1 = tier1(&ctx, &contents).unwrap();
        // serde/serde_core read OUT_DIR through env!: tier 2 must ignore the
        // target-dir part of its value.
        tier2(&t1, &["OUT_DIR".to_owned()], &env, &ph, Path::new("/"))
    }

    fn key(name: &str) -> String {
        key_with(name, |_, _| {}, "stable-aarch64-apple-darwin")
    }

    #[test]
    fn same_crate_in_two_target_dirs_has_same_key() {
        for c in ["itoa", "serde", "tokio", "proc_macro2", "serde_derive"] {
            assert_eq!(key(&format!("{c}_ta")), key(&format!("{c}_tb")), "{c}");
        }
    }

    #[test]
    fn different_crates_differ() {
        assert_ne!(key("itoa_ta"), key("serde_ta"));
    }

    fn replace_arg(args: &mut [std::ffi::OsString], from: &str, to: &str) {
        let mut hit = false;
        for a in args.iter_mut() {
            if a.to_str() == Some(from) {
                *a = to.into();
                hit = true;
            }
        }
        assert!(hit, "fixture has no {from}");
    }

    #[test]
    fn inputs_that_change_output_change_key() {
        let base = key("serde_ta");
        let tc = "stable-aarch64-apple-darwin";
        let feature = key_with(
            "serde_ta",
            |a, _| replace_arg(a, "feature=\"std\"", "feature=\"alloc\""),
            tc,
        );
        let profile = key_with(
            "serde_ta",
            |a, _| replace_arg(a, "debuginfo=2", "debuginfo=0"),
            tc,
        );
        let target = key_with(
            "serde_ta",
            |a, _| {
                a.push("--target".into());
                a.push("x86_64-unknown-linux-gnu".into());
            },
            tc,
        );
        let rustflags = key_with(
            "serde_ta",
            |_, e| {
                e.insert(
                    "CARGO_ENCODED_RUSTFLAGS".into(),
                    "-Ctarget-cpu=native".into(),
                );
            },
            tc,
        );
        let macos = key_with(
            "serde_ta",
            |_, e| {
                e.insert("MACOSX_DEPLOYMENT_TARGET".into(), "11.0".into());
            },
            tc,
        );
        let toolchain = key_with("serde_ta", |_, _| {}, "1.99.0-aarch64-apple-darwin");
        let all = [
            &base, &feature, &profile, &target, &rustflags, &macos, &toolchain,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn jobserver_and_target_dir_env_do_not_change_key() {
        let tc = "stable-aarch64-apple-darwin";
        let base = key("itoa_ta");
        let changed = key_with(
            "itoa_ta",
            |_, e| {
                e.insert("CARGO_MAKEFLAGS".into(), "-j --jobserver-fds=9,10".into());
                e.insert("CARGO_TARGET_DIR".into(), "/elsewhere".into());
                e.insert("CARGO_BUILD_JOBS".into(), "7".into());
            },
            tc,
        );
        assert_eq!(base, changed);
    }

    #[test]
    fn extern_identity_changes_key() {
        let (args, env, cwd) = fixture("serde_derive_ta");
        let home = Path::new("/home/dev/.cargo");
        let inv = parse(&args);
        let c = check(Path::new("rustc"), &inv, &cwd, &env, home, false).unwrap();
        let ph = Placeholders::new(&c.profile_dir, home);
        let ctx = KeyContext {
            inv: &inv,
            cacheable: &c,
            cwd: &cwd,
            env: &env,
            toolchain: &toolchain("stable-aarch64-apple-darwin"),
            placeholders: &ph,
        };
        let a = tier1(
            &ctx,
            &FakeContents {
                ph: ph.clone(),
                extern_salt: "1".into(),
            },
        )
        .unwrap();
        let b = tier1(
            &ctx,
            &FakeContents {
                ph: ph.clone(),
                extern_salt: "2".into(),
            },
        )
        .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn env_dep_values_split_tier2() {
        let ph = Placeholders::new(Path::new("/w/t/debug"), Path::new("/h/.cargo"));
        let mut env = Env::new();
        env.insert("FOO".into(), "1".into());
        let a = tier2("t1", &["FOO".into()], &env, &ph, Path::new("/"));
        env.insert("FOO".into(), "2".into());
        let b = tier2("t1", &["FOO".into()], &env, &ph, Path::new("/"));
        env.remove("FOO");
        let c = tier2("t1", &["FOO".into()], &env, &ph, Path::new("/"));
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn dep_info_files_unescape_spaces() {
        let d = "/t/deps/x-1.d: src/lib.rs src/my\\ mod.rs /abs/gen.rs\n\nsrc/lib.rs:\n";
        assert_eq!(
            dep_info_files(d),
            vec!["/abs/gen.rs", "src/lib.rs", "src/my mod.rs"]
        );
    }

    #[test]
    fn tier2_tracks_listed_source_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}").unwrap();
        let ph = Placeholders::new(&dir.path().join("target/debug"), Path::new("/h/.cargo"))
            .with_workspace(dir.path());
        let deps = vec!["file:src/lib.rs".to_owned(), "file:src/gone.rs".to_owned()];
        let env = Env::new();
        let a = tier2("t1", &deps, &env, &ph, dir.path());
        assert_eq!(a, tier2("t1", &deps, &env, &ph, dir.path()));
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn b() {}").unwrap();
        let b = tier2("t1", &deps, &env, &ph, dir.path());
        assert_ne!(a, b);
        std::fs::write(dir.path().join("src/gone.rs"), "").unwrap();
        assert_ne!(b, tier2("t1", &deps, &env, &ph, dir.path()));
    }

    #[test]
    fn parses_env_dep_names() {
        let d = "x.rlib: a.rs\n\na.rs:\n\n# env-dep:OUT_DIR=/t/out\n# env-dep:CARGO_PKG_VERSION=1\n# env-dep:NO_VALUE\n";
        assert_eq!(
            env_dep_names(d),
            vec!["CARGO_PKG_VERSION", "NO_VALUE", "OUT_DIR"]
        );
    }
}
