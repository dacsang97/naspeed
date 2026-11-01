//! Machine setup helpers shared by `setup`, `uninstall` and `status`.

pub mod cargo_config;
pub mod shell_env_check;

use std::path::{Path, PathBuf};

use crate::config::cargo_home;

/// Where `setup` installs the binary: the internal disk, so a missing USB
/// volume never leaves cargo pointing at a vanished wrapper.
pub fn installed_binary() -> PathBuf {
    cargo_home()
        .join("bin")
        .join(crate::platform::exe_name("naspeed"))
}

/// The `ns` front-end next to the installed binary: a relative symlink on
/// Unix, a hard link (or copy) `ns.exe` on Windows, where symlinks need
/// Developer Mode.
pub fn ns_alias() -> PathBuf {
    cargo_home()
        .join("bin")
        .join(crate::platform::exe_name(crate::cli::NS_ALIAS))
}

/// True when `path` is the alias `install_ns_alias` creates.
#[cfg(unix)]
pub fn is_our_ns_alias(path: &Path) -> bool {
    std::fs::read_link(path)
        .is_ok_and(|target| target.file_name() == installed_binary().file_name())
}

/// True when `path` is the alias `install_ns_alias` creates: a file with
/// exactly the installed binary's bytes.
#[cfg(windows)]
pub fn is_our_ns_alias(path: &Path) -> bool {
    let bin = installed_binary();
    let same_len = std::fs::metadata(path)
        .ok()
        .zip(std::fs::metadata(&bin).ok())
        .is_some_and(|(a, b)| a.len() == b.len());
    same_len && std::fs::read(path).ok() == std::fs::read(&bin).ok()
}

/// Creates the `ns` alias of the installed binary. An `ns` that is not ours
/// (another tool with the same name) is left alone, unless `was_ours`: the
/// caller saw it was our alias before replacing the binary (a Windows hard
/// link keeps the old bytes). Returns whether the alias is in place.
pub fn install_ns_alias(was_ours: bool) -> std::io::Result<bool> {
    let alias = ns_alias();
    if is_our_ns_alias(&alias) {
        return Ok(true);
    }
    if alias.symlink_metadata().is_ok() {
        if !was_ours {
            return Ok(false);
        }
        crate::platform::remove_exe(&alias)?;
    }
    let target = installed_binary();
    #[cfg(unix)]
    crate::platform::symlink(target.file_name().unwrap(), &alias)?;
    #[cfg(windows)]
    if std::fs::hard_link(&target, &alias).is_err() {
        std::fs::copy(&target, &alias)?;
    }
    Ok(true)
}

pub fn cargo_config_path() -> PathBuf {
    cargo_home().join("config.toml")
}
