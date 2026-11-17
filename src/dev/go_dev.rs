//! Go project graph, input classification, and staged binary builds for `ns dev`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use super::ansi_strip::strip_ansi;
use super::go_list::{self, GoContext, ListedPackage};
use super::process_supervisor::{GroupChild, STOP_GRACE};
use crate::project::NsConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoInputKind {
    Source,
    Test,
    Manifest,
    Ignored,
    Other,
}

#[derive(Debug, Clone)]
struct GoPackage {
    import_path: String,
    dir: PathBuf,
    deps: BTreeSet<String>,
    test_deps: BTreeSet<String>,
    build_files: BTreeSet<PathBuf>,
    embed_patterns: GlobSet,
    has_cgo: bool,
    test_files: BTreeSet<PathBuf>,
}

impl GoPackage {
    fn matches_unlisted_build_input(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.dir) else {
            return false;
        };
        if self.embed_patterns.is_match(relative) {
            return true;
        }
        self.has_cgo
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    [
                        "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "m", "mm", "f", "for", "f90",
                        "s", "syso",
                    ]
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                })
    }
}

fn compile_embed_patterns(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for original in patterns {
        let pattern = original.strip_prefix("all:").unwrap_or(original);
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .backslash_escape(false)
            .build()
            .with_context(|| format!("invalid go:embed pattern {original:?}"))?;
        builder.add(glob);
        let descendants = GlobBuilder::new(&format!("{pattern}/**"))
            .literal_separator(true)
            .backslash_escape(false)
            .build()
            .with_context(|| format!("invalid go:embed pattern {original:?}"))?;
        builder.add(descendants);
    }
    builder.build().context("compile go:embed patterns")
}

#[derive(Debug, Clone)]
pub struct GoGraph {
    root: PathBuf,
    packages: Vec<GoPackage>,
    modules: BTreeSet<PathBuf>,
    manifests: BTreeSet<PathBuf>,
    bins: BTreeMap<String, Vec<String>>,
}

impl GoGraph {
    fn from_list(root: PathBuf, listed: Vec<ListedPackage>, workspace: bool) -> Result<GoGraph> {
        let mut packages = Vec::new();
        let mut modules = BTreeSet::new();
        let mut manifests = BTreeSet::new();
        let mut bins: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if workspace {
            manifests.insert(root.join("go.work"));
            manifests.insert(root.join("go.work.sum"));
        }
        for package in listed.into_iter().filter(|package| !package.standard) {
            if package.import_path.is_empty() {
                continue;
            }
            if let Some(module) = &package.module {
                let dir = &module.dir;
                modules.insert(dir.clone());
                manifests.insert(dir.join("go.mod"));
                manifests.insert(dir.join("go.sum"));
            }
            let build_files = package
                .go_files
                .iter()
                .chain(&package.cgo_files)
                .chain(&package.c_files)
                .chain(&package.h_files)
                .chain(&package.s_files)
                .chain(&package.cxx_files)
                .chain(&package.m_files)
                .chain(&package.f_files)
                .chain(&package.syso_files)
                .chain(&package.embed_files)
                .map(|file| package.dir.join(file))
                .collect();
            let test_files = package
                .test_go_files
                .iter()
                .chain(&package.x_test_go_files)
                .map(|file| package.dir.join(file))
                .collect();
            let embed_patterns = compile_embed_patterns(&package.embed_patterns)?;
            let has_cgo = !package.cgo_files.is_empty();
            if package.name == "main" {
                let name = package
                    .import_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&package.import_path)
                    .to_string();
                bins.entry(name)
                    .or_default()
                    .push(package.import_path.clone());
            }
            packages.push(GoPackage {
                import_path: package.import_path,
                dir: package.dir,
                deps: package.deps.iter().cloned().collect(),
                test_deps: package
                    .deps
                    .into_iter()
                    .chain(package.test_imports)
                    .chain(package.x_test_imports)
                    .collect(),
                build_files,
                test_files,
                embed_patterns,
                has_cgo,
            });
        }
        if modules.is_empty() {
            modules.insert(root.clone());
            manifests.insert(root.join("go.mod"));
            manifests.insert(root.join("go.sum"));
        }
        Ok(GoGraph {
            root,
            packages,
            modules,
            manifests,
            bins,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bin_names(&self) -> Vec<String> {
        self.bins.keys().cloned().collect()
    }

    pub fn bin_owner(&self, name: &str, package: Option<&str>) -> Result<String> {
        let owners = self
            .bins
            .get(name)
            .with_context(|| format!("no Go main package named {name:?}"))?;
        if let Some(package) = package {
            if owners.iter().any(|owner| owner == package) {
                return Ok(package.to_string());
            }
            bail!("[dev.apps.{name}].package {package:?} is not a main package named {name:?}");
        }
        match owners.as_slice() {
            [owner] => Ok(owner.clone()),
            many => bail!(
                "Go binary name {name:?} is shared by {}; set [dev.apps.{name}].package to one import path",
                many.join(", ")
            ),
        }
    }

    pub fn owner_for_path(&self, path: &Path) -> Option<&str> {
        self.packages
            .iter()
            .filter(|package| path.starts_with(&package.dir))
            .max_by_key(|package| package.dir.components().count())
            .map(|package| package.import_path.as_str())
    }

    pub fn with_dependents<'a, I>(&self, owners: I) -> BTreeSet<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        self.expand_dependents(owners, false)
    }

    pub fn with_test_dependents<'a, I>(&self, owners: I) -> BTreeSet<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        self.expand_dependents(owners, true)
    }

    fn expand_dependents<'a, I>(&self, owners: I, include_test: bool) -> BTreeSet<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut out: BTreeSet<String> = owners.into_iter().map(str::to_string).collect();
        loop {
            let mut changed = false;
            for package in &self.packages {
                let dependencies = if include_test {
                    &package.test_deps
                } else {
                    &package.deps
                };
                if !out.contains(&package.import_path)
                    && dependencies
                        .iter()
                        .any(|dependency| out.contains(dependency))
                {
                    changed |= out.insert(package.import_path.clone());
                }
            }
            if !changed {
                return out;
            }
        }
    }

    pub fn all_packages(&self) -> BTreeSet<String> {
        self.packages
            .iter()
            .map(|package| package.import_path.clone())
            .collect()
    }

    pub fn input_kind(&self, path: &Path) -> GoInputKind {
        if self.in_ignored_nested_module(path) {
            return GoInputKind::Ignored;
        }
        if self.manifests.contains(path) {
            return GoInputKind::Manifest;
        }
        if self
            .packages
            .iter()
            .any(|package| package.test_files.contains(path))
            || path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("_test.go"))
        {
            return GoInputKind::Test;
        }
        if path.extension().is_some_and(|extension| extension == "go") && !path.exists() {
            return GoInputKind::Manifest;
        }
        if self
            .packages
            .iter()
            .any(|package| package.build_files.contains(path))
        {
            return GoInputKind::Source;
        }
        if path.extension().is_some_and(|extension| extension == "go") {
            return if self.owner_for_path(path).is_some() {
                GoInputKind::Source
            } else {
                GoInputKind::Manifest
            };
        }
        if self
            .packages
            .iter()
            .filter(|package| path.starts_with(&package.dir))
            .max_by_key(|package| package.dir.components().count())
            .is_some_and(|package| package.matches_unlisted_build_input(path))
        {
            return GoInputKind::Source;
        }
        GoInputKind::Other
    }

    fn in_ignored_nested_module(&self, path: &Path) -> bool {
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == self.root || !dir.starts_with(&self.root) {
                return false;
            }
            if dir.join("go.mod").is_file() && !self.modules.contains(dir) {
                return true;
            }
            current = dir.parent();
        }
        false
    }

    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        self.modules
            .iter()
            .filter_map(|dir| dir.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .collect()
    }

    pub fn watch_files(&self) -> Vec<PathBuf> {
        self.manifests
            .iter()
            .filter_map(|path| path.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .collect()
    }
}

pub struct GoDev {
    context: GoContext,
    pub graph: GoGraph,
    pub config: NsConfig,
}

impl GoDev {
    pub fn load(dir: &Path) -> Result<GoDev> {
        let context = go_list::detect(dir)?;
        let graph = GoGraph::from_list(
            context.root.clone(),
            go_list::load(&context)?,
            context.workspace,
        )?;
        let config = NsConfig::load(&context.root)?;
        Ok(GoDev {
            context,
            graph,
            config,
        })
    }

    pub fn reload(&mut self) -> Result<()> {
        self.graph = GoGraph::from_list(
            self.context.root.clone(),
            go_list::load(&self.context)?,
            self.context.workspace,
        )?;
        Ok(())
    }

    pub fn output_dir(&self) -> PathBuf {
        self.graph.root.join(".ns-dev")
    }

    pub fn build(
        &self,
        targets: &[(String, String)],
        mut tick: impl FnMut() -> bool,
    ) -> Result<GoBuildOutcome> {
        let staging = self.output_dir().join("bin/.staging");
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("create {}", staging.display()))?;
        let mut artifacts = BTreeMap::new();
        for (name, owner) in targets {
            let artifact = staging.join(executable_name(name));
            let _ = std::fs::remove_file(&artifact);
            let target = self.build_target(owner, &artifact, &mut tick)?;
            if target.interrupted {
                return Ok(GoBuildOutcome {
                    interrupted: true,
                    ..GoBuildOutcome::default()
                });
            }
            if !target.success {
                let errors = if target.errors.is_empty() {
                    vec![format!("go build failed for {owner}")]
                } else {
                    vec![target.errors.join("\n")]
                };
                return Ok(GoBuildOutcome {
                    errors,
                    ..GoBuildOutcome::default()
                });
            }
            artifacts.insert(name.clone(), artifact);
        }
        Ok(GoBuildOutcome {
            success: true,
            artifacts,
            ..GoBuildOutcome::default()
        })
    }

    fn build_target(
        &self,
        owner: &str,
        artifact: &Path,
        tick: &mut impl FnMut() -> bool,
    ) -> Result<GoBuildTargetOutcome> {
        let mut command = Command::new("go");
        command
            .args(["build", "-json", "-o"])
            .arg(artifact)
            .arg(owner)
            .current_dir(&self.graph.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = GroupChild::spawn(command)?;
        let stdout = child.child_mut().stdout.take().context("go build stdout")?;
        let stderr = child.child_mut().stderr.take().context("go build stderr")?;
        let stdout = read_build_output(stdout);
        let stderr = read_build_output(stderr);
        let mut interrupted = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if tick() {
                interrupted = true;
                break child.stop(STOP_GRACE);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        child.stop(STOP_GRACE);
        let mut errors = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        if errors != stderr {
            errors.extend(stderr);
        }
        Ok(GoBuildTargetOutcome {
            success: !interrupted && status.is_some_and(|status| status.success()),
            interrupted,
            errors,
        })
    }
}

#[derive(Default)]
struct GoBuildTargetOutcome {
    success: bool,
    interrupted: bool,
    errors: Vec<String>,
}

#[derive(Default)]
pub struct GoBuildOutcome {
    pub success: bool,
    pub interrupted: bool,
    pub errors: Vec<String>,
    pub artifacts: BTreeMap<String, PathBuf>,
}

#[derive(Deserialize)]
struct BuildEvent {
    #[serde(rename = "Output")]
    output: Option<String>,
}

fn read_build_output(pipe: impl Read + Send + 'static) -> JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut collected = Vec::new();
        for line in BufReader::new(pipe)
            .lines()
            .map_while(std::result::Result::ok)
        {
            let text = match serde_json::from_str::<BuildEvent>(&line) {
                Ok(event) => {
                    let Some(output) = event.output else {
                        continue;
                    };
                    output
                }
                Err(_) => line,
            };
            let text = strip_ansi(text.trim_end());
            if !text.is_empty() && !text.starts_with("# ") {
                collected.push(text);
            }
        }
        collected
    })
}

pub fn executable_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_embed_patterns_include_matched_directory_trees() {
        let patterns = compile_embed_patterns(&["templates/*".to_string()]).unwrap();
        assert!(patterns.is_match("templates/admin/new.html"));
    }

    #[test]
    fn executable_names_follow_the_host() {
        assert_eq!(
            executable_name("api"),
            format!("api{}", std::env::consts::EXE_SUFFIX)
        );
    }

    #[test]
    fn fixture_models_workspace_bins_dependencies_and_inputs() {
        if Command::new("go").arg("version").output().is_err() {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_go_work");
        let go = GoDev::load(&root).unwrap();
        assert_eq!(go.graph.bin_names(), vec!["api", "tool"]);
        assert_eq!(
            go.graph.bin_owner("api", None).unwrap(),
            "example.test/svc/cmd/api"
        );
        let greet = root.join("svc/internal/greet/greet.go");
        let embed = root.join("svc/internal/greet/greeting.txt");
        let test = root.join("svc/internal/greet/greet_test.go");
        assert_eq!(go.graph.input_kind(&greet), GoInputKind::Source);
        assert_eq!(go.graph.input_kind(&embed), GoInputKind::Source);
        assert_eq!(go.graph.input_kind(&test), GoInputKind::Test);
        let affected = go
            .graph
            .with_dependents(["example.test/svc/internal/greet"]);
        assert!(affected.contains("example.test/svc/cmd/api"));
        assert_eq!(
            go.graph
                .input_kind(&root.join("svc/internal/greet/greeting-new.txt")),
            GoInputKind::Source
        );
        assert_eq!(
            go.graph
                .input_kind(&root.join("svc/internal/greet/README.md")),
            GoInputKind::Other
        );
        assert!(!affected.contains("example.test/tools/cmd/tool"));
        let test_helper = "example.test/svc/internal/testutil";
        let build_affected = go.graph.with_dependents([test_helper]);
        assert!(!build_affected.contains("example.test/svc/internal/greet"));
        let test_affected = go.graph.with_test_dependents([test_helper]);
        assert!(test_affected.contains("example.test/svc/internal/greet"));
        assert!(test_affected.contains("example.test/svc/cmd/api"));
        assert_eq!(
            go.graph
                .input_kind(&root.join("svc/internal/greet/deleted_test.go")),
            GoInputKind::Test
        );
    }

    #[test]
    fn fixture_detects_single_module() {
        if Command::new("go").arg("version").output().is_err() {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_go_mod");
        let go = GoDev::load(&root).unwrap();
        assert_eq!(go.graph.bin_names(), vec!["hello"]);
        assert_eq!(go.graph.watch_dirs(), vec![PathBuf::new()]);
    }

    #[test]
    fn build_output_ignores_json_status_and_package_headers() {
        let input = concat!(
            "{\"ImportPath\":\"example.test/m\",\"Action\":\"build-fail\"}\n",
            "{\"ImportPath\":\"example.test/m\",\"Action\":\"build-output\",\"Output\":\"# example.test/m\\n\"}\n",
            "{\"ImportPath\":\"example.test/m\",\"Action\":\"build-output\",\"Output\":\"./main.go:3:1: syntax error\\n\"}\n",
        );
        let output = read_build_output(std::io::Cursor::new(input.as_bytes().to_vec()))
            .join()
            .unwrap();
        assert_eq!(output, vec!["./main.go:3:1: syntax error"]);
    }

    #[test]
    fn major_version_module_builds_to_the_configured_binary_name() {
        if Command::new("go").arg("version").output().is_err() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("go.mod"),
            "module example.test/service/v2\n\ngo 1.25\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("main.go"),
            "package main\nfunc main() {}\n",
        )
        .unwrap();
        let go = GoDev::load(temp.path()).unwrap();
        let outcome = go
            .build(
                &[("service".to_string(), "example.test/service/v2".to_string())],
                || false,
            )
            .unwrap();
        assert!(outcome.success);
        assert!(outcome.artifacts["service"].is_file());
    }
}
