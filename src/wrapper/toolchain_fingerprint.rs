//! Identify the real toolchain behind `rustc`.
//!
//! Cargo passes the bare name `rustc`, which usually resolves to the rustup
//! proxy in `~/.cargo/bin`; its mtime says nothing about the toolchain that
//! will run. We resolve the sysroot (from `RUSTUP_TOOLCHAIN` when the proxy
//! is in use, else `rustc --print sysroot`).
//!
//! The identity that goes into cache keys is `rustc -vV` plus the blake3 of
//! the `librustc_driver-*` dylib content. Neither depends on where the
//! toolchain is installed (`$HOME`, `RUSTUP_HOME`), so a store exported from
//! one CI runner hits on the next one even when the home dir differs. The
//! content hash catches a locally rebuilt or patched compiler that reports
//! the same version. Hashing the dylib (~80-150 MB) costs well under a
//! second, and is paid once per install: both values are memoised in the
//! store by sysroot path plus inode/size/mtime of the dylib.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};

use super::Env;
use crate::store::content_hash::hash_file;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    /// Where the toolchain lives on this machine; never part of a key.
    pub sysroot: PathBuf,
    pub version_verbose: String,
    /// blake3 (hex) of the `librustc_driver-*` content.
    pub driver_hash: String,
}

/// Memo subdir; the file holds `<driver_hash>\n<rustc -vV output>`.
const MEMO_SUBDIR: &str = "toolchain-ids";

fn find_in_path(name: &str, env: &Env) -> Option<PathBuf> {
    let path = env.get("PATH")?;
    let names = [name.to_owned(), crate::platform::exe_name(name)];
    std::env::split_paths(path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|candidate| candidate.is_file())
}

/// Resolve `program` (bare name or path) to the executable that will run.
pub fn resolve_program(program: &Path, cwd: &Path, env: &Env) -> Option<PathBuf> {
    if program.components().count() == 1 {
        find_in_path(program.to_str()?, env)
    } else {
        Some(cwd.join(program))
    }
}

/// rustup's proxies are symlinks to `rustup` on Unix, and hardlinks (or
/// copies) of `rustup.exe` next to it on Windows.
fn is_rustup_proxy(resolved: &Path) -> bool {
    let canonical = crate::platform::canonicalize(resolved).ok();
    if canonical
        .as_deref()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|n| n.starts_with("rustup"))
    {
        return true;
    }
    cfg!(windows)
        && resolved
            .parent()
            .map(|dir| dir.join(crate::platform::exe_name("rustup")))
            .and_then(|rustup| std::fs::metadata(rustup).ok())
            .zip(std::fs::metadata(resolved).ok())
            .is_some_and(|(proxy, exe)| proxy.len() == exe.len())
}

fn rustup_toolchain_dir(env: &Env) -> Option<PathBuf> {
    let toolchain = env.get("RUSTUP_TOOLCHAIN")?;
    let candidate = PathBuf::from(toolchain);
    if candidate.is_absolute() {
        return candidate.is_dir().then_some(candidate);
    }
    let home = env
        .get("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home_dir().join(".rustup"));
    let dir = home.join("toolchains").join(toolchain);
    dir.is_dir().then_some(dir)
}

pub fn sysroot(program: &Path, cwd: &Path, env: &Env) -> Result<PathBuf> {
    let resolved = resolve_program(program, cwd, env);
    match resolved.as_deref() {
        Some(exe) if is_rustup_proxy(exe) => {
            if let Some(dir) = rustup_toolchain_dir(env) {
                return Ok(dir);
            }
        }
        // A real rustc binary: <sysroot>/bin/rustc.
        Some(exe) => {
            if let Some(root) = crate::platform::canonicalize(exe)
                .ok()
                .and_then(|p| p.parent()?.parent().map(Path::to_path_buf))
                && root.join("lib").is_dir()
            {
                return Ok(root);
            }
        }
        None => {}
    }
    let out = Command::new(program)
        .arg("--print")
        .arg("sysroot")
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .output()
        .context("run rustc --print sysroot")?;
    if !out.status.success() {
        bail!("rustc --print sysroot failed");
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// Where the driver library lives: `lib/librustc_driver-*.{so,dylib}` on
/// Unix, `bin/rustc_driver-*.dll` on Windows.
const DRIVER_DIR: &str = if cfg!(windows) { "bin" } else { "lib" };
const DRIVER_PREFIX: &str = if cfg!(windows) {
    "rustc_driver-"
} else {
    "librustc_driver-"
};

/// The driver dylib and a cheap stamp of it (name, inode, size, mtime).
fn driver_stamp(sysroot: &Path) -> Result<(PathBuf, String)> {
    let lib = sysroot.join(DRIVER_DIR);
    for entry in std::fs::read_dir(&lib).with_context(|| format!("read {}", lib.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(DRIVER_PREFIX) {
            let meta = entry.metadata()?;
            let mtime = meta
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let ino = crate::platform::file_index(&meta);
            return Ok((entry.path(), format!("{name}:{ino}:{}:{mtime}", meta.len())));
        }
    }
    bail!("no {DRIVER_PREFIX}* in {}", lib.display())
}

fn parse_memo(text: &str) -> Option<(String, String)> {
    let (hash, version) = text.split_once('\n')?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !version.is_empty())
        .then(|| (hash.to_owned(), version.to_owned()))
}

pub fn resolve(program: &Path, cwd: &Path, env: &Env, memo_dir: &Path) -> Result<Toolchain> {
    let sysroot = sysroot(program, cwd, env)?;
    let (driver, stamp) = driver_stamp(&sysroot)?;
    let memo_key = blake3::hash(format!("{}\0{stamp}", sysroot.display()).as_bytes()).to_hex();
    let memo = memo_dir.join(MEMO_SUBDIR).join(memo_key.as_str());
    if let Some((driver_hash, version_verbose)) = std::fs::read_to_string(&memo)
        .ok()
        .as_deref()
        .and_then(parse_memo)
    {
        return Ok(Toolchain {
            sysroot,
            version_verbose,
            driver_hash,
        });
    }
    let rustc = sysroot.join("bin").join(crate::platform::exe_name("rustc"));
    let out = Command::new(&rustc)
        .arg("-vV")
        .output()
        .with_context(|| format!("run {} -vV", rustc.display()))?;
    if !out.status.success() {
        bail!("{} -vV failed", rustc.display());
    }
    let version_verbose = String::from_utf8_lossy(&out.stdout).into_owned();
    let driver_hash = hash_file(&driver)
        .with_context(|| format!("hash {}", driver.display()))?
        .to_hex()
        .to_string();
    if let Some(parent) = memo.parent() {
        let _ = std::fs::create_dir_all(parent);
        let tmp = parent.join(format!(".{}.{}", memo_key, std::process::id()));
        if std::fs::write(&tmp, format!("{driver_hash}\n{version_verbose}")).is_ok() {
            let _ = std::fs::rename(&tmp, &memo);
        }
    }
    Ok(Toolchain {
        sysroot,
        version_verbose,
        driver_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memo_round_trip_and_rejects_garbage() {
        let hash = "ab".repeat(32);
        let text = format!("{hash}\nrustc 1.98.0\nhost: x\n");
        assert_eq!(
            parse_memo(&text),
            Some((hash, "rustc 1.98.0\nhost: x\n".to_owned()))
        );
        // Old-format memo (version text only) must not be trusted.
        assert_eq!(parse_memo("rustc 1.98.0\nhost: x\n"), None);
        assert_eq!(parse_memo(""), None);
    }

    /// The identity of the running toolchain is independent of its install
    /// path: a copy of the driver in another dir hashes the same.
    #[test]
    fn identity_ignores_install_path() {
        let env = crate::wrapper::current_env();
        let cwd = std::env::current_dir().unwrap();
        let memo = tempfile::tempdir().unwrap();
        let a = resolve(Path::new("rustc"), &cwd, &env, memo.path()).unwrap();
        assert_eq!(a.driver_hash.len(), 64);
        let (driver, _) = driver_stamp(&a.sysroot).unwrap();
        let copy = tempfile::tempdir().unwrap();
        let moved = copy.path().join(driver.file_name().unwrap());
        std::fs::copy(&driver, &moved).unwrap();
        assert_eq!(
            hash_file(&moved).unwrap().to_hex().to_string(),
            a.driver_hash
        );
        // Second call is served from the memo with the same identity.
        let b = resolve(Path::new("rustc"), &cwd, &env, memo.path()).unwrap();
        assert_eq!(a, b);
    }
}
