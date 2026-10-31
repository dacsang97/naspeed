//! `setup --cross`: generate per-triple shim scripts in
//! `~/.config/naspeed/bin/` and the cargo config keys that point at them.
//! The host triple is skipped so native builds never go through zig.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use super::linker_shim::shim_script;
use super::{
    APPLE_TARGETS, MSVC_TARGET, ZIG_TARGETS, env_suffix, host_triple, installed_targets, msvc_sdk,
};
use crate::cli::SetupArgs;
use crate::setup::cargo_config::{Edit, key};

pub struct CrossPlan {
    pub edits: Vec<Edit>,
    pub shims: Vec<PathBuf>,
    pub notes: Vec<String>,
}

/// Write `<dir>/<triple>-<tool>` scripts; returns (cc, c++, ar) paths.
pub fn write_shims(dir: &Path, bin: &Path, triple: &str) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut paths = Vec::new();
    for tool in ["cc", "c++", "ar", "ranlib"] {
        let path = dir.join(format!("{triple}-{tool}"));
        let script = shim_script(bin, triple, tool);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
            std::fs::write(&path, script)?;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        paths.push(path);
    }
    Ok(paths)
}

pub fn zig_edits(triple: &str, shims: &[PathBuf]) -> Vec<Edit> {
    let quoted = |p: &Path| toml_edit::Value::from(p.to_string_lossy().as_ref()).to_string();
    let suffix = env_suffix(triple);
    vec![
        Edit::Set(key(&["target", triple, "linker"]), quoted(&shims[0])),
        Edit::Set(key(&["env", &format!("CC_{suffix}")]), quoted(&shims[0])),
        Edit::Set(key(&["env", &format!("CXX_{suffix}")]), quoted(&shims[1])),
        Edit::Set(key(&["env", &format!("AR_{suffix}")]), quoted(&shims[2])),
        Edit::Set(
            key(&["env", &format!("RANLIB_{suffix}")]),
            quoted(&shims[3]),
        ),
    ]
}

pub fn prepare(
    config_dir: &Path,
    store_root: &Path,
    bin: &Path,
    args: &SetupArgs,
) -> Result<CrossPlan> {
    let host = host_triple()?;
    let shim_dir = config_dir.join("bin");
    let mut plan = CrossPlan {
        edits: Vec::new(),
        shims: Vec::new(),
        notes: Vec::new(),
    };
    for triple in ZIG_TARGETS.iter().filter(|t| **t != host) {
        let shims = write_shims(&shim_dir, bin, triple)?;
        plan.edits.extend(zig_edits(triple, &shims));
        plan.shims.extend(shims);
    }

    if MSVC_TARGET != host {
        match (msvc_sdk::find_llvm(), msvc_sdk::ensure_sdk(store_root, args.accept_msvc_license)) {
            (Some(llvm), Ok(splat)) => plan.edits.extend(msvc_sdk::config_edits(&splat, &llvm)),
            (None, _) => plan.notes.push(
                "msvc: LLVM (clang-cl, lld-link, llvm-lib) not found; install it (macOS: `brew install llvm`) and rerun setup --cross".into(),
            ),
            (_, Err(e)) => plan.notes.push(format!("msvc: {e:#}")),
        }
    }

    let installed = installed_targets();
    let wanted: Vec<&str> = ZIG_TARGETS
        .iter()
        .chain(std::iter::once(&MSVC_TARGET))
        .chain(
            APPLE_TARGETS
                .iter()
                .filter(|_| host.ends_with("apple-darwin")),
        )
        .copied()
        .filter(|t| *t != host)
        .collect();
    let missing: Vec<&str> = wanted
        .into_iter()
        .filter(|t| !installed.iter().any(|i| i == t))
        .collect();
    if !missing.is_empty() {
        if args.install_targets {
            let ok = Command::new("rustup")
                .args(["target", "add"])
                .args(&missing)
                .status()
                .is_ok_and(|s| s.success());
            plan.notes.push(format!(
                "rustup target add {}: {}",
                missing.join(" "),
                if ok { "done" } else { "FAILED" }
            ));
        } else {
            plan.notes.push(format!(
                "missing targets; run `rustup target add {}` (or setup --cross --install-targets)",
                missing.join(" ")
            ));
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shims_call_back_into_naspeed() {
        let dir = tempfile::tempdir().unwrap();
        let shims = write_shims(
            dir.path(),
            Path::new("/home/dev/.cargo/bin/naspeed"),
            "x86_64-unknown-linux-musl",
        )
        .unwrap();
        let text = std::fs::read_to_string(&shims[0]).unwrap();
        assert!(
            text.contains("'/home/dev/.cargo/bin/naspeed' __link 'x86_64-unknown-linux-musl' 'cc'")
        );
        let edits = zig_edits("x86_64-unknown-linux-musl", &shims);
        assert!(
            matches!(&edits[1], Edit::Set(k, _) if k == &key(&["env", "CC_x86_64_unknown_linux_musl"]))
        );
    }
}
