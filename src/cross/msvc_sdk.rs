//! x86_64-pc-windows-msvc from macOS/Linux: Microsoft
//! CRT + Windows SDK splatted by `xwin`, `lld-link` as linker, `clang-cl`
//! for C, `llvm-lib` as archiver. The SDK lives on the store volume (keeping
//! several GB off the system disk) and is only downloaded once the user
//! accepts the Microsoft license with `--accept-msvc-license`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::MSVC_TARGET;
use crate::setup::cargo_config::{Edit, key};

/// Free space needed before downloading (download cache + splat).
const MIN_FREE_BYTES: u64 = 3 * 1024 * 1024 * 1024;

pub struct LlvmTools {
    pub clang_cl: PathBuf,
    pub lld_link: PathBuf,
    pub llvm_lib: PathBuf,
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain(
            [
                // Homebrew ships lld-link in its own `lld` keg.
                "/opt/homebrew/opt/llvm/bin",
                "/opt/homebrew/opt/lld/bin",
                "/usr/local/opt/llvm/bin",
                "/usr/local/opt/lld/bin",
                "/usr/lib/llvm/bin",
            ]
            .iter()
            .map(PathBuf::from),
        )
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

pub fn find_llvm() -> Option<LlvmTools> {
    Some(LlvmTools {
        clang_cl: which("clang-cl")?,
        lld_link: which("lld-link")?,
        llvm_lib: which("llvm-lib")?,
    })
}

pub fn free_bytes(dir: &Path) -> Option<u64> {
    let st = rustix::fs::statvfs(dir).ok()?;
    Some(st.f_bavail * st.f_frsize)
}

pub fn sdk_dir(store_root: &Path) -> PathBuf {
    store_root.join("xwin")
}

/// Download (once) and return the splat dir.
pub fn ensure_sdk(store_root: &Path, accept_license: bool) -> Result<PathBuf> {
    let dir = sdk_dir(store_root);
    let splat = dir.join("splat");
    if splat.join("crt").is_dir() && splat.join("sdk").is_dir() {
        return Ok(splat);
    }
    if !accept_license {
        bail!(
            "the msvc target needs the Microsoft CRT/SDK; review https://go.microsoft.com/fwlink/?LinkId=2086102 and rerun with --accept-msvc-license"
        );
    }
    let xwin =
        which("xwin").context("xwin not found: install it with `cargo install xwin --locked`")?;
    std::fs::create_dir_all(&dir)?;
    if free_bytes(&dir).is_some_and(|f| f < MIN_FREE_BYTES) {
        bail!(
            "less than 3 GiB free on {}; not downloading the Windows SDK",
            dir.display()
        );
    }
    let status = Command::new(xwin)
        .arg("--accept-license")
        .arg("--cache-dir")
        .arg(dir.join("cache"))
        .args(["--arch", "x86_64", "splat", "--output"])
        .arg(&splat)
        .status()
        .context("run xwin splat")?;
    if !status.success() {
        bail!("xwin splat failed");
    }
    // The download cache is not needed after splatting.
    let _ = std::fs::remove_dir_all(dir.join("cache"));
    Ok(splat)
}

pub fn config_edits(splat: &Path, llvm: &LlvmTools) -> Vec<Edit> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let lib = |sub: &str| format!("-Lnative={}", s(&splat.join(sub)));
    let rustflags = toml_edit::Array::from_iter([
        lib("crt/lib/x86_64"),
        lib("sdk/lib/um/x86_64"),
        lib("sdk/lib/ucrt/x86_64"),
    ]);
    let includes = [
        "crt/include",
        "sdk/include/ucrt",
        "sdk/include/um",
        "sdk/include/shared",
    ]
    .iter()
    .map(|sub| format!("/imsvc{}", s(&splat.join(sub))))
    .collect::<Vec<_>>()
    .join(" ");
    let cflags = format!(
        "--target={MSVC_TARGET} -Wno-unused-command-line-argument -fuse-ld=lld-link {includes}"
    );
    let suffix = super::env_suffix(MSVC_TARGET);
    let quoted = |v: &str| toml_edit::Value::from(v).to_string();
    vec![
        Edit::Set(
            key(&["target", MSVC_TARGET, "linker"]),
            quoted(&s(&llvm.lld_link)),
        ),
        Edit::Set(
            key(&["target", MSVC_TARGET, "rustflags"]),
            rustflags.to_string(),
        ),
        Edit::Set(
            key(&["env", &format!("CC_{suffix}")]),
            quoted(&s(&llvm.clang_cl)),
        ),
        Edit::Set(
            key(&["env", &format!("CXX_{suffix}")]),
            quoted(&s(&llvm.clang_cl)),
        ),
        Edit::Set(
            key(&["env", &format!("AR_{suffix}")]),
            quoted(&s(&llvm.llvm_lib)),
        ),
        Edit::Set(key(&["env", &format!("CFLAGS_{suffix}")]), quoted(&cflags)),
        Edit::Set(
            key(&["env", &format!("CXXFLAGS_{suffix}")]),
            quoted(&cflags),
        ),
    ]
}
