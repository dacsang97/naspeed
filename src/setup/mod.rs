//! Machine setup helpers shared by `setup`, `uninstall` and `status`.

pub mod cargo_config;
pub mod shell_env_check;

use std::path::PathBuf;

use crate::config::cargo_home;

/// Where `setup` installs the binary: the internal disk, so a missing USB
/// volume never leaves cargo pointing at a vanished wrapper.
pub fn installed_binary() -> PathBuf {
    cargo_home().join("bin").join("naspeed")
}

/// The `ns` front-end: a relative symlink to the installed binary.
pub fn ns_alias() -> PathBuf {
    cargo_home().join("bin").join(crate::cli::NS_ALIAS)
}

/// True when `path` is the symlink `install_ns_alias` creates.
pub fn is_our_ns_alias(path: &std::path::Path) -> bool {
    std::fs::read_link(path)
        .is_ok_and(|target| target.file_name() == installed_binary().file_name())
}

/// Creates `ns` → `naspeed` next to the installed binary. An `ns` that is not
/// ours (another tool with the same name) is left alone; returns whether the
/// alias is in place.
pub fn install_ns_alias() -> std::io::Result<bool> {
    let alias = ns_alias();
    if is_our_ns_alias(&alias) {
        return Ok(true);
    }
    if alias.symlink_metadata().is_ok() {
        return Ok(false);
    }
    let target = installed_binary();
    std::os::unix::fs::symlink(target.file_name().unwrap(), &alias)?;
    Ok(true)
}

pub fn cargo_config_path() -> PathBuf {
    cargo_home().join("config.toml")
}
