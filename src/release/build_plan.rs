//! Which builds a release makes: `[[release.builds]]` (or one plain build per
//! `release.bins` entry), the package each comes from, its targets, and the
//! environment every build and `before` hook gets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::target_matrix::exe_name;
use crate::project::Project;
use crate::project::ns_config::BuildConfig;

/// One build variant, resolved against the workspace.
#[derive(Debug, Clone)]
pub struct ReleaseBuild {
    pub config: BuildConfig,
    /// Package owning `config.bin`.
    pub package: String,
    pub version: String,
    /// Release targets of this build, in matrix order.
    pub targets: Vec<String>,
}

impl ReleaseBuild {
    pub fn id(&self) -> &str {
        self.config.id()
    }

    pub fn binary(&self) -> &str {
        self.config.binary_name()
    }

    /// `--features` / `--no-default-features` as cargo arguments; plain
    /// feature names are qualified with the package so they also work from
    /// a virtual workspace root.
    pub fn feature_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.config.features.is_empty() {
            let features: Vec<String> = self
                .config
                .features
                .iter()
                .map(|f| match f.contains('/') {
                    true => f.clone(),
                    false => format!("{}/{f}", self.package),
                })
                .collect();
            args.extend(["--features".to_string(), features.join(",")]);
        }
        if self.config.no_default_features {
            args.push("--no-default-features".into());
        }
        args
    }
}

/// Builds from `ns.toml`; `cli_targets` (from `--target`) override every
/// build's targets. All builds must share one package version.
pub fn resolve(project: &Project, cli_targets: &[String]) -> Result<Vec<ReleaseBuild>> {
    let graph = &project.graph;
    let cfg = &project.config.release;
    let configs: Vec<BuildConfig> = if !cfg.builds.is_empty() {
        cfg.builds.clone()
    } else {
        let bins: Vec<String> = match cfg.bins.is_empty() {
            true => graph.bins().iter().map(|(_, b)| b.to_string()).collect(),
            false => cfg.bins.clone(),
        };
        bins.into_iter()
            .map(|bin| BuildConfig {
                id: None,
                bin,
                binary: None,
                features: Vec::new(),
                no_default_features: false,
                env: BTreeMap::new(),
                targets: Vec::new(),
            })
            .collect()
    };
    if configs.is_empty() {
        bail!("the workspace has no binaries to release");
    }
    let builds: Vec<ReleaseBuild> = configs
        .into_iter()
        .map(|config| {
            let owner = graph.bin_owner(&config.bin).with_context(|| {
                format!(
                    "release bin {:?} is not a binary of this workspace",
                    config.bin
                )
            })?;
            let targets = match (cli_targets.is_empty(), config.targets.is_empty()) {
                (false, _) => cli_targets.to_vec(),
                (true, false) => config.targets.clone(),
                (true, true) => cfg.targets.clone(),
            };
            if targets.is_empty() {
                bail!("build {:?} has no release targets", config.id());
            }
            Ok(ReleaseBuild {
                package: owner.name.clone(),
                version: owner.version.clone(),
                targets,
                config,
            })
        })
        .collect::<Result<_>>()?;
    check_unique(&builds)?;
    Ok(builds)
}

/// Ids are unique, binary names are valid file names, and no two builds
/// write the same `dist/<target>/<binary>`; all packages share a version.
fn check_unique(builds: &[ReleaseBuild]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for b in builds {
        if !ids.insert(b.id()) {
            bail!("release build id {:?} is used twice", b.id());
        }
        let name = b.binary();
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            bail!("build {:?}: invalid binary name {name:?}", b.id());
        }
        for t in &b.targets {
            if !outputs.insert((t.clone(), exe_name(name, t))) {
                bail!("two builds ship binary {name:?} for {t}; give one another `binary` name");
            }
        }
        if b.version != builds[0].version {
            bail!(
                "release binaries come from packages with different versions ({} is {}, {} is {})",
                b.package,
                b.version,
                builds[0].package,
                builds[0].version
            );
        }
    }
    Ok(())
}

/// Every target of every build, deduplicated, first-seen order.
pub fn all_targets(builds: &[ReleaseBuild]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    builds
        .iter()
        .flat_map(|b| b.targets.iter())
        .filter(|t| seen.insert(t.as_str()))
        .cloned()
        .collect()
}

/// `{project}`: the workspace root package if the root manifest is a
/// package, else the first build's binary.
pub fn project_name(project: &Project, builds: &[ReleaseBuild]) -> String {
    let graph = &project.graph;
    graph
        .members
        .iter()
        .find(|m| m.dir == graph.root)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| builds[0].binary().to_string())
}

/// `NS_VERSION`, `NS_TAG`, `NS_COMMIT`, `SOURCE_DATE_EPOCH`: set for every
/// build (before the build's own `env`) and every `before` hook.
pub fn base_env(
    version: &str,
    tag: Option<&str>,
    commit: Option<&str>,
    source_date_epoch: u64,
) -> Vec<(String, String)> {
    vec![
        (
            "NS_VERSION".into(),
            version.trim_start_matches('v').to_string(),
        ),
        ("NS_TAG".into(), tag.unwrap_or_default().to_string()),
        ("NS_COMMIT".into(), commit.unwrap_or_default().to_string()),
        ("SOURCE_DATE_EPOCH".into(), source_date_epoch.to_string()),
    ]
}

/// Run each `before` hook with `sh -c` in `root`; the first failure stops
/// the release. Errors name the command (from ns.toml) but not its env.
pub fn run_hooks(hooks: &[String], root: &Path, env: &[(String, String)]) -> Result<()> {
    for hook in hooks {
        println!("ns release: before hook: {hook}");
        let status = Command::new("sh")
            .arg("-c")
            .arg(hook)
            .current_dir(root)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .status()
            .with_context(|| format!("spawn before hook `{hook}`"))?;
        if !status.success() {
            bail!("before hook `{hook}` failed ({status}); release aborted");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(id: &str, bin: &str, binary: Option<&str>, targets: &[&str]) -> ReleaseBuild {
        ReleaseBuild {
            config: BuildConfig {
                id: Some(id.into()),
                bin: bin.into(),
                binary: binary.map(str::to_string),
                features: vec!["ui".into(), "dep/x".into()],
                no_default_features: true,
                env: BTreeMap::new(),
                targets: Vec::new(),
            },
            package: "pkg".into(),
            version: "1.0.0".into(),
            targets: targets.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn feature_args_are_package_qualified() {
        let b = build("a", "api", None, &["t"]);
        assert_eq!(
            b.feature_args(),
            ["--features", "pkg/ui,dep/x", "--no-default-features"]
        );
    }

    #[test]
    fn collisions_are_rejected() {
        let t = ["x86_64-unknown-linux-musl"];
        let ok = [
            build("a", "api", None, &t),
            build("b", "api", Some("api-noui"), &t),
        ];
        check_unique(&ok).unwrap();
        assert_eq!(all_targets(&ok), vec![t[0].to_string()]);
        let same_bin = [build("a", "api", None, &t), build("b", "api", None, &t)];
        assert!(
            check_unique(&same_bin)
                .unwrap_err()
                .to_string()
                .contains("binary")
        );
        let same_id = [
            build("a", "api", None, &t),
            build("a", "api", Some("x"), &t),
        ];
        assert!(
            check_unique(&same_id)
                .unwrap_err()
                .to_string()
                .contains("id")
        );
        let disjoint = [
            build("a", "api", None, &t),
            build("b", "api", None, &["aarch64-unknown-linux-musl"]),
        ];
        check_unique(&disjoint).unwrap();
    }

    #[test]
    fn env_and_hooks() {
        let env = base_env("1.2.0", Some("v1.2.0"), None, 7);
        assert_eq!(env[0], ("NS_VERSION".into(), "1.2.0".into()));
        assert_eq!(env[2], ("NS_COMMIT".into(), String::new()));
        let dir = tempfile::tempdir().unwrap();
        run_hooks(&["echo \"$NS_TAG\" > tag.txt".into()], dir.path(), &env).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tag.txt")).unwrap(),
            "v1.2.0\n"
        );
        let err = run_hooks(&["exit 3".into(), "touch never".into()], dir.path(), &env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`exit 3`"), "{err}");
        assert!(!dir.path().join("never").exists());
    }
}
