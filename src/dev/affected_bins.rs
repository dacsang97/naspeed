//! Maps changed workspace packages and runtime files to selected applications.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::project::WorkspaceGraph;

use super::affected_tests::{Scope, changed_scope};
use super::dev_settings::AppSettings;

pub fn affected_bins(
    graph: &WorkspaceGraph,
    apps: &[AppSettings],
    rust_changes: &BTreeSet<PathBuf>,
    other_changes: &BTreeSet<PathBuf>,
) -> BTreeSet<String> {
    let mut affected: BTreeSet<String> = match changed_scope(graph, rust_changes) {
        Scope::Workspace => apps.iter().map(|app| app.bin.clone()).collect(),
        Scope::Packages(names) => {
            let dependent_owners = graph.with_dependents(names.iter().map(String::as_str));
            apps.iter()
                .filter(|app| dependent_owners.contains(&app.owner))
                .map(|app| app.bin.clone())
                .collect()
        }
    };
    for change in other_changes {
        let matching: Vec<&AppSettings> = apps
            .iter()
            .filter(|app| app.env_file.as_ref() == Some(change))
            .collect();
        if matching.is_empty() {
            return apps.iter().map(|app| app.bin.clone()).collect();
        }
        affected.extend(matching.into_iter().map(|app| app.bin.clone()));
    }
    affected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::DevArgs;
    use crate::dev::dev_settings::Settings;
    use crate::project::Project;
    use std::path::Path;

    fn settings() -> (Project, Settings) {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_multi_project");
        let project = Project::load(&fixture).unwrap();
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
        (project, settings)
    }

    fn paths(root: &std::path::Path, relative: &[&str]) -> BTreeSet<PathBuf> {
        relative.iter().map(|path| root.join(path)).collect()
    }

    #[test]
    fn api_only_change_does_not_restart_worker() {
        let (project, settings) = settings();
        assert_eq!(
            affected_bins(
                &project.graph,
                &settings.apps,
                &paths(&settings.root, &["crates/api-only/src/lib.rs"]),
                &BTreeSet::new(),
            ),
            ["api".to_string()].into()
        );
    }

    #[test]
    fn shared_change_restarts_both_dependents() {
        let (project, settings) = settings();
        assert_eq!(
            affected_bins(
                &project.graph,
                &settings.apps,
                &paths(&settings.root, &["crates/shared/src/lib.rs"]),
                &BTreeSet::new(),
            ),
            ["api".to_string(), "worker".to_string()].into()
        );
    }

    #[test]
    fn workspace_change_restarts_all_and_unknown_runtime_file_restarts_all() {
        let (project, settings) = settings();
        assert_eq!(
            affected_bins(
                &project.graph,
                &settings.apps,
                &paths(&settings.root, &["Cargo.lock"]),
                &BTreeSet::new(),
            ),
            ["api".to_string(), "worker".to_string()].into()
        );
        assert_eq!(
            affected_bins(
                &project.graph,
                &settings.apps,
                &BTreeSet::new(),
                &paths(&settings.root, &["README.md"]),
            ),
            ["api".to_string(), "worker".to_string()].into()
        );
    }

    #[test]
    fn changed_env_file_restarts_only_its_app() {
        let (project, mut settings) = settings();
        settings.apps[0].env_file = Some(settings.root.join("api.env"));
        settings.apps[1].env_file = Some(settings.root.join("worker.env"));
        assert_eq!(
            affected_bins(
                &project.graph,
                &settings.apps,
                &BTreeSet::new(),
                &paths(&settings.root, &["api.env"]),
            ),
            ["api".to_string()].into()
        );
    }
}
