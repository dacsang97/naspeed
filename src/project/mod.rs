//! What `ns` knows about the current project: `ns.toml` plus the workspace
//! layout from `cargo metadata`.

pub mod ns_config;
pub mod workspace_graph;

use std::path::Path;

use anyhow::Result;

pub use ns_config::NsConfig;
pub use workspace_graph::{Member, WorkspaceGraph};

/// Workspace graph and `ns.toml` for the workspace containing `dir`.
pub struct Project {
    pub graph: WorkspaceGraph,
    pub config: NsConfig,
}

impl Project {
    pub fn load(dir: &Path) -> Result<Project> {
        let graph = WorkspaceGraph::load(dir)?;
        let config = NsConfig::load(&graph.root)?;
        Ok(Project { graph, config })
    }

    pub fn load_cwd() -> Result<Project> {
        Project::load(&std::env::current_dir()?)
    }
}
