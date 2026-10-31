//! Release targets → how to build them and where the binaries land.
//!
//! * plain triples: `cargo build --target <T>` (cross linkers come from the
//!   cargo config written by `naspeed setup`);
//! * `universal2-apple-darwin` and `<triple>.<glibc>`: `naspeed build`
//!   (run as a child of the current executable), which lipo-merges or pins
//!   glibc through zig.
//!
//! Targets run in parallel lanes with a small bound; every build gets the
//! release profile, its features, `NS_*`, `SOURCE_DATE_EPOCH` and its own
//! env, and its binary is copied to the dist dir right after it is built.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use super::build_plan::ReleaseBuild;
use crate::commands::build::{profile_dir_name, split_glibc};
use crate::cross::APPLE_TARGETS;

pub const UNIVERSAL2: &str = "universal2-apple-darwin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildKind {
    Cargo,
    Universal2,
    PinnedGlibc { triple: String, glibc: String },
}

pub fn classify(target: &str) -> BuildKind {
    if target == UNIVERSAL2 {
        return BuildKind::Universal2;
    }
    match split_glibc(target) {
        Some((triple, glibc)) => BuildKind::PinnedGlibc {
            triple: triple.into(),
            glibc: glibc.into(),
        },
        None => BuildKind::Cargo,
    }
}

/// rustup targets a release target needs.
pub fn rustup_targets(target: &str) -> Vec<String> {
    match classify(target) {
        BuildKind::Cargo => vec![target.to_string()],
        BuildKind::Universal2 => APPLE_TARGETS.iter().map(|t| t.to_string()).collect(),
        BuildKind::PinnedGlibc { triple, .. } => vec![triple],
    }
}

/// Error naming every missing rustup target and the command to add them.
pub fn check_installed(targets: &[String], installed: &[String]) -> Result<()> {
    let mut missing: Vec<String> = targets
        .iter()
        .flat_map(|t| rustup_targets(t))
        .filter(|t| !installed.contains(t))
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        bail!(
            "rustup target(s) not installed: {}; run `rustup target add {}`",
            missing.join(", "),
            missing.join(" ")
        );
    }
    Ok(())
}

pub fn is_windows(target: &str) -> bool {
    target.contains("-windows-")
}

pub fn exe_name(bin: &str, target: &str) -> String {
    if is_windows(target) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    }
}

/// Directory holding the final binaries of `target` for `profile`.
pub fn output_dir(target_dir: &Path, target: &str, profile: &str) -> PathBuf {
    let profile_dir = profile_dir_name(&["--profile".into(), profile.into()]);
    match classify(target) {
        BuildKind::Cargo | BuildKind::Universal2 => target_dir.join(target).join(profile_dir),
        BuildKind::PinnedGlibc { triple, glibc } => target_dir
            .join(format!("glibc-{glibc}"))
            .join(triple)
            .join(profile_dir),
    }
}

/// Everything the matrix build needs.
pub struct MatrixInput<'a> {
    pub manifest_path: PathBuf,
    pub target_dir: &'a Path,
    pub profile: &'a str,
    pub builds: &'a [ReleaseBuild],
    /// `NS_*` and `SOURCE_DATE_EPOCH`, applied before each build's own env.
    pub base_env: &'a [(String, String)],
    pub dist_dir: &'a Path,
}

impl MatrixInput<'_> {
    fn cargo_args(&self, build: &ReleaseBuild) -> Vec<String> {
        let mut args = vec![
            "--manifest-path".to_string(),
            self.manifest_path.to_string_lossy().into_owned(),
            "--profile".to_string(),
            self.profile.to_string(),
            "-p".to_string(),
            build.package.clone(),
            "--bin".to_string(),
            build.config.bin.clone(),
        ];
        args.extend(build.feature_args());
        args.extend(trim_paths_config(&crate::config::cargo_home()));
        args
    }

    fn command(&self, build: &ReleaseBuild, target: &str) -> Result<Command> {
        let mut cmd = match classify(target) {
            BuildKind::Cargo => {
                let mut c =
                    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
                c.args(["build", "--target", target]);
                c
            }
            BuildKind::Universal2 | BuildKind::PinnedGlibc { .. } => {
                let exe = std::env::current_exe().context("locate the naspeed executable")?;
                let mut c = Command::new(exe);
                c.args(["build", "--target", target, "--"]);
                c
            }
        };
        cmd.args(self.cargo_args(build))
            .envs(self.base_env.iter().map(|(k, v)| (k, v)))
            .envs(&build.config.env);
        Ok(cmd)
    }

    /// Build `build` for `target`, then copy the binary to
    /// `<dist>/<target>/<binary>[.exe]` before anything can rebuild it.
    fn build_one(&self, build: &ReleaseBuild, target: &str) -> Result<()> {
        let status = self
            .command(build, target)?
            .status()
            .context("spawn build")?;
        if !status.success() {
            bail!("exit status {status}");
        }
        let built = output_dir(self.target_dir, target, self.profile)
            .join(exe_name(&build.config.bin, target));
        if !built.is_file() {
            bail!("build did not produce {}", built.display());
        }
        let dir = self.dist_dir.join(target);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let dest = dir.join(exe_name(build.binary(), target));
        std::fs::copy(&built, &dest).with_context(|| format!("copy {}", built.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", dest.display()))
    }
}

/// Path trimming: registry and git dependencies are compiled from
/// `$CARGO_HOME`, whose absolute path (and so the builder's user name)
/// would otherwise end up in every shipped binary's panic locations.
/// Workspace crates already get relative paths from cargo. Passed as a
/// `--config` array, which cargo merges with the user's `build.rustflags`.
fn trim_paths_config(cargo_home: &Path) -> Vec<String> {
    let remap = format!("--remap-path-prefix={}=/cargo", cargo_home.display());
    vec![
        "--config".to_string(),
        format!(
            "build.rustflags=[{}]",
            serde_json::to_string(&remap).unwrap_or_default()
        ),
    ]
}

/// Cargo output dirs (below the target dir) a release target writes.
fn output_keys(target: &str) -> Vec<String> {
    match classify(target) {
        BuildKind::Cargo => vec![target.to_string()],
        BuildKind::Universal2 => APPLE_TARGETS
            .iter()
            .map(|t| t.to_string())
            .chain([UNIVERSAL2.to_string()])
            .collect(),
        BuildKind::PinnedGlibc { triple, glibc } => vec![format!("glibc-{glibc}/{triple}")],
    }
}

/// Targets grouped so that targets sharing a cargo output dir (e.g.
/// `universal2-apple-darwin` and `aarch64-apple-darwin`) land in one lane:
/// a lane runs sequentially, lanes run in parallel.
pub fn lanes(targets: &[String]) -> Vec<Vec<String>> {
    let mut lanes: Vec<(BTreeSet<String>, Vec<String>)> = Vec::new();
    for t in targets {
        let mut keys: BTreeSet<String> = output_keys(t).into_iter().collect();
        let mut members = Vec::new();
        let mut i = 0;
        while i < lanes.len() {
            if lanes[i].0.is_disjoint(&keys) {
                i += 1;
            } else {
                let (k, m) = lanes.remove(i);
                keys.extend(k);
                members.extend(m);
            }
        }
        members.push(t.clone());
        lanes.push((keys, members));
    }
    lanes.into_iter().map(|(_, m)| m).collect()
}

/// Build every (build, target) pair, at most `jobs` lanes at a time. Within
/// a lane the builds run back to back and each output is copied out at
/// once, so variants of one bin (other features, same cargo output path)
/// never overwrite each other. All failures are reported.
pub fn build_all(input: &MatrixInput, targets: &[String], jobs: usize) -> Result<()> {
    let lanes = lanes(targets);
    let queue = Mutex::new(lanes.iter());
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, lanes.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().map(|mut q| q.next());
                    let Ok(Some(lane)) = next else { break };
                    for target in lane {
                        for build in input.builds.iter().filter(|b| b.targets.contains(target)) {
                            eprintln!("ns release: building {} for {target}", build.id());
                            if let Err(err) = input.build_one(build, target)
                                && let Ok(mut f) = failures.lock()
                            {
                                f.push(format!("{} {target}: {err:#}", build.id()));
                            }
                        }
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap_or_default();
    if !failures.is_empty() {
        bail!("build failed for {}", failures.join("; "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_builds_trim_the_cargo_home_path() {
        let args = trim_paths_config(Path::new("/home/dev/.cargo"));
        assert_eq!(args[0], "--config");
        assert_eq!(
            args[1],
            r#"build.rustflags=["--remap-path-prefix=/home/dev/.cargo=/cargo"]"#
        );
    }

    #[test]
    fn classification_and_paths() {
        assert_eq!(classify("x86_64-unknown-linux-musl"), BuildKind::Cargo);
        assert_eq!(classify(UNIVERSAL2), BuildKind::Universal2);
        assert_eq!(
            classify("x86_64-unknown-linux-gnu.2.17"),
            BuildKind::PinnedGlibc {
                triple: "x86_64-unknown-linux-gnu".into(),
                glibc: "2.17".into()
            }
        );
        let t = Path::new("/t");
        assert_eq!(
            output_dir(t, UNIVERSAL2, "release"),
            Path::new("/t/universal2-apple-darwin/release")
        );
        assert_eq!(
            output_dir(t, "x86_64-unknown-linux-gnu.2.17", "release"),
            Path::new("/t/glibc-2.17/x86_64-unknown-linux-gnu/release")
        );
        assert_eq!(
            output_dir(t, "x86_64-pc-windows-msvc", "dev"),
            Path::new("/t/x86_64-pc-windows-msvc/debug")
        );
        assert_eq!(exe_name("api", "x86_64-pc-windows-msvc"), "api.exe");
        assert_eq!(exe_name("api", UNIVERSAL2), "api");
    }

    #[test]
    fn targets_sharing_output_dirs_share_a_lane() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let lanes = lanes(&t(&[
            "x86_64-unknown-linux-musl",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu.2.17",
            UNIVERSAL2,
            "x86_64-unknown-linux-gnu",
        ]));
        assert_eq!(
            lanes,
            vec![
                t(&["x86_64-unknown-linux-musl"]),
                t(&["x86_64-unknown-linux-gnu.2.17"]),
                t(&["aarch64-apple-darwin", UNIVERSAL2]),
                t(&["x86_64-unknown-linux-gnu"]),
            ]
        );
    }

    #[test]
    fn missing_rustup_targets_are_named() {
        let installed = vec!["aarch64-apple-darwin".to_string()];
        let err = check_installed(
            &[UNIVERSAL2.into(), "x86_64-unknown-linux-musl".into()],
            &installed,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("x86_64-apple-darwin") && err.contains("x86_64-unknown-linux-musl"),
            "{err}"
        );
        assert!(err.contains("rustup target add"), "{err}");
        assert!(!err.contains("aarch64-apple-darwin"), "{err}");
    }
}
