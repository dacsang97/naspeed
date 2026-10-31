//! `ns ci init`: GitHub Actions workflows with the naspeed store cached,
//! laid out as three workflows: `ns-ci.yml` (lint + tests),
//! `ns-build.yml` (snapshot release build on main, warms the release cache)
//! and `ns-release.yml` (tagged release + optional image push).

mod release_workflows;
mod test_workflow;
pub mod workflow_templates;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::project::Project;
use workflow_templates::Toolchain;

/// Workflow directory relative to the workspace root.
pub const WORKFLOW_DIR: &str = ".github/workflows";
pub const CI_FILE: &str = "ns-ci.yml";
pub const BUILD_FILE: &str = "ns-build.yml";
pub const RELEASE_FILE: &str = "ns-release.yml";

#[derive(Debug, Subcommand)]
pub enum CiCommand {
    /// Write `ns-ci.yml`, `ns-build.yml` and `ns-release.yml` under `.github/workflows/`.
    Init(CiInitArgs),
}

#[derive(Debug, Args)]
pub struct CiInitArgs {
    /// Overwrite existing workflows.
    #[arg(long)]
    pub force: bool,
}

/// Where `ns image --push` goes, as far as the workflow cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTarget {
    /// Registry host (`ghcr.io`, `docker.io`, `registry.example.com:5000`).
    pub registry: String,
    /// ghcr.io: `ns image` authenticates with GITHUB_TOKEN, no login step.
    pub ghcr: bool,
}

/// Everything the templates take from the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSpec {
    pub project: String,
    /// Release build ids (`[[release.builds]]`, else one per released bin).
    pub builds: Vec<String>,
    pub ns_version: String,
    pub toolchain: Toolchain,
    /// Number of `[release].before` hooks (their text never enters the YAML).
    pub before_hooks: usize,
    /// `[image].name` is set.
    pub image: Option<ImageTarget>,
}

pub fn run(cmd: CiCommand) -> Result<u8> {
    match cmd {
        CiCommand::Init(args) => {
            let project = Project::load_cwd()?;
            for path in init(&project, args.force)? {
                crate::ui_info!("wrote workflow"; path = path.display());
            }
            for note in notes(&project)? {
                crate::ui_warn!(note);
            }
            Ok(0)
        }
    }
}

/// Names end up in YAML and comments unquoted: keep a conservative charset.
fn safe_name(name: &str) -> Option<String> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then(|| name.to_owned())
}

/// Registry of an image name, by Docker's rule (the one `ns image` uses): a
/// first path component with a `.` or `:`, or `localhost`, is a registry;
/// anything else is Docker Hub.
fn registry_of(image: &str) -> Result<String> {
    let registry = match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            first
        }
        _ => "docker.io",
    };
    let ok = registry
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':'));
    if !ok {
        bail!("[image].name {image:?} has an invalid registry {registry:?}");
    }
    Ok(registry.to_owned())
}

/// The workflow spec for `project`: root package name (else the workspace
/// dir name), build variants, toolchain file, cross and image needs.
pub fn spec_for(project: &Project) -> Result<WorkflowSpec> {
    let graph = &project.graph;
    let root_package = graph
        .members
        .iter()
        .find(|m| m.dir == graph.root)
        .map(|m| m.name.clone());
    let dir_name = graph
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let project_name = root_package
        .or(dir_name)
        .as_deref()
        .and_then(safe_name)
        .unwrap_or_else(|| "ns".to_owned());
    let release = &project.config.release;
    let builds: Vec<String> = if !release.builds.is_empty() {
        release.builds.iter().map(|b| b.id().to_owned()).collect()
    } else if !release.bins.is_empty() {
        release.bins.clone()
    } else {
        graph
            .bins()
            .into_iter()
            .map(|(_, b)| b.to_owned())
            .collect()
    };
    // Per-build target overrides count towards the cross tools needed.
    let targets: Vec<&str> = release
        .targets
        .iter()
        .chain(release.builds.iter().flat_map(|b| b.targets.iter()))
        .map(String::as_str)
        .collect();
    let image = match &project.config.image.name {
        None => None,
        Some(name) => {
            let registry = registry_of(name)?;
            Some(ImageTarget {
                ghcr: registry == "ghcr.io",
                registry,
            })
        }
    };
    Ok(WorkflowSpec {
        project: project_name,
        builds: builds.iter().filter_map(|b| safe_name(b)).collect(),
        ns_version: env!("CARGO_PKG_VERSION").to_owned(),
        toolchain: Toolchain {
            file: ["rust-toolchain.toml", "rust-toolchain"]
                .iter()
                .any(|f| graph.root.join(f).is_file()),
            zig: targets
                .iter()
                .any(|t| crate::cross::ZIG_TARGETS.contains(t)),
            msvc: targets.contains(&crate::cross::MSVC_TARGET),
        },
        before_hooks: release.before.len(),
        image,
    })
}

/// The three workflows as `(file name, text)`, in a fixed order.
pub fn render_all(spec: &WorkflowSpec) -> [(&'static str, String); 3] {
    [
        (CI_FILE, test_workflow::render(spec)),
        (BUILD_FILE, release_workflows::render_build(spec)),
        (RELEASE_FILE, release_workflows::render_release(spec)),
    ]
}

/// Write the workflows under the workspace root. Without `force`, nothing is
/// written when any of them already exists.
pub fn init(project: &Project, force: bool) -> Result<Vec<PathBuf>> {
    let dir = project.graph.root.join(WORKFLOW_DIR);
    let files = render_all(&spec_for(project)?);
    if !force {
        let existing: Vec<String> = files
            .iter()
            .map(|(name, _)| dir.join(name))
            .filter(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if !existing.is_empty() {
            bail!(
                "{} already exist(s); pass --force to overwrite",
                existing.join(", ")
            );
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    files
        .iter()
        .map(|(name, text)| {
            let path = dir.join(name);
            write(&path, text)?;
            Ok(path)
        })
        .collect()
}

/// Atomic write: temp file in the same dir, then rename.
fn write(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("workflow path has no parent")?;
    let name = path.file_name().context("workflow path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("write {}", path.display()))
}

/// Follow-ups the generated workflows cannot decide for the owner.
fn notes(project: &Project) -> Result<Vec<String>> {
    let spec = spec_for(project)?;
    let mut notes = vec![format!(
        "the workflows download naspeed v{} from github.com/{}: that release must be published first",
        spec.ns_version,
        workflow_templates::NASPEED_RELEASE_REPO
    )];
    if spec.toolchain.msvc {
        notes.push(format!(
            "{} needs the Microsoft CRT/SDK: after reviewing its license, add --accept-msvc-license to the `setup` line in {BUILD_FILE} and {RELEASE_FILE}",
            crate::cross::MSVC_TARGET
        ));
    }
    if spec.before_hooks > 0 {
        notes.push(format!(
            "ns.toml has {} `before` hook(s): add steps installing their tools to {BUILD_FILE} and {RELEASE_FILE}",
            spec.before_hooks
        ));
    }
    if let Some(ImageTarget {
        registry,
        ghcr: false,
    }) = &spec.image
    {
        notes.push(format!(
            "{RELEASE_FILE} logs in to {registry} with placeholder secrets REGISTRY_USERNAME / REGISTRY_PASSWORD: create them"
        ));
    }
    Ok(notes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Spec with two build variants, before hooks, zig and msvc targets.
    pub(crate) fn sample_spec(image: Option<ImageTarget>) -> WorkflowSpec {
        WorkflowSpec {
            project: "demo".into(),
            builds: vec!["api".into(), "api-noui".into()],
            ns_version: "0.1.0".into(),
            toolchain: Toolchain {
                file: true,
                zig: true,
                msvc: true,
            },
            before_hooks: 2,
            image,
        }
    }

    #[test]
    fn names_with_yaml_or_shell_syntax_are_rejected() {
        assert_eq!(safe_name("hello-server"), Some("hello-server".into()));
        assert_eq!(safe_name("a: b"), None);
        assert_eq!(safe_name("x$(id)"), None);
        assert_eq!(safe_name(""), None);
    }

    #[test]
    fn registry_follows_the_docker_rule() {
        assert_eq!(registry_of("ghcr.io/o/api").unwrap(), "ghcr.io");
        assert_eq!(registry_of("localhost:5000/a/b").unwrap(), "localhost:5000");
        assert_eq!(registry_of("localhost/a").unwrap(), "localhost");
        assert_eq!(registry_of("owner/api").unwrap(), "docker.io");
        assert_eq!(registry_of("api").unwrap(), "docker.io");
        assert!(registry_of("ev$il.io/a").is_err());
    }

    #[test]
    fn rendering_is_deterministic() {
        let spec = sample_spec(None);
        assert_eq!(render_all(&spec), render_all(&spec));
    }
}
