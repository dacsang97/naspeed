//! Workspace layout from `cargo metadata --no-deps`: members, their binaries,
//! source dirs, and the reverse graph "crate → workspace crates that depend on
//! it" used to pick the tests affected by a change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    /// Directory holding the member's `Cargo.toml`.
    pub dir: PathBuf,
    pub bins: Vec<String>,
    pub has_lib: bool,
    /// Workspace members this one depends on (normal, dev and build deps).
    pub deps: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceGraph {
    pub root: PathBuf,
    pub target_dir: PathBuf,
    pub members: Vec<Member>,
}

impl WorkspaceGraph {
    /// Runs `cargo metadata` in `dir` (any directory inside the workspace).
    pub fn load(dir: &Path) -> Result<WorkspaceGraph> {
        let out = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(dir)
            .output()
            .context("run cargo metadata")?;
        if !out.status.success() {
            bail!(
                "cargo metadata failed in {}: {}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let json: Value = serde_json::from_slice(&out.stdout).context("parse cargo metadata")?;
        WorkspaceGraph::from_metadata(&json)
    }

    pub fn from_metadata(json: &Value) -> Result<WorkspaceGraph> {
        let root = PathBuf::from(json["workspace_root"].as_str().context("workspace_root")?);
        let target_dir = PathBuf::from(
            json["target_directory"]
                .as_str()
                .context("target_directory")?,
        );
        let member_ids: BTreeSet<&str> = json["workspace_members"]
            .as_array()
            .context("workspace_members")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let packages = json["packages"].as_array().context("packages")?;
        let mut members = Vec::new();
        for pkg in packages {
            if !member_ids.contains(pkg["id"].as_str().unwrap_or_default()) {
                continue;
            }
            let manifest_path =
                PathBuf::from(pkg["manifest_path"].as_str().context("manifest_path")?);
            let mut bins = Vec::new();
            let mut has_lib = false;
            for target in pkg["targets"].as_array().into_iter().flatten() {
                let kinds: Vec<&str> = target["kind"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if kinds.contains(&"bin") {
                    bins.push(target["name"].as_str().unwrap_or_default().to_string());
                }
                if kinds.iter().any(|k| {
                    matches!(
                        *k,
                        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                    )
                }) {
                    has_lib = true;
                }
            }
            // Path dependencies; filtered to members below once every name is known.
            let deps = pkg["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| d["path"].is_string())
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect();
            members.push(Member {
                name: pkg["name"].as_str().context("package name")?.to_string(),
                version: pkg["version"].as_str().unwrap_or_default().to_string(),
                dir: manifest_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                manifest_path,
                bins,
                has_lib,
                deps,
            });
        }
        let names: BTreeSet<String> = members.iter().map(|m| m.name.clone()).collect();
        for m in &mut members {
            m.deps.retain(|d| names.contains(d));
        }
        Ok(WorkspaceGraph {
            root,
            target_dir,
            members,
        })
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    /// `(member, bin)` for every binary in the workspace.
    pub fn bins(&self) -> Vec<(&Member, &str)> {
        self.members
            .iter()
            .flat_map(|m| m.bins.iter().map(move |b| (m, b.as_str())))
            .collect()
    }

    /// The member owning `bin`.
    pub fn bin_owner(&self, bin: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.bins.iter().any(|b| b == bin))
    }

    /// The member whose directory contains `path` (deepest match, so nested
    /// members win over the root package).
    pub fn member_for_path(&self, path: &Path) -> Option<&Member> {
        self.members
            .iter()
            .filter(|m| path.starts_with(&m.dir))
            .max_by_key(|m| m.dir.components().count())
    }

    /// `names` plus every workspace crate that depends on them, transitively.
    pub fn with_dependents<'a, I>(&self, names: I) -> BTreeSet<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for m in &self.members {
            for d in &m.deps {
                reverse.entry(d.as_str()).or_default().push(m.name.as_str());
            }
        }
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<&str> = names.into_iter().collect();
        while let Some(n) = stack.pop() {
            if out.insert(n.to_string()) {
                stack.extend(reverse.get(n).into_iter().flatten().copied());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkspaceGraph {
        let pkg = |name: &str, dir: &str, kinds: &[(&str, &str)], deps: &[(&str, bool)]| {
            serde_json::json!({
                "id": format!("path+file://{dir}#{name}@0.1.0"),
                "name": name,
                "version": "0.1.0",
                "manifest_path": format!("{dir}/Cargo.toml"),
                "targets": kinds.iter().map(|(k, n)| serde_json::json!({"kind": [k], "name": n})).collect::<Vec<_>>(),
                "dependencies": deps.iter().map(|(d, local)| if *local {
                    serde_json::json!({"name": d, "path": format!("/ws/crates/{d}")})
                } else {
                    serde_json::json!({"name": d})
                }).collect::<Vec<_>>(),
            })
        };
        let packages = vec![
            pkg(
                "core",
                "/ws/crates/core",
                &[("lib", "core")],
                &[("serde", false)],
            ),
            pkg("db", "/ws/crates/db", &[("lib", "db")], &[("core", true)]),
            pkg(
                "api",
                "/ws/apps/api",
                &[("bin", "api")],
                &[("db", true), ("core", true)],
            ),
            pkg("tool", "/ws/crates/core/tool", &[("bin", "tool")], &[]),
        ];
        let ids: Vec<Value> = packages.iter().map(|p| p["id"].clone()).collect();
        WorkspaceGraph::from_metadata(&serde_json::json!({
            "workspace_root": "/ws",
            "target_directory": "/ws/target",
            "workspace_members": ids,
            "packages": packages,
        }))
        .unwrap()
    }

    #[test]
    fn members_bins_and_local_deps() {
        let g = sample();
        assert_eq!(g.members.len(), 4);
        assert_eq!(g.bins().len(), 2);
        assert_eq!(g.bin_owner("api").unwrap().name, "api");
        assert!(g.member("core").unwrap().has_lib);
        assert!(
            g.member("core").unwrap().deps.is_empty(),
            "registry deps dropped"
        );
        assert_eq!(g.member("api").unwrap().deps.len(), 2);
    }

    #[test]
    fn path_maps_to_deepest_member() {
        let g = sample();
        let m = g
            .member_for_path(Path::new("/ws/crates/core/tool/src/main.rs"))
            .unwrap();
        assert_eq!(m.name, "tool");
        let m = g
            .member_for_path(Path::new("/ws/crates/core/src/lib.rs"))
            .unwrap();
        assert_eq!(m.name, "core");
        assert!(g.member_for_path(Path::new("/elsewhere/x.rs")).is_none());
    }

    #[test]
    fn dependents_are_transitive() {
        let g = sample();
        let set: Vec<String> = g.with_dependents(["core"]).into_iter().collect();
        assert_eq!(set, vec!["api", "core", "db"]);
        let set: Vec<String> = g.with_dependents(["api"]).into_iter().collect();
        assert_eq!(set, vec!["api"]);
    }

    #[test]
    fn loads_real_workspace() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project");
        let g = WorkspaceGraph::load(&fixture).unwrap();
        assert!(g.bin_owner("hello-server").is_some(), "{g:?}");
        let set = g.with_dependents(["greeter"]);
        assert!(set.contains("hello-server"), "{set:?}");
    }
}
