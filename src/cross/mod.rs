//! Cross builds: zig as linker and C
//! compiler for Linux gnu/musl and windows-gnu, an xwin-style Windows SDK
//! for msvc, and `naspeed build` for universal2 and pinned glibc.

pub mod linker_shim;
pub mod msvc_sdk;
pub mod shim_install;

use std::process::Command;

use anyhow::{Context, Result};

/// Targets that go through a zig linker shim.
pub const ZIG_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-gnu",
];

pub const MSVC_TARGET: &str = "x86_64-pc-windows-msvc";

/// Both halves of `universal2-apple-darwin`.
pub const APPLE_TARGETS: &[&str] = &["aarch64-apple-darwin", "x86_64-apple-darwin"];

pub fn host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("run rustc -vV")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_owned)
        .context("no host line in rustc -vV")
}

pub fn installed_targets() -> Vec<String> {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `x86_64-unknown-linux-gnu` → `x86_64_unknown_linux_gnu` (cc-rs env suffix).
pub fn env_suffix(triple: &str) -> String {
    triple.replace(['-', '.'], "_")
}
