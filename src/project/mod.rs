//! What `ns` knows about the current project: `ns.toml` plus the workspace
//! layout from `cargo metadata`.

pub mod ns_config;
pub mod workspace_graph;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub use ns_config::NsConfig;
pub use workspace_graph::{Member, WorkspaceGraph};

/// Workspace graph and `ns.toml` for the workspace containing `dir`.
pub struct Project {
    pub graph: WorkspaceGraph,
    pub config: NsConfig,
}

impl Project {
    pub fn load(dir: &Path) -> Result<Project> {
        match WorkspaceGraph::load(dir) {
            Ok(graph) => {
                let config = NsConfig::load(&graph.root)?;
                Ok(Project { graph, config })
            }
            Err(cargo_err) => {
                let root = find_go_root(dir)
                    .with_context(|| format!("not a Cargo or Go project ({cargo_err:#})"))?;
                let config = NsConfig::load(&root)?;
                let bins = config.release.bins.clone();
                if bins.is_empty() {
                    bail!("Go releases require release.bins in ns.toml (the shipped binary names)");
                }
                let graph = WorkspaceGraph {
                    root: root.clone(),
                    target_dir: root.join(".naspeed-go"),
                    members: vec![Member {
                        name: bins[0].clone(),
                        // Go modules do not carry a package version. An empty
                        // version tells release versioning to use the tag.
                        version: String::new(),
                        edition: "go".into(),
                        manifest_path: root.join("go.mod"),
                        dir: root,
                        bins,
                        has_lib: false,
                        deps: Default::default(),
                    }],
                };
                Ok(Project { graph, config })
            }
        }
    }

    pub fn load_cwd() -> Result<Project> {
        Project::load(&std::env::current_dir()?)
    }
}

fn find_go_root(dir: &Path) -> Result<PathBuf> {
    let mut at = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    loop {
        if at.join("go.mod").is_file() {
            return Ok(at);
        }
        if !at.pop() {
            bail!("no go.mod found");
        }
    }
}
