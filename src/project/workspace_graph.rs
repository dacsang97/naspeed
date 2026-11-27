//! Workspace layout from `cargo metadata --no-deps`: members, their binaries,
//! source dirs, and the reverse graph "crate → workspace crates that depend on
//! it" used to pick the tests affected by a change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Project language a normalized graph was exported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Go,
}

/// Adapter-owned language metadata retained on a normalized node. Pure
/// data — the engine interprets it; the adapter that produced the graph
/// decides what it means.
#[derive(Debug, Clone, Default)]
pub struct NodeMeta {
    /// Every file under `dir` is a compile input (Rust members: cargo
    /// rebuilds the crate when anything inside changes).
    pub owns_all_files: bool,
    /// File-name suffix marking a test-only input (`_test.go` for Go).
    pub test_suffix: Option<String>,
    /// `go:embed` patterns (as listed, `all:` prefix kept) for files under
    /// `dir` that count as compile inputs without being listed sources.
    pub embed_globs: Vec<String>,
    /// cgo side inputs (`.c`/`.h`/...) under `dir` are compile inputs.
    pub has_cgo: bool,
}

/// One package/module node, language-neutral.
#[derive(Debug, Clone)]
pub struct NormalizedNode {
    /// Crate name (Rust) or import path (Go).
    pub name: String,
    /// Absolute package directory.
    pub dir: PathBuf,
    /// Compile-time dependency edges (member names / import paths).
    pub deps: BTreeSet<String>,
    /// Test-only dependency edges (Go `TestImports`/`XTestImports`; empty
    /// for Rust where test deps ride the compile edges).
    pub test_deps: BTreeSet<String>,
    /// Shipped binary names this node produces.
    pub bins: Vec<String>,
    /// Absolute test-only input files.
    pub test_files: BTreeSet<PathBuf>,
    /// Absolute listed compile input files (Go's file lists; empty for
    /// Rust where `owns_all_files` covers it).
    pub build_files: BTreeSet<PathBuf>,
    pub meta: NodeMeta,
}

/// The normalized project graph both adapters export and the engine
/// consumes. All paths absolute.
#[derive(Debug, Clone)]
pub struct NormalizedGraph {
    pub root: PathBuf,
    pub language: Language,
    pub nodes: Vec<NormalizedNode>,
    /// Manifest/toolchain files whose change forces a full reload.
    pub manifests: BTreeSet<PathBuf>,
    /// Directories that are known module roots (for foreign-module
    /// detection with `module_marker`).
    pub module_dirs: BTreeSet<PathBuf>,
    /// Filename that makes a directory a module root (`go.mod`,
    /// `Cargo.toml`); `None` disables foreign-module detection.
    pub module_marker: Option<String>,
    /// Generated/output dirs whose contents never invalidate actions.
    pub ignored_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub version: String,
    pub edition: String,
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
                edition: pkg["edition"].as_str().unwrap_or("2015").to_string(),
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

    /// Every manifest/toolchain file whose content can change the graph:
    /// the workspace and member `Cargo.toml`s plus `Cargo.lock` and any
    /// rust-toolchain pin. Used by the engine's manifest rescan.
    pub fn manifest_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = vec![self.root.join("Cargo.toml")];
        for name in ["Cargo.lock", "rust-toolchain", "rust-toolchain.toml"] {
            let candidate = self.root.join(name);
            if candidate.is_file() {
                paths.push(candidate);
            }
        }
        for member in &self.members {
            paths.push(member.manifest_path.clone());
        }
        paths
    }

    /// Export the workspace as the engine's normalized graph. Member deps
    /// are already filtered to workspace members; every file under a
    /// member dir is a compile input (cargo rebuilds the crate on any of
    /// them), so `build_files`/`test_files` stay empty and `owns_all_files`
    /// carries the semantics.
    pub fn normalized(&self) -> NormalizedGraph {
        let nodes = self
            .members
            .iter()
            .map(|member| NormalizedNode {
                name: member.name.clone(),
                dir: member.dir.clone(),
                deps: member.deps.clone(),
                test_deps: BTreeSet::new(),
                bins: member.bins.clone(),
                test_files: BTreeSet::new(),
                build_files: BTreeSet::new(),
                meta: NodeMeta {
                    owns_all_files: true,
                    ..NodeMeta::default()
                },
            })
            .collect();
        NormalizedGraph {
            root: self.root.clone(),
            language: Language::Rust,
            nodes,
            manifests: self.manifest_paths().into_iter().collect(),
            module_dirs: self
                .members
                .iter()
                .map(|member| member.dir.clone())
                .collect(),
            module_marker: Some("Cargo.toml".to_string()),
            ignored_dirs: vec![self.target_dir.clone()],
        }
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
