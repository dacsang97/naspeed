//! What `ns dev` runs and watches, resolved once at startup from CLI flags,
//! `[dev]` in ns.toml and the workspace graph.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::DevArgs;
use crate::project::ns_config::LogDir;
use crate::project::{Project, WorkspaceGraph};

pub struct Settings {
    pub root: PathBuf,
    pub apps: Vec<AppSettings>,
    pub test: bool,
    /// Root-relative subtrees watched recursively (default: every member).
    pub watch_dirs: Vec<PathBuf>,
    /// Root-relative single files watched outside `watch_dirs`.
    pub watch_files: Vec<PathBuf>,
    pub ignore: Vec<String>,
    /// File output directory; `None` disables logging.
    pub log_dir: Option<PathBuf>,
    pub tui: TuiMode,
}

#[derive(Debug, Clone)]
pub struct AppSettings {
    pub bin: String,
    pub owner: String,
    pub args: Vec<String>,
    pub env_file: Option<PathBuf>,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Auto,
    Disabled,
}

impl Settings {
    pub fn resolve(args: DevArgs, project: &Project) -> Result<Settings> {
        let DevArgs {
            bin: cli_bins,
            all,
            no_tui,
            test,
            args: cli_args,
        } = args;
        let graph = &project.graph;
        let cfg = &project.config.dev;
        if cfg.bin.is_some() && !cfg.bins.is_empty() {
            bail!("ns.toml cannot set both [dev].bin and [dev].bins");
        }
        if all && !cli_bins.is_empty() {
            bail!("--all cannot be used with --bin");
        }
        let bins = resolve_bins(graph, cli_bins, all, cfg.bin.as_deref(), &cfg.bins)?;
        if !cli_args.is_empty() && bins.len() != 1 {
            bail!("arguments after -- require exactly one selected binary");
        }
        let mut cli_args = (!cli_args.is_empty()).then_some(cli_args);
        for name in cfg.apps.keys() {
            if graph.bin_owner(name).is_none() {
                bail!("[dev.apps.{name}] does not name a workspace binary");
            }
        }
        let root = graph.root.clone();
        let mut used_ports = BTreeMap::new();
        let mut watch_files: Vec<PathBuf> = [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
        ]
        .map(PathBuf::from)
        .into();
        let apps = bins
            .into_iter()
            .map(|bin| {
                let owner = graph
                    .bin_owner(&bin)
                    .with_context(|| format!("no workspace binary named {bin:?}"))?
                    .name
                    .clone();
                let override_cfg = cfg.apps.get(&bin);
                let env_file = override_cfg
                    .and_then(|app| app.env_file.clone())
                    .or_else(|| cfg.env_file.clone());
                if let Some(path) = &env_file
                    && !watch_files.contains(path)
                {
                    watch_files.push(path.clone());
                }
                let ports = override_cfg
                    .and_then(|app| app.ports.clone())
                    .unwrap_or_else(|| cfg.ports.clone());
                for port in &ports {
                    if let Some(other) = used_ports.insert(*port, bin.clone()) {
                        bail!("port {port} is assigned to both {other} and {bin}");
                    }
                }
                let app_args = cli_args.take().unwrap_or_else(|| {
                    override_cfg
                        .and_then(|app| app.args.clone())
                        .unwrap_or_else(|| cfg.args.clone())
                });
                Ok(AppSettings {
                    bin,
                    owner,
                    args: app_args,
                    env_file: env_file.map(|path| root.join(path)),
                    ports,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let watch_dirs = if cfg.watch.is_empty() {
            graph
                .members
                .iter()
                .filter_map(|m| m.dir.strip_prefix(&root).ok().map(Path::to_path_buf))
                .collect()
        } else {
            cfg.watch.clone()
        };
        let log_dir = match &cfg.log_dir {
            LogDir::Enabled(false) => None,
            LogDir::Enabled(true) => Some(graph.target_dir.join("ns-dev")),
            LogDir::Path(path) => Some(root.join(path)),
        };
        if let Some(log_dir) = &log_dir {
            validate_log_dir(log_dir, &root, &watch_dirs, &watch_files)?;
        }
        let mut ignore = cfg.ignore.clone();
        if let Some(log_dir) = &log_dir {
            for pattern in log_dir_ignore_patterns(&root, log_dir) {
                if !ignore.contains(&pattern) {
                    ignore.push(pattern);
                }
            }
        }
        Ok(Settings {
            apps,
            test: test || cfg.test,
            watch_dirs,
            watch_files,
            ignore,
            log_dir,
            tui: if no_tui {
                TuiMode::Disabled
            } else {
                TuiMode::Auto
            },
            root,
        })
    }
}

fn validate_log_dir(
    log_dir: &Path,
    root: &Path,
    watch_dirs: &[PathBuf],
    watch_files: &[PathBuf],
) -> Result<()> {
    validate_log_dir_symlinks(log_dir)?;
    let log_paths = path_candidates(log_dir);
    for dir in watch_dirs {
        let watched_paths = path_candidates(&root.join(dir));
        if log_paths.iter().any(|log_path| {
            watched_paths
                .iter()
                .any(|watched| watched == log_path || watched.starts_with(log_path))
        }) {
            bail!(
                "log directory {} overlaps watched source directory {}",
                log_dir.display(),
                dir.display()
            );
        }
    }
    for file in watch_files {
        let watched_paths = path_candidates(&root.join(file));
        if log_paths.iter().any(|log_path| {
            watched_paths
                .iter()
                .any(|watched| watched.starts_with(log_path))
        }) {
            bail!(
                "log directory {} contains explicitly watched file {}",
                log_dir.display(),
                file.display()
            );
        }
    }
    Ok(())
}

fn validate_log_dir_symlinks(log_dir: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    let mut resolved_symlink: Option<PathBuf> = None;
    for component in log_dir.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {
                current.push(component.as_os_str());
            }
            Component::ParentDir => {
                if let Some(symlink) = &resolved_symlink {
                    bail!(
                        "invalid [dev].log_dir {}: parent traversal after symlink component {} is ambiguous",
                        log_dir.display(),
                        symlink.display()
                    );
                }
                if current.file_name().is_some() {
                    current.pop();
                } else {
                    current.push(component.as_os_str());
                }
            }
            Component::Normal(name) => {
                current.push(name);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        current.canonicalize().with_context(|| {
                            format!(
                                "[dev].log_dir {} contains an unresolved symlink at {}",
                                log_dir.display(),
                                current.display()
                            )
                        })?;
                        resolved_symlink = Some(current.clone());
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!(
                                "inspect [dev].log_dir path {} at {}",
                                log_dir.display(),
                                current.display()
                            )
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

fn path_candidates(path: &Path) -> Vec<PathBuf> {
    let lexical = normalize_path(path);
    let resolved = resolve_through_existing_ancestor(path);
    if resolved == lexical {
        vec![lexical]
    } else {
        vec![lexical, resolved]
    }
}

/// Resolves symlinks in the deepest existing prefix, then appends the
/// normalized missing suffix. This also handles log directories created
/// after settings resolution.
fn resolve_through_existing_ancestor(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    let mut ancestor = normalized.clone();
    let mut missing = Vec::<OsString>::new();
    loop {
        let candidate = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor.as_path()
        };
        if let Ok(mut resolved) = candidate.canonicalize() {
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return normalize_path(&resolved);
        }

        let Some(name) = ancestor.file_name() else {
            return normalized;
        };
        missing.push(name.to_os_string());
        if !ancestor.pop() {
            return normalized;
        }
    }
}

fn log_dir_ignore_patterns(root: &Path, log_dir: &Path) -> Vec<String> {
    let mut patterns = Vec::new();
    for root_path in path_candidates(root) {
        for log_path in path_candidates(log_dir) {
            if let Some(pattern) = log_dir_ignore_pattern(&root_path, &log_path)
                && !patterns.contains(&pattern)
            {
                patterns.push(pattern);
            }
        }
    }
    patterns
}

/// Builds a root-anchored gitignore pattern for an exact directory path.
/// Components are escaped so glob characters in a configured path stay literal.
fn log_dir_ignore_pattern(root: &Path, log_dir: &Path) -> Option<String> {
    let root = normalize_path(root);
    let log_dir = normalize_path(log_dir);
    let relative = log_dir.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return Some("/**".to_string());
    }

    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(escape_gitignore_component(name)),
            _ => None,
        })
        .collect::<Vec<_>>();
    (!components.is_empty()).then(|| format!("/{}", components.join("/")))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn escape_gitignore_component(component: &std::ffi::OsStr) -> String {
    let mut escaped = String::new();
    let characters = component.to_string_lossy().chars().collect::<Vec<_>>();
    for (index, character) in characters.iter().copied().enumerate() {
        let trailing_space = character == ' ' && characters[index..].iter().all(|c| *c == ' ');
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}' | ',') || trailing_space {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Files that can change what gets compiled (as opposed to templates,
/// `.env`... which only need a restart).
pub fn is_rust_input(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    path.extension().is_some_and(|e| e == "rs")
        || matches!(
            name.as_ref(),
            "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
        )
}

fn resolve_bins(
    graph: &WorkspaceGraph,
    cli_bins: Vec<String>,
    all: bool,
    legacy: Option<&str>,
    configured: &[String],
) -> Result<Vec<String>> {
    let bins = if !cli_bins.is_empty() {
        cli_bins
    } else if all {
        graph
            .bins()
            .into_iter()
            .map(|(_, bin)| bin.to_string())
            .collect()
    } else if !configured.is_empty() {
        configured.to_vec()
    } else if let Some(bin) = legacy {
        vec![bin.to_string()]
    } else {
        match graph.bins().as_slice() {
            [(_, bin)] => vec![bin.to_string()],
            [] => bail!("the workspace has no binary to run"),
            many => bail!(
                "the workspace has several binaries ({}); pick with --bin, --all or [dev].bins",
                many.iter()
                    .map(|(_, bin)| *bin)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    };
    if bins.is_empty() {
        bail!("the workspace has no binary to run");
    }
    let mut seen = std::collections::BTreeSet::new();
    for bin in &bins {
        if !seen.insert(bin) {
            bail!("binary {bin:?} is selected more than once");
        }
        if graph.bin_owner(bin).is_none() {
            bail!("no workspace binary named {bin:?}");
        }
    }
    Ok(bins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_inputs_trigger_builds_others_restart() {
        assert!(is_rust_input(Path::new("/ws/crates/a/src/lib.rs")));
        assert!(is_rust_input(Path::new("/ws/crates/a/Cargo.toml")));
        assert!(is_rust_input(Path::new("/ws/Cargo.lock")));
        assert!(!is_rust_input(Path::new("/ws/.env")));
        assert!(!is_rust_input(Path::new(
            "/ws/apps/api/templates/index.html"
        )));
    }

    #[test]
    fn single_binary_is_the_default() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project");
        let project = Project::load(&fixture).unwrap();
        let args = DevArgs {
            bin: Vec::new(),
            all: false,
            no_tui: true,
            test: true,
            args: Vec::new(),
        };
        let s = Settings::resolve(args, &project).unwrap();
        assert_eq!(
            (s.apps[0].bin.as_str(), s.apps[0].owner.as_str()),
            ("hello-server", "hello-server")
        );
        assert_eq!(s.apps[0].args, ["--port", "18080"]);
        assert_eq!(s.apps[0].ports, [18080]);
        assert!(s.test);
        assert_eq!(s.watch_dirs.len(), 2);
        assert_eq!(s.log_dir, Some(project.graph.target_dir.join("ns-dev")));
        let wrong = DevArgs {
            bin: vec!["nope".into()],
            all: false,
            no_tui: true,
            test: false,
            args: Vec::new(),
        };
        assert!(Settings::resolve(wrong, &project).is_err());
    }

    #[test]
    fn resolves_multi_bin_overrides_and_selection_precedence() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.args = vec!["--default".into()];
        project.config.dev.env_file = Some(".env".into());
        project.config.dev.apps.get_mut("worker").unwrap().env_file = None;
        project.config.dev.apps.get_mut("api").unwrap().env_file =
            Some(PathBuf::from("config/api.env"));
        project.config.dev.apps.get_mut("api").unwrap().args = Some(vec!["--api".into()]);
        let args = DevArgs {
            bin: Vec::new(),
            all: false,
            no_tui: true,
            test: false,
            args: Vec::new(),
        };
        let settings = Settings::resolve(args, &project).unwrap();
        assert_eq!(
            settings
                .apps
                .iter()
                .map(|app| app.bin.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker"]
        );
        assert_eq!(settings.apps[0].args, ["--api"]);
        assert_eq!(settings.apps[1].args, ["--default"]);
        assert_eq!(settings.apps[0].ports, [18081]);
        let nested_env = settings.root.join("config/api.env");
        assert_eq!(
            settings.apps[0].env_file.as_deref(),
            Some(nested_env.as_path())
        );
        assert!(
            settings.apps[1]
                .env_file
                .as_ref()
                .is_some_and(|path| path == &settings.root.join(".env"))
        );
        assert!(
            settings
                .watch_files
                .contains(&PathBuf::from("config/api.env"))
        );
        assert!(settings.watch_files.contains(&PathBuf::from(".env")));
        assert!(
            settings
                .watch_dirs
                .iter()
                .all(|dir| { !PathBuf::from("config/api.env").starts_with(dir) })
        );
        assert_eq!(
            super::super::affected_bins::affected_bins(
                &project.graph,
                &settings.apps,
                &std::collections::BTreeSet::new(),
                &[nested_env].into_iter().collect(),
            ),
            ["api".to_string()].into()
        );

        let one = Settings::resolve(
            DevArgs {
                bin: vec!["api".into()],
                all: false,
                no_tui: true,
                test: false,
                args: vec!["--cli".into()],
            },
            &project,
        )
        .unwrap();
        assert_eq!(one.apps.len(), 1);
        assert_eq!(one.apps[0].args, ["--cli"]);
    }
    #[test]
    fn all_selects_every_workspace_bin_even_when_configured_bins_are_narrower() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.bins = vec!["api".into()];
        let settings = Settings::resolve(
            DevArgs {
                bin: Vec::new(),
                all: true,
                no_tui: true,
                test: false,
                args: Vec::new(),
            },
            &project,
        )
        .unwrap();
        assert_eq!(
            settings
                .apps
                .iter()
                .map(|app| app.bin.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker"]
        );
    }

    #[test]
    fn rejects_conflicting_config_ports_and_invalid_cli_combinations() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let mut project = Project::load(&fixture).unwrap();
        let args = || DevArgs {
            bin: Vec::new(),
            all: false,
            no_tui: true,
            test: false,
            args: Vec::new(),
        };
        project.config.dev.bin = Some("api".into());
        assert!(Settings::resolve(args(), &project).is_err());

        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.ports = vec![18081];
        project.config.dev.apps.get_mut("worker").unwrap().ports = Some(vec![18081]);
        let err = Settings::resolve(args(), &project)
            .err()
            .expect("duplicate ports must be rejected")
            .to_string();
        assert!(err.contains("api") && err.contains("worker"), "{err}");

        let project = Project::load(&fixture).unwrap();
        let two = DevArgs {
            args: vec!["--flag".into()],
            ..args()
        };
        assert!(Settings::resolve(two, &project).is_err());
        let conflict = DevArgs {
            bin: vec!["api".into()],
            all: true,
            ..args()
        };
        assert!(Settings::resolve(conflict, &project).is_err());
    }
    #[test]
    fn resolves_disabled_and_explicit_log_directories() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let mut project = Project::load(&fixture).unwrap();
        let args = || DevArgs {
            bin: Vec::new(),
            all: false,
            no_tui: true,
            test: false,
            args: Vec::new(),
        };
        project.config.dev.log_dir = LogDir::Enabled(false);
        let disabled = Settings::resolve(args(), &project).unwrap();
        assert_eq!(disabled.log_dir, None);
        assert_eq!(disabled.tui, TuiMode::Disabled);

        project.config.dev.log_dir = LogDir::Path(PathBuf::from(".run-logs"));
        let explicit = Settings::resolve(args(), &project).unwrap();
        assert_eq!(explicit.log_dir, Some(project.graph.root.join(".run-logs")));
    }

    #[test]
    fn custom_log_directory_is_excluded_as_an_exact_literal_path() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.watch = vec![PathBuf::from(".")];
        project.config.dev.log_dir = LogDir::Path(PathBuf::from("run[logs]"));
        let settings = Settings::resolve(
            DevArgs {
                bin: Vec::new(),
                all: false,
                no_tui: true,
                test: false,
                args: Vec::new(),
            },
            &project,
        )
        .unwrap();
        let filter = super::super::file_watcher::WatchFilter::new(
            &settings.root,
            &settings.watch_dirs,
            &settings.watch_files,
            &settings.ignore,
        )
        .unwrap();

        assert!(
            filter
                .relevant(&settings.root.join("run[logs]/api.log"))
                .is_none()
        );
        assert!(
            filter
                .relevant(&settings.root.join("runxlogs/api.log"))
                .is_some(),
            "a neighboring directory must remain watchable"
        );
    }

    #[test]
    fn rejects_log_directories_that_cover_watched_sources_or_files() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let args = || DevArgs {
            bin: Vec::new(),
            all: false,
            no_tui: true,
            test: false,
            args: Vec::new(),
        };

        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.log_dir = LogDir::Path(PathBuf::from("."));
        let err = Settings::resolve(args(), &project)
            .err()
            .expect("workspace root logging would hide watched source files");
        assert!(err.to_string().contains("watched source directory"));

        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.watch = vec![PathBuf::from("crates/api-only")];
        project.config.dev.log_dir = LogDir::Path(PathBuf::from("crates"));
        let err = Settings::resolve(args(), &project)
            .err()
            .expect("member ancestor logging would hide its watched subtree");
        assert!(err.to_string().contains("watched source directory"));

        let mut project = Project::load(&fixture).unwrap();
        project.config.dev.apps.get_mut("api").unwrap().env_file =
            Some(PathBuf::from("config/api.env"));
        project.config.dev.log_dir = LogDir::Path(PathBuf::from("config"));
        let err = Settings::resolve(args(), &project)
            .err()
            .expect("logging would hide the explicitly watched env file");
        assert!(err.to_string().contains("explicitly watched file"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_log_directory_resolving_to_watched_source() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let source = root.join("apps/api");
        std::fs::create_dir_all(&source).unwrap();
        symlink(&source, root.join("logs")).unwrap();

        let err = validate_log_dir(&root.join("logs"), &root, &[PathBuf::from("apps/api")], &[])
            .unwrap_err();
        assert!(err.to_string().contains("watched source directory"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_log_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("apps/api")).unwrap();
        symlink(root.join("apps/api/runtime-logs"), root.join("logs")).unwrap();

        let err = validate_log_dir(&root.join("logs"), &root, &[], &[]).unwrap_err();
        assert!(err.to_string().contains("unresolved symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_parent_traversal_after_log_dir_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let nested = root.join("apps/api/nested");
        std::fs::create_dir_all(&nested).unwrap();
        symlink(&nested, root.join("alias")).unwrap();

        let err = validate_log_dir(
            &root.join("alias/../runtime-logs"),
            &root,
            &[PathBuf::from("apps/api")],
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("parent traversal after symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn safe_nested_symlink_log_directory_ignores_its_alias_and_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let target = root.join("apps/api/runtime-logs");
        std::fs::create_dir_all(root.join("apps/api/src")).unwrap();
        symlink(root.join("apps/api"), root.join("logs")).unwrap();
        let log_dir = root.join("logs/runtime-logs");
        let watch_dirs = [PathBuf::from(".")];

        validate_log_dir(&log_dir, &root, &watch_dirs, &[]).unwrap();
        let ignore = log_dir_ignore_patterns(&root, &log_dir);
        assert!(ignore.contains(&"/logs/runtime-logs".to_string()));
        assert!(ignore.contains(&"/apps/api/runtime-logs".to_string()));

        let filter =
            super::super::file_watcher::WatchFilter::new(&root, &watch_dirs, &[], &ignore).unwrap();
        assert!(
            filter
                .relevant(&root.join("logs/runtime-logs/build.log"))
                .is_none()
        );
        assert!(filter.relevant(&target.join("build.log")).is_none());
        assert!(
            filter
                .relevant(&root.join("apps/api/src/main.rs"))
                .is_some()
        );
    }
}
