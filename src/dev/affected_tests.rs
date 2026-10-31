//! Tests affected by a change: changed files → owning workspace members →
//! plus every member depending on them, run as one background
//! `cargo test -p a -p b` that a newer change can cancel (whole group).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};

use super::cargo_runner::cargo_command;
use super::process_supervisor::{GroupChild, STOP_GRACE};
use super::status_line;
use crate::project::WorkspaceGraph;

/// Which packages a change touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// A file outside every member (workspace `Cargo.toml`, `Cargo.lock`...)
    /// can affect anything.
    Workspace,
    Packages(BTreeSet<String>),
}

impl Scope {
    /// `-p` flags (or `--workspace`) for cargo.
    pub fn cargo_args(&self) -> Vec<String> {
        match self {
            Scope::Workspace => vec!["--workspace".into()],
            Scope::Packages(names) => names
                .iter()
                .flat_map(|n| ["-p".to_string(), n.clone()])
                .collect(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Scope::Workspace => "workspace".into(),
            Scope::Packages(names) => names.iter().cloned().collect::<Vec<_>>().join(", "),
        }
    }
}

/// Members owning the changed paths; `Workspace` when one has no owner.
pub fn changed_scope(graph: &WorkspaceGraph, paths: &BTreeSet<PathBuf>) -> Scope {
    let mut names = BTreeSet::new();
    for path in paths {
        match graph.member_for_path(path) {
            // The root package's dir is the workspace root: its own
            // Cargo.toml / Cargo.lock still affect every member.
            Some(m) if !(m.dir == graph.root && is_workspace_file(path, &graph.root)) => {
                names.insert(m.name.clone());
            }
            _ => return Scope::Workspace,
        }
    }
    Scope::Packages(names)
}

fn is_workspace_file(path: &Path, root: &Path) -> bool {
    path.parent() == Some(root)
        && path
            .file_name()
            .is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock")
}

/// Changed members plus their transitive dependents.
pub fn test_scope(graph: &WorkspaceGraph, paths: &BTreeSet<PathBuf>) -> Scope {
    match changed_scope(graph, paths) {
        Scope::Workspace => Scope::Workspace,
        Scope::Packages(names) => {
            Scope::Packages(graph.with_dependents(names.iter().map(String::as_str)))
        }
    }
}

/// A background `cargo test` whose combined output is shown only on failure.
pub struct TestRun {
    child: GroupChild,
    output: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
    started: Instant,
    scope: Scope,
    /// Changes this run covers; handed back when it is cancelled.
    pub paths: BTreeSet<PathBuf>,
    /// Test-cache plan the runner reads; removed when the run is dropped.
    _plan: Option<crate::testcache::PlanFile>,
}

pub enum TestResult {
    Passed,
    Failed,
}

impl TestRun {
    pub fn start(root: &Path, scope: Scope, paths: BTreeSet<PathBuf>) -> Result<TestRun> {
        // Through the test cache (unchanged packages replay their last
        // pass) unless the workspace cannot be loaded or opts out.
        let (mut cmd, plan) = match crate::project::Project::load(root)
            .and_then(|p| crate::testcache::cargo_test_command(&p, false, &[]))
        {
            Ok(pair) => pair,
            Err(_) => {
                let mut cmd = cargo_command();
                cmd.arg("test");
                (cmd, None)
            }
        };
        cmd.args(scope.cargo_args())
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = GroupChild::spawn(cmd)?;
        let output = Arc::new(Mutex::new(String::new()));
        let mut readers = Vec::new();
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .context("cargo test stdout")?;
        let stderr = child
            .child_mut()
            .stderr
            .take()
            .context("cargo test stderr")?;
        readers.push(collect(stdout, output.clone()));
        readers.push(collect(stderr, output.clone()));
        status_line::info(format!("testing {}", scope.describe()));
        Ok(TestRun {
            child,
            output,
            readers,
            started: Instant::now(),
            scope,
            paths,
            _plan: plan,
        })
    }

    /// Reports and returns the result once `cargo test` has finished.
    pub fn poll(&mut self) -> Result<Option<TestResult>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        self.child.stop(STOP_GRACE);
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
        let elapsed = status_line::ms(self.started.elapsed());
        if status.success() {
            status_line::ok(format!(
                "tests passed ({}) in {elapsed}",
                self.scope.describe()
            ));
            return Ok(Some(TestResult::Passed));
        }
        let output = self.output.lock().map(|o| o.clone()).unwrap_or_default();
        let lines: Vec<&str> = output.lines().collect();
        let tail = &lines[lines.len().saturating_sub(40)..];
        eprintln!("{}", tail.join("\n"));
        status_line::error(format!(
            "tests failed ({}) in {elapsed}",
            self.scope.describe()
        ));
        Ok(Some(TestResult::Failed))
    }

    /// Kills the whole test group; returns the changes it did not get to cover.
    pub fn cancel(mut self) -> BTreeSet<PathBuf> {
        self.child.stop(STOP_GRACE);
        status_line::info("tests cancelled by a newer change");
        std::mem::take(&mut self.paths)
    }
}

fn collect(pipe: impl std::io::Read + Send + 'static, into: Arc<Mutex<String>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
            if let Ok(mut out) = into.lock() {
                out.push_str(&line);
                out.push('\n');
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> WorkspaceGraph {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project");
        WorkspaceGraph::load(&fixture).unwrap()
    }

    fn paths(g: &WorkspaceGraph, rel: &[&str]) -> BTreeSet<PathBuf> {
        rel.iter().map(|r| g.root.join(r)).collect()
    }

    #[test]
    fn lib_change_tests_the_lib_and_its_dependents() {
        let g = graph();
        let scope = test_scope(&g, &paths(&g, &["crates/greeter/src/lib.rs"]));
        let want: BTreeSet<String> = ["greeter", "hello-server"].map(String::from).into();
        assert_eq!(scope, Scope::Packages(want));
        assert_eq!(scope.cargo_args(), ["-p", "greeter", "-p", "hello-server"]);
    }

    #[test]
    fn bin_change_tests_only_the_bin() {
        let g = graph();
        let scope = test_scope(&g, &paths(&g, &["apps/hello-server/src/main.rs"]));
        assert_eq!(scope, Scope::Packages(["hello-server".to_string()].into()));
    }

    #[test]
    fn workspace_files_affect_everything() {
        let g = graph();
        let scope = test_scope(&g, &paths(&g, &["Cargo.lock", "crates/greeter/src/lib.rs"]));
        assert_eq!(scope, Scope::Workspace);
        assert_eq!(scope.cargo_args(), ["--workspace"]);
    }
}
