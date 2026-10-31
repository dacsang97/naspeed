//! What `ns test` hands the runner: per package, the source trees and
//! binaries a test run may read, plus workspace-wide inputs. Written once as
//! JSON under the store's `tmp/` and found through `NASPEED_TEST_PLAN`, so
//! the runner (started by cargo once per test binary) never re-runs
//! `cargo metadata`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::project::Project;

pub const PLAN_ENV: &str = "NASPEED_TEST_PLAN";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageInputs {
    /// The package dir and the dirs of every workspace member it depends
    /// on (normal, dev and build deps, transitively).
    pub dirs: Vec<PathBuf>,
    /// The package's binaries; integration tests run them through
    /// `CARGO_BIN_EXE_<name>`, so their content joins the key.
    pub bins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestPlan {
    pub store: PathBuf,
    pub workspace_root: PathBuf,
    pub target_dir: PathBuf,
    /// `[test].env`: variables whose values join the key.
    pub env: Vec<String>,
    /// `Cargo.lock` plus `[test].inputs`, absolute.
    pub inputs: Vec<PathBuf>,
    /// Keyed by `CARGO_MANIFEST_DIR` (the package dir cargo runs tests in).
    pub packages: BTreeMap<PathBuf, PackageInputs>,
}

impl TestPlan {
    pub fn build(project: &Project, store: &Path) -> TestPlan {
        let graph = &project.graph;
        let mut packages = BTreeMap::new();
        for member in &graph.members {
            // Forward closure over workspace deps.
            let mut seen = BTreeSet::new();
            let mut stack = vec![member.name.as_str()];
            while let Some(name) = stack.pop() {
                if seen.insert(name)
                    && let Some(m) = graph.member(name)
                {
                    stack.extend(m.deps.iter().map(String::as_str));
                }
            }
            let mut dirs: Vec<PathBuf> = seen
                .iter()
                .filter_map(|n| graph.member(n))
                .map(|m| m.dir.clone())
                .collect();
            dirs.sort();
            dirs.dedup();
            packages.insert(
                member.dir.clone(),
                PackageInputs {
                    dirs,
                    bins: member.bins.clone(),
                },
            );
        }
        // Cargo.lock, cargo's `[env]` and dotenv files are read by tests
        // even though they are usually outside (or gitignored in) the
        // package trees.
        let mut inputs = vec![
            graph.root.join("Cargo.lock"),
            graph.root.join(".cargo/config.toml"),
            graph.root.join(".cargo/config"),
            graph.root.join(".env"),
        ];
        for m in &graph.members {
            inputs.push(m.dir.join(".env"));
            inputs.push(m.dir.join(".cargo/config.toml"));
        }
        inputs.sort();
        inputs.dedup();
        inputs.extend(
            project
                .config
                .test
                .inputs
                .iter()
                .map(|p| graph.root.join(p)),
        );
        TestPlan {
            store: store.to_path_buf(),
            workspace_root: graph.root.clone(),
            target_dir: graph.target_dir.clone(),
            env: project.config.test.env.clone(),
            inputs,
            packages,
        }
    }

    /// Write under `<store>/tmp/`; the caller removes the file afterwards.
    pub fn write(&self) -> Result<PathBuf> {
        // Not `tmp/`: gc clears old files there, and a long `ns test` run
        // must keep its plan.
        let dir = self.store.join("test-plans");
        std::fs::create_dir_all(&dir)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!("test-plan.{}.{nanos}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_vec(self)?)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }

    pub fn read(path: &Path) -> Result<TestPlan> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_plan_links_bins_to_their_deps() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project");
        let project = Project::load(&fixture).unwrap();
        let plan = TestPlan::build(&project, Path::new("/store"));
        let server = plan
            .packages
            .iter()
            .find(|(dir, _)| dir.ends_with("apps/hello-server"))
            .map(|(_, p)| p)
            .unwrap();
        assert_eq!(server.bins, vec!["hello-server".to_string()]);
        assert!(server.dirs.iter().any(|d| d.ends_with("crates/greeter")));
        let greeter = plan
            .packages
            .iter()
            .find(|(dir, _)| dir.ends_with("crates/greeter"))
            .map(|(_, p)| p)
            .unwrap();
        assert_eq!(greeter.dirs.len(), 1, "a leaf crate covers only itself");
        assert!(plan.inputs.iter().any(|p| p.ends_with("Cargo.lock")));
        assert!(plan.inputs.iter().any(|p| p.ends_with(".env")));
    }
}
