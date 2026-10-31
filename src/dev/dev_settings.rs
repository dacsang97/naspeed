//! What `ns dev` runs and watches, resolved once at startup from CLI flags,
//! `[dev]` in ns.toml and the workspace graph.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::DevArgs;
use crate::project::{Project, WorkspaceGraph};

pub struct Settings {
    pub root: PathBuf,
    pub bin: String,
    /// Package owning `bin`.
    pub owner: String,
    pub args: Vec<String>,
    /// Absolute path of `[dev].env_file`.
    pub env_file: Option<PathBuf>,
    pub ports: Vec<u16>,
    pub test: bool,
    /// Root-relative subtrees watched recursively (default: every member).
    pub watch_dirs: Vec<PathBuf>,
    /// Root-relative single files watched outside `watch_dirs`.
    pub watch_files: Vec<PathBuf>,
    pub ignore: Vec<String>,
}

impl Settings {
    pub fn resolve(args: DevArgs, project: &Project) -> Result<Settings> {
        let graph = &project.graph;
        let cfg = &project.config.dev;
        let bin = resolve_bin(graph, args.bin.or_else(|| cfg.bin.clone()))?;
        let owner = graph
            .bin_owner(&bin)
            .with_context(|| format!("no workspace binary named {bin:?}"))?
            .name
            .clone();
        let root = graph.root.clone();
        let watch_dirs = if cfg.watch.is_empty() {
            graph
                .members
                .iter()
                .filter_map(|m| m.dir.strip_prefix(&root).ok().map(Path::to_path_buf))
                .collect()
        } else {
            cfg.watch.clone()
        };
        let mut watch_files: Vec<PathBuf> = [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
        ]
        .map(PathBuf::from)
        .into();
        watch_files.extend(cfg.env_file.clone());
        Ok(Settings {
            bin,
            owner,
            args: if args.args.is_empty() {
                cfg.args.clone()
            } else {
                args.args
            },
            env_file: cfg.env_file.as_ref().map(|p| root.join(p)),
            ports: cfg.ports.clone(),
            test: args.test || cfg.test,
            watch_dirs,
            watch_files,
            ignore: cfg.ignore.clone(),
            root,
        })
    }
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

fn resolve_bin(graph: &WorkspaceGraph, wanted: Option<String>) -> Result<String> {
    if let Some(bin) = wanted {
        return Ok(bin);
    }
    let bins = graph.bins();
    match bins.as_slice() {
        [(_, bin)] => Ok(bin.to_string()),
        [] => bail!("the workspace has no binary to run"),
        many => bail!(
            "the workspace has several binaries ({}); pick one with --bin or [dev].bin in ns.toml",
            many.iter().map(|(_, b)| *b).collect::<Vec<_>>().join(", ")
        ),
    }
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
            bin: None,
            test: true,
            args: Vec::new(),
        };
        let s = Settings::resolve(args, &project).unwrap();
        assert_eq!(
            (s.bin.as_str(), s.owner.as_str()),
            ("hello-server", "hello-server")
        );
        assert_eq!(s.args, ["--port", "18080"]);
        assert!(s.test);
        assert_eq!(s.watch_dirs.len(), 2);
        let wrong = DevArgs {
            bin: Some("nope".into()),
            test: false,
            args: Vec::new(),
        };
        assert!(Settings::resolve(wrong, &project).is_err());
    }
}
