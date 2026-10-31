//! `naspeed build --target <T>` for the two cases plain cargo cannot do:
//!
//! * `universal2-apple-darwin`: build aarch64 and x86_64, then `lipo`.
//! * `<triple>.<glibc>` (e.g. `x86_64-unknown-linux-gnu.2.17`): link against
//!   that glibc through zig, with every C-facing variable pointed at a shim
//!   for the pinned triple and a separate target dir per glibc version so
//!   build-script `OUT_DIR`s of the unpinned build are never reused.
//!
//! Everything else is `cargo build --target <T>` (transparent via setup).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::BuildArgs;
use crate::config::Settings;
use crate::cross::{APPLE_TARGETS, env_suffix, linker_shim, shim_install};
use crate::setup::installed_binary;

fn cargo_target_dir(cargo_args: &[String]) -> Result<PathBuf> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--offline",
    ]);
    if let Some(i) = cargo_args.iter().position(|a| a == "--manifest-path") {
        cmd.arg("--manifest-path").args(cargo_args.get(i + 1));
    }
    let out = cmd.output().context("cargo metadata")?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(PathBuf::from(
        v["target_directory"].as_str().context("target_directory")?,
    ))
}

pub fn profile_dir_name(cargo_args: &[String]) -> String {
    if cargo_args.iter().any(|a| a == "--release" || a == "-r") {
        return "release".into();
    }
    match cargo_args.iter().position(|a| a == "--profile") {
        Some(i) => match cargo_args.get(i + 1).map(String::as_str) {
            Some("dev") | Some("test") | None => "debug".into(),
            Some("bench") => "release".into(),
            Some(p) => p.into(),
        },
        None => "debug".into(),
    }
}

fn run_cargo(target: &str, cargo_args: &[String], envs: &[(String, String)]) -> Result<()> {
    let status = Command::new("cargo")
        .arg("build")
        .arg("--target")
        .arg(target)
        .args(cargo_args)
        .envs(envs.iter().map(|(k, v)| (k, v)))
        .status()
        .context("run cargo build")?;
    if !status.success() {
        bail!("cargo build --target {target} failed");
    }
    Ok(())
}

/// Mach-O executables and libraries directly in a profile dir.
fn final_artifacts(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_lib = name.ends_with(".dylib") || name.ends_with(".a");
            let is_exe = !name.contains('.')
                && std::os::unix::fs::PermissionsExt::mode(&e.metadata().ok()?.permissions())
                    & 0o111
                    != 0;
            (is_lib || is_exe).then_some(name)
        })
        .collect();
    names.sort();
    names
}

fn universal2(args: &BuildArgs) -> Result<u8> {
    for t in APPLE_TARGETS {
        run_cargo(t, &args.cargo_args, &[])?;
    }
    let target_dir = cargo_target_dir(&args.cargo_args)?;
    let profile = profile_dir_name(&args.cargo_args);
    let dirs: Vec<PathBuf> = APPLE_TARGETS
        .iter()
        .map(|t| target_dir.join(t).join(&profile))
        .collect();
    let out = target_dir.join("universal2-apple-darwin").join(&profile);
    std::fs::create_dir_all(&out)?;
    let mut made = 0;
    for name in final_artifacts(&dirs[0]) {
        let inputs: Vec<PathBuf> = dirs.iter().map(|d| d.join(&name)).collect();
        if !inputs.iter().all(|p| p.is_file()) {
            continue;
        }
        let status = Command::new("lipo")
            .arg("-create")
            .arg("-output")
            .arg(out.join(&name))
            .args(&inputs)
            .status()
            .context("run lipo")?;
        if !status.success() {
            bail!("lipo failed for {name}");
        }
        made += 1;
        crate::ui_info!("universal2 binary"; path = out.join(&name).display());
    }
    if made == 0 {
        bail!("no executables or libraries found in {}", dirs[0].display());
    }
    Ok(0)
}

/// `-isystem` dirs that `zig cc` uses for the pinned triple, for bindgen.
fn zig_include_args(cc_shim: &Path) -> Vec<String> {
    let Ok(out) = Command::new(cc_shim)
        .args(["-E", "-x", "c", "-", "-v"])
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stderr);
    let mut in_list = false;
    let mut args = Vec::new();
    for line in text.lines() {
        if line.starts_with("#include <...> search starts here:") {
            in_list = true;
        } else if line.starts_with("End of search list.") {
            break;
        } else if in_list {
            args.push(format!("-isystem{}", line.trim()));
        }
    }
    args
}

fn pinned_glibc(args: &BuildArgs, triple: &str, glibc: &str) -> Result<u8> {
    let settings = Settings::load();
    let pinned = format!("{triple}.{glibc}");
    // Fail early with a clear message if zig cannot serve the triple.
    linker_shim::zig_wrapper(&pinned)?;
    let shims = shim_install::write_shims(
        &settings.config_dir.join("bin"),
        &installed_or_current(),
        &pinned,
    )?;
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let suffix = env_suffix(triple);
    let target_dir = cargo_target_dir(&args.cargo_args)?.join(format!("glibc-{glibc}"));
    let mut envs = vec![
        (
            format!("CARGO_TARGET_{}_LINKER", suffix.to_uppercase()),
            s(&shims[0]),
        ),
        (format!("CC_{suffix}"), s(&shims[0])),
        (format!("CXX_{suffix}"), s(&shims[1])),
        (format!("AR_{suffix}"), s(&shims[2])),
        (format!("RANLIB_{suffix}"), s(&shims[3])),
        ("CARGO_TARGET_DIR".to_owned(), s(&target_dir)),
    ];
    let bindgen = zig_include_args(&shims[0]).join(" ");
    if !bindgen.is_empty() {
        envs.push((
            format!("BINDGEN_EXTRA_CLANG_ARGS_{suffix}"),
            bindgen.clone(),
        ));
        envs.push((format!("BINDGEN_EXTRA_CLANG_ARGS_{triple}"), bindgen));
    }
    if crate::cross::host_triple()? == triple {
        // Keep host build scripts on the native
        // linker while the target uses the pinned glibc.
        envs.push((
            "__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS".into(),
            "nightly".into(),
        ));
        envs.push((
            "CARGO_UNSTABLE_TARGET_APPLIES_TO_HOST".into(),
            "true".into(),
        ));
        envs.push(("CARGO_TARGET_APPLIES_TO_HOST".into(), "false".into()));
    }
    run_cargo(triple, &args.cargo_args, &envs)?;
    crate::ui_info!("built";
        glibc = glibc,
        artifacts = target_dir.join(triple).join(profile_dir_name(&args.cargo_args)).display());
    Ok(0)
}

fn installed_or_current() -> PathBuf {
    let bin = installed_binary();
    if bin.exists() {
        bin
    } else {
        std::env::current_exe().unwrap_or(bin)
    }
}

/// Split `x86_64-unknown-linux-gnu.2.17` into triple and glibc version.
pub fn split_glibc(target: &str) -> Option<(&str, &str)> {
    let (triple, version) = target.split_once('.')?;
    let ok = triple.contains("linux-gnu")
        && version.split('.').count() == 2
        && version
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then_some((triple, version))
}

pub fn run(args: BuildArgs) -> Result<u8> {
    if args.target == "universal2-apple-darwin" {
        return universal2(&args);
    }
    if let Some((triple, glibc)) = split_glibc(&args.target) {
        return pinned_glibc(&args, triple, glibc);
    }
    bail!(
        "naspeed build only handles universal2-apple-darwin and <triple>.<glibc>; use `cargo build --target {}` directly",
        args.target
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glibc_suffix() {
        assert_eq!(
            split_glibc("x86_64-unknown-linux-gnu.2.17"),
            Some(("x86_64-unknown-linux-gnu", "2.17"))
        );
        assert_eq!(split_glibc("x86_64-unknown-linux-musl"), None);
        assert_eq!(split_glibc("x86_64-unknown-linux-gnu.2"), None);
    }

    #[test]
    fn profile_dirs() {
        assert_eq!(profile_dir_name(&[]), "debug");
        assert_eq!(profile_dir_name(&["--release".into()]), "release");
        assert_eq!(
            profile_dir_name(&["--profile".into(), "dist".into()]),
            "dist"
        );
    }
}
