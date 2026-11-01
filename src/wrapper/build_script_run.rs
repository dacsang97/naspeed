//! Cache the *run* of registry/git crates' build scripts (`OUT_DIR` plus
//! the `cargo:` directives they print). Scripts such as jemalloc-sys or
//! aws-lc-sys spend 30+ s compiling C in every fresh target dir.
//!
//! Cargo runs build scripts itself, not through `RUSTC_WRAPPER`. naspeed
//! therefore swaps the build-script binary it just compiled (or restored)
//! for a tiny shell launcher; the real binary sits next to it as
//! `<name>.naspeed-real`. Cargo uplifts and runs the launcher, which calls
//! `naspeed __build-script <real> <binary-hash> …`.
//!
//! The key follows cargo's own freshness model and is deliberately wide:
//! - tier 1: the script binary, the package sources, the toolchain, the
//!   host C toolchain (`cc --version`, tools on PATH), cargo's build-script
//!   env (`TARGET`, `PROFILE`, features, `CARGO_CFG_*`, `DEP_*`), C/C++
//!   and `*_SYS_*`-style configuration variables, and the content of every
//!   `DEP_*` directory inside this profile dir;
//! - tier 2: the current values of every `rerun-if-env-changed` variable
//!   and the content of every `rerun-if-changed` path the script declared
//!   the first time it ran.
//!
//! A hit empties `OUT_DIR`, restores the stored tree (clones; text files
//! that name the target dir are rewritten) and replays the output. A miss
//! also starts from an empty `OUT_DIR`, exactly like a fresh target dir.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::cache_key::{Contents, FsContents, KeyHasher};
use super::cacheability::source_root;
use super::execute::run_teed;
use super::{Env, current_env, exit_code, extern_identity, toolchain_fingerprint};
use crate::config::{Settings, cargo_home};
use crate::setup::installed_binary;
use crate::store::content_hash::{TEXT_LIMIT, hash_dir, hash_file};
use crate::store::path_placeholders::{PROFILE_TOKEN, Placeholders};
use crate::store::usage_log::Record;
use crate::store::{
    DepInfoFile, ENTRY_VERSION, Entry, Manifest, OutputFile, Store, cow_copy, now_secs, volume_root,
};

pub const KEY_VERSION: &str = "naspeed-build-script-v2";
pub const REAL_SUFFIX: &str = ".naspeed-real";
const LAUNCHER_MARKER: &str = "# naspeed build-script launcher";

/// Host tools whose identity changes what C-building scripts produce.
const HOST_TOOLS: &[&str] = &[
    "cc",
    "c++",
    "clang",
    "gcc",
    "ar",
    "ranlib",
    "cmake",
    "make",
    "ninja",
    "nasm",
    "perl",
    "pkg-config",
    "autoconf",
    "go",
];

const DROPPED_ENV: &[&str] = &[
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "MFLAGS",
    "NUM_JOBS",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
    "CARGO_HOME",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_INCREMENTAL",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC",
    "RUSTDOC",
    "OUT_DIR",
];
const DROPPED_ENV_PREFIXES: &[&str] = &[
    "CARGO_TERM_",
    "CARGO_HTTP_",
    "CARGO_NET_",
    "CARGO_REGISTRIES_",
    "CARGO_REGISTRY_",
];
const KEPT_ENV: &[&str] = &[
    "TARGET",
    "HOST",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "RUSTC_LINKER",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "SDKROOT",
    "DEVELOPER_DIR",
    "CROSS_COMPILE",
    "LIBCLANG_PATH",
    "CLANG_PATH",
];
const KEPT_ENV_PREFIXES: &[&str] = &[
    "CARGO_",
    "DEP_",
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "AR",
    "RANLIB",
    "LD_",
    "PKG_CONFIG",
    "CMAKE",
    "BINDGEN_",
];
/// Other variables a script reads (`OPENSSL_DIR`, `ZSTD_SYS_USE_PKG_CONFIG`,
/// …) are covered by tier 2 through the script's `rerun-if-env-changed`
/// declarations, which is also all cargo itself tracks.
fn keep_env(name: &str) -> bool {
    if DROPPED_ENV.contains(&name) || DROPPED_ENV_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    KEPT_ENV.contains(&name) || KEPT_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn real_path(binary: &Path) -> PathBuf {
    let mut name = binary.file_name().unwrap_or_default().to_os_string();
    name.push(REAL_SUFFIX);
    binary.with_file_name(name)
}

/// The wrapper cargo is using right now (normally `~/.cargo/bin/naspeed`),
/// so the launcher and the compile always agree on the store and format.
fn naspeed_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| installed_binary())
}

/// Move the real build-script binary aside and put the launcher in place.
pub fn install_launcher(binary: &Path, binary_hash: &str) -> Result<()> {
    let real = real_path(binary);
    std::fs::rename(binary, &real).with_context(|| format!("rename {}", binary.display()))?;
    let naspeed = naspeed_exe();
    let script = format!(
        "#!/bin/sh\n{LAUNCHER_MARKER}\nn={n}\nif [ -x \"$n\" ]; then exec \"$n\" __build-script {r} {h} \"$@\"; fi\nexec {r} \"$@\"\n",
        n = shell_quote(&naspeed.to_string_lossy()),
        r = shell_quote(&real.to_string_lossy()),
        h = shell_quote(binary_hash),
    );
    std::fs::write(binary, script)?;
    crate::platform::set_mode(binary, 0o755)?;
    let _ = extern_identity::mark_placed(binary);
    Ok(())
}

pub fn is_launcher(path: &Path) -> bool {
    std::fs::read(path)
        .map(|b| b.len() < 4096 && String::from_utf8_lossy(&b).contains(LAUNCHER_MARKER))
        .unwrap_or(false)
}

fn exec_real(real: &Path, args: &[OsString]) -> i32 {
    match Command::new(real).args(args).status() {
        Ok(status) => exit_code(status),
        Err(e) => {
            eprintln!(
                "naspeed: failed to run build script {}: {e}",
                real.display()
            );
            1
        }
    }
}

/// `naspeed __build-script <real> <binary-hash> args…`
pub fn run(real: &Path, binary_hash: &str, args: &[OsString]) -> i32 {
    let settings = Settings::load();
    if settings.disabled || !settings.build_script_cache {
        return exec_real(real, args);
    }
    match Run::prepare(&settings, real, binary_hash) {
        Some(run) => run.execute(args),
        None => exec_real(real, args),
    }
}

struct Run<'a> {
    real: &'a Path,
    env: Env,
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    store: Store,
    placeholders: Placeholders,
    tier1: String,
    cow_mode: crate::config::CowMode,
}

/// `<profile>/build/<unit>/out` → `<profile>`
fn profile_of(out_dir: &Path) -> Option<PathBuf> {
    let unit = out_dir.parent()?;
    let build = unit.parent()?;
    (out_dir.file_name()? == "out" && build.file_name()? == "build")
        .then(|| build.parent().map(Path::to_path_buf))
        .flatten()
}

fn tool_identity(name: &str, env: &Env) -> String {
    let Some(path) = env.get("PATH").and_then(|p| {
        std::env::split_paths(p)
            .map(|d| d.join(name))
            .find(|c| c.is_file())
    }) else {
        return "-".into();
    };
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    let meta = std::fs::metadata(&canonical).ok();
    format!(
        "{}:{}:{:?}",
        canonical.display(),
        meta.as_ref().map(|m| m.len()).unwrap_or(0),
        meta.and_then(|m| m.modified().ok())
    )
}

fn cc_version(env: &Env) -> String {
    let cc = env.get("CC").cloned().unwrap_or_else(|| "cc".into());
    Command::new(cc)
        .arg("--version")
        .env_clear()
        .envs(env)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `rerun-if-env-changed` names and `rerun-if-changed` paths from the
/// script's stdout, stored in the manifest as `env:NAME` / `file:PATH`.
pub fn rerun_inputs(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let rest = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"));
        let Some(rest) = rest else { continue };
        if let Some(v) = rest.strip_prefix("rerun-if-env-changed=") {
            out.push(format!("env:{}", v.trim()));
        } else if let Some(v) = rest.strip_prefix("rerun-if-changed=") {
            out.push(format!("file:{}", v.trim()));
        }
    }
    out.sort();
    out.dedup();
    out
}

impl<'a> Run<'a> {
    fn prepare(settings: &Settings, real: &'a Path, binary_hash: &str) -> Option<Run<'a>> {
        let env = current_env();
        let cwd = std::env::current_dir().ok()?;
        let out_dir = PathBuf::from(env.get("OUT_DIR")?);
        let manifest_dir = PathBuf::from(env.get("CARGO_MANIFEST_DIR")?);
        let cargo_home = env
            .get("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(cargo_home);
        let (source_kind, source_root) =
            source_root(&manifest_dir.join("Cargo.toml"), &cargo_home)?;
        let profile_dir = profile_of(&out_dir)?;
        let store_root = volume_root::store_for(&out_dir, &settings.stores)?;
        let store = Store::open(&store_root, settings.cow_mode).ok()?;
        let toolchain = toolchain_fingerprint::resolve(
            Path::new(env.get("RUSTC")?),
            &cwd,
            &env,
            &store.memo_dir(),
        )
        .inspect_err(|e| warn!("build script toolchain: {e:#}"))
        .ok()?;
        let placeholders = Placeholders::new(&profile_dir, &cargo_home);
        let memo = store.memo_dir();
        let contents = FsContents {
            memo_dir: Some(&memo),
            placeholders: Some(&placeholders),
        };

        let mut h = KeyHasher(blake3::Hasher::new());
        h.field("version", KEY_VERSION);
        h.field("binary", binary_hash);
        h.field("rustc", &toolchain.version_verbose);
        h.field("driver", &toolchain.driver_hash);
        h.field(
            "source",
            &contents.source_tree(&source_root, &source_kind).ok()?,
        );
        h.field("cc", &cc_version(&env));
        for tool in HOST_TOOLS {
            h.field(&format!("tool:{tool}"), &tool_identity(tool, &env));
        }
        let mut dep_dirs = Vec::new();
        for (name, value) in &env {
            if !keep_env(name) {
                continue;
            }
            let encoded = placeholders.encode(value);
            h.field(&format!("env:{name}"), &encoded);
            // Headers and libs a dependency's build script left in its
            // OUT_DIR (DEP_Z_INCLUDE, DEP_OPENSSL_ROOT, …).
            if name.starts_with("DEP_") && encoded.starts_with(PROFILE_TOKEN) {
                let p = PathBuf::from(value);
                if p.is_dir() && !dep_dirs.contains(&p) {
                    dep_dirs.push(p);
                }
            }
        }
        dep_dirs.sort();
        for dir in &dep_dirs {
            h.field(
                &format!("dep-dir:{}", placeholders.encode(&dir.to_string_lossy())),
                &contents.dir(dir).ok()?,
            );
        }
        let tier1 = h.finish();
        store.append_usage(&Record::Target { profile_dir });
        Some(Run {
            real,
            env,
            manifest_dir,
            out_dir,
            store,
            placeholders,
            tier1,
            cow_mode: settings.cow_mode,
        })
    }

    fn tier2(&self, inputs: &[String]) -> String {
        let mut h = KeyHasher(blake3::Hasher::new());
        h.field("version", KEY_VERSION);
        h.field("tier1", &self.tier1);
        for input in inputs {
            if let Some(name) = input.strip_prefix("env:") {
                let value = self.env.get(name).map(|v| self.placeholders.encode(v));
                h.field(input, value.as_deref().unwrap_or("\0unset"));
            } else if let Some(path) = input.strip_prefix("file:") {
                let path = self.manifest_dir.join(self.placeholders.decode(path));
                let content = if path.is_dir() {
                    hash_dir(&path, &[]).map(|x| x.to_hex().to_string())
                } else {
                    hash_file(&path).map(|x| x.to_hex().to_string())
                };
                h.field(input, content.as_deref().unwrap_or("\0missing"));
            }
        }
        h.finish()
    }

    fn execute(self, args: &[OsString]) -> i32 {
        let Ok(_shared) = self.store.shared_lock() else {
            return exec_real(self.real, args);
        };
        if let Some(code) = self.try_hit() {
            return code;
        }
        let Ok(_key_lock) = self.store.key_lock(&self.tier1) else {
            return exec_real(self.real, args);
        };
        if let Some(code) = self.try_hit() {
            return code;
        }
        if let Some(code) = self.try_peer() {
            return code;
        }
        self.run_and_store(args)
    }

    fn try_peer(&self) -> Option<i32> {
        let settings = Settings::load();
        let client = crate::remote::peer_client::PeerClient::new(&self.store, &settings)?;
        client.fetch(&self.tier1, &|manifest| self.tier2(&manifest.env_deps))?;
        self.try_hit()
    }

    fn try_hit(&self) -> Option<i32> {
        let manifest = self.store.read_manifest(&self.tier1)?;
        let key = self.tier2(&manifest.env_deps);
        let entry = self.store.read_entry(&key)?;
        let restore_started = std::time::Instant::now();
        if let Err(e) = self.restore(&entry) {
            warn!(out_dir = %self.out_dir.display(), "build script restore failed, running it: {e:#}");
            return None;
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(self.placeholders.decode(&entry.stdout).as_bytes());
        let _ = out.flush();
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(self.placeholders.decode(&entry.stderr).as_bytes());
        let restore_ms = restore_started.elapsed().as_millis() as u64;
        self.store.append_usage(&Record::Hit {
            key,
            saved_ms: entry.compile_ms.saturating_sub(restore_ms),
        });
        info!(crate_name = %entry.crate_name, "build script hit");
        Some(0)
    }

    fn restore(&self, entry: &Entry) -> Result<()> {
        for o in &entry.outputs {
            let ok = std::fs::metadata(self.store.object_path(&o.object))
                .is_ok_and(|m| m.len() == o.size);
            if !ok {
                bail!("object {} missing", o.object);
            }
        }
        empty_dir(&self.out_dir)?;
        for dir in &entry.dirs {
            std::fs::create_dir_all(self.out_dir.join(dir))?;
        }
        for o in &entry.outputs {
            let dst = self.out_dir.join(&o.name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            cow_copy::clone_or_copy(&self.store.object_path(&o.object), &dst, self.cow_mode)?;
            if self.cow_mode != crate::config::CowMode::Hardlink {
                crate::platform::set_mode(&dst, o.mode | 0o200)?;
                filetime::set_file_mtime(&dst, filetime::FileTime::now())?;
            }
        }
        for t in &entry.texts {
            let dst = self.out_dir.join(&t.name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dst, self.placeholders.decode(&t.text))?;
        }
        for (name, target) in &entry.symlinks {
            let dst = self.out_dir.join(name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::platform::symlink(self.placeholders.decode(target), &dst)?;
        }
        Ok(())
    }

    fn run_and_store(&self, args: &[OsString]) -> i32 {
        // Same starting point as a fresh target dir: no leftovers (possibly
        // restored from another target dir) for the script to build on.
        let _ = empty_dir(&self.out_dir);
        let started = std::time::Instant::now();
        let output = match run_teed(self.real, args) {
            Ok(o) => o,
            Err(e) => {
                eprintln!(
                    "naspeed: failed to run build script {}: {e:#}",
                    self.real.display()
                );
                return 1;
            }
        };
        let code = exit_code(output.status);
        if code != 0 {
            return code;
        }
        let (Some(stdout), Some(stderr)) = (&output.stdout, &output.stderr) else {
            return code;
        };
        let run_ms = started.elapsed().as_millis() as u64;
        match self.store_run(stdout, stderr, run_ms) {
            Ok(key) => {
                info!(%key, out_dir = %self.out_dir.display(), "build script stored");
                crate::gc::auto::maybe_spawn(&self.store.root);
            }
            Err(e) => warn!(out_dir = %self.out_dir.display(), "build script not stored: {e:#}"),
        }
        code
    }

    fn store_run(&self, stdout: &str, stderr: &str, run_ms: u64) -> Result<String> {
        let inputs = rerun_inputs(stdout)
            .into_iter()
            .map(|i| match i.strip_prefix("file:") {
                Some(p) => format!("file:{}", self.placeholders.encode(p)),
                None => i,
            })
            .collect::<Vec<_>>();
        let mut entry = Entry {
            version: ENTRY_VERSION,
            key: String::new(),
            tier1: self.tier1.clone(),
            crate_name: self
                .env
                .get("CARGO_PKG_NAME")
                .map(|n| format!("{n} (build script run)"))
                .unwrap_or_default(),
            outputs: Vec::new(),
            dep_info: None,
            stdout: self.placeholders.encode(stdout),
            stderr: self.placeholders.encode(stderr),
            created: now_secs(),
            texts: Vec::new(),
            symlinks: Vec::new(),
            dirs: Vec::new(),
            compile_ms: run_ms,
        };
        self.snapshot(&self.out_dir, &mut entry)?;
        let key = self.tier2(&inputs);
        entry.key = key.clone();
        self.store
            .write_manifest(&self.tier1, &Manifest { env_deps: inputs })?;
        self.store.write_entry(&entry)?;
        self.store.append_usage(&Record::Miss {
            key: key.clone(),
            crate_name: entry.crate_name.clone(),
            size: entry.size(),
            compile_ms: run_ms,
        });
        Ok(key)
    }

    fn snapshot(&self, dir: &Path, entry: &mut Entry) -> Result<()> {
        let mut children: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let path = child.path();
            let rel = path
                .strip_prefix(&self.out_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path)?;
                entry
                    .symlinks
                    .push((rel, self.placeholders.encode(&target.to_string_lossy())));
            } else if meta.is_dir() {
                entry.dirs.push(rel);
                self.snapshot(&path, entry)?;
            } else if meta.is_file() {
                if meta.len() <= TEXT_LIMIT
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    let encoded = self.placeholders.encode(&text);
                    if encoded != text {
                        entry.texts.push(DepInfoFile {
                            name: rel,
                            text: encoded,
                        });
                        continue;
                    }
                }
                let (object, size) = self.store.ingest_object(&path)?;
                entry.outputs.push(OutputFile {
                    name: rel,
                    object,
                    size,
                    mode: crate::platform::mode_of(&meta),
                });
            }
        }
        Ok(())
    }
}

fn empty_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for child in std::fs::read_dir(dir)? {
        let path = child?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rerun_directives_both_syntaxes() {
        let out = "cargo:rustc-link-lib=z\ncargo:rerun-if-env-changed=ZLIB_STATIC\ncargo::rerun-if-changed=build.rs\ncargo:rerun-if-changed=src/zlib\nnoise\n";
        assert_eq!(
            rerun_inputs(out),
            vec!["env:ZLIB_STATIC", "file:build.rs", "file:src/zlib"]
        );
    }

    #[test]
    fn env_filter_keeps_build_config_and_drops_volatile() {
        for k in [
            "TARGET",
            "CARGO_FEATURE_STD",
            "CARGO_CFG_TARGET_OS",
            "DEP_Z_INCLUDE",
            "CC_x86_64_unknown_linux_gnu",
            "CFLAGS",
            "MACOSX_DEPLOYMENT_TARGET",
        ] {
            assert!(keep_env(k), "{k}");
        }
        for k in [
            "CARGO_MAKEFLAGS",
            "NUM_JOBS",
            "PATH",
            "HOME",
            "TERM",
            "OUT_DIR",
            "RUSTC_WRAPPER",
            "CK_REPORTS_DIR",
            "SSH_AUTH_SOCK",
        ] {
            assert!(!keep_env(k), "{k}");
        }
    }

    #[test]
    fn launcher_replaces_binary_and_points_at_real() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("build_script_build-0123456789abcdef");
        std::fs::write(&bin, b"\x7fELF real").unwrap();
        install_launcher(&bin, "abc").unwrap();
        assert!(is_launcher(&bin));
        assert_eq!(std::fs::read(real_path(&bin)).unwrap(), b"\x7fELF real");
        let script = std::fs::read_to_string(&bin).unwrap();
        assert!(script.contains("__build-script"));
        assert!(script.contains(&*real_path(&bin).to_string_lossy()));
    }

    #[test]
    fn profile_dir_from_out_dir() {
        assert_eq!(
            profile_of(Path::new("/t/debug/build/zstd-sys-0123/out")),
            Some(PathBuf::from("/t/debug"))
        );
        assert_eq!(profile_of(Path::new("/t/debug/deps/out")), None);
    }
}
