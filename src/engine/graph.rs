//! Language-neutral project graph and affected-action planning.
//!
//! The graph does not rediscover dependencies: adapters export a
//! `NormalizedGraph` (defined in `project::workspace_graph`) that carries
//! the dependency edges, file ownership and language metadata each
//! language adapter already computed — `cargo metadata` for Rust, `go
//! list` for Go. The engine layer adds a stable action identity scheme
//! (package / compile / link / test / binary) and a [`Plan`] mapping a
//! change set to the exact actions it invalidates.
//!
//! Classification rules, applied at plan boundaries:
//!
//! - a change to a manifest/toolchain file reloads the whole graph;
//! - a compile-time input change invalidates the compile closure (the
//!   owning node plus everything that transitively depends on it) and the
//!   test closure (dependents through test-only edges too);
//! - a test-only input change invalidates only the test closure;
//! - a source-looking file outside every known package dir is treated as
//!   a manifest change — a new package may be forming, and a rescan is
//!   the only safe answer;
//! - files under a foreign module root (a nested `go.mod`/`Cargo.toml`
//!   the project does not own) or a generated dir never invalidate.
//!
//! Go specifics the normalized data preserves: `*_test.go` inputs are
//! test-only; unlisted embed/cgo side inputs count as compile inputs; a
//! missing `.go` file (delete) is a manifest change; test-only
//! dependency edges extend the test closure past the compile closure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::project::workspace_graph::{NormalizedGraph, NormalizedNode, WorkspaceGraph};
pub use crate::project::workspace_graph::Language;

use super::digest_memo::{DigestMemo, FileProbe};

/// Kind of action a node can produce. Stable across sessions: it is the
/// identity phases 5+ will key executions and cache records by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionKind {
    Package,
    Compile,
    Link,
    Test,
    Binary,
}

impl ActionKind {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            ActionKind::Package => b'P',
            ActionKind::Compile => b'C',
            ActionKind::Link => b'L',
            ActionKind::Test => b'T',
            ActionKind::Binary => b'B',
        }
    }

    pub fn from_tag(tag: u8) -> Option<ActionKind> {
        match tag {
            b'P' => Some(ActionKind::Package),
            b'C' => Some(ActionKind::Compile),
            b'L' => Some(ActionKind::Link),
            b'T' => Some(ActionKind::Test),
            b'B' => Some(ActionKind::Binary),
            _ => None,
        }
    }
}

/// `kind:name`, e.g. `C:example.test/svc/internal/greet`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId {
    pub kind: ActionKind,
    pub name: String,
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind.tag() as char, self.name)
    }
}

/// What a [`Plan`] decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The graph must be reloaded (manifest/toolchain shape changed);
    /// `actions` then contains every action in the graph.
    pub reload: bool,
    /// Invalidated actions, deduplicated and sorted.
    pub actions: BTreeSet<ActionId>,
    /// Changed inputs that matched nothing (ignored/generated paths).
    pub ignored: usize,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        !self.reload && self.actions.is_empty()
    }
}

/// Manifest/toolchain basenames that force a reload wherever they appear
/// under the root (a new one appearing means a new package/module).
const MANIFEST_BASENAMES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    "go.mod",
    "go.sum",
    "go.work",
    "go.work.sum",
];

/// Extensions that, when found outside every package dir, mean "maybe a
/// new package" — reload rather than guess.
const SOURCE_EXTENSIONS: &[&str] = &["rs", "go"];

/// cgo side-input extensions (Go): an unlisted file under a cgo package
/// dir with one of these is a compile input, mirroring `go list`.
const CGO_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "m", "mm", "f", "for", "f90", "s", "syso",
];

fn is_manifest_basename(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| MANIFEST_BASENAMES.contains(&n))
}

/// Canonicalize the longest existing prefix of `path`, reattaching any
/// tail that doesn't exist yet — callers hand us deleted files too, and
/// `canonicalize` fails on those whole. Result is comparable against
/// `EngineGraph` roots/dirs, which are canonicalized the same way.
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(canon) = dunce::canonicalize(path) {
        return canon;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur: &Path = path;
    loop {
        match cur.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                match cur.parent() {
                    Some(parent) => cur = parent,
                    None => return path.to_path_buf(),
                }
            }
            None => return path.to_path_buf(),
        }
        if let Ok(canon) = dunce::canonicalize(cur) {
            let mut out = canon;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
    }
}

fn extension(path: &Path) -> &str {
    path.extension().and_then(|e| e.to_str()).unwrap_or("")
}

/// Live project graph: normalized adapter data plus compiled side
/// indexes. `meta` on each node keeps the adapter-owned language fields.
pub struct EngineGraph {
    pub root: PathBuf,
    pub store_root: PathBuf,
    pub language: Language,
    graph: NormalizedGraph,
    nodes: BTreeMap<String, NormalizedNode>,
    /// Deepest-first dirs for owner lookup.
    by_dir: Vec<(PathBuf, String)>,
    /// Compiled embed globs per node index (Go; empty for Rust).
    embeds: BTreeMap<String, GlobSet>,
    /// Reverse compile edges: dep name -> dependent names.
    compile_reverse: BTreeMap<String, Vec<String>>,
    /// Reverse test edges (compile ∪ test-only deps).
    test_reverse: BTreeMap<String, Vec<String>>,
}

impl EngineGraph {
    /// Rust workspace first, then Go — but only when `dir` actually lives
    /// inside the workspace `cargo metadata` found. `cargo metadata`
    /// climbs ancestor directories, so a Go project sitting inside a Rust
    /// repo would otherwise be silently claimed by the outer workspace.
    pub fn load(dir: &Path, store_root: &Path) -> Result<EngineGraph> {
        match WorkspaceGraph::load(dir) {
            Ok(graph) => {
                let dir_canon = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                let root_canon =
                    dunce::canonicalize(&graph.root).unwrap_or_else(|_| graph.root.clone());
                if dir_canon.starts_with(&root_canon) {
                    return Ok(Self::from_normalized(graph.normalized(), store_root));
                }
                let go = crate::dev::go_dev::GoDev::load(dir)
                    .context("dir is outside the enclosing cargo workspace and not a Go project")?;
                Ok(Self::from_normalized(go.graph.normalized(), store_root))
            }
            Err(cargo_err) => {
                let go = crate::dev::go_dev::GoDev::load(dir)
                    .with_context(|| format!("not a Cargo or Go project ({cargo_err:#})"))?;
                Ok(Self::from_normalized(go.graph.normalized(), store_root))
            }
        }
    }

    /// Wrap an adapter-exported normalized graph. Every stored path is
    /// canonicalized (leniently — deleted/absent manifests keep their
    /// name) so prefix checks match whatever form change sources report.
    pub fn from_normalized(mut graph: NormalizedGraph, store_root: &Path) -> EngineGraph {
        graph.root = canonicalize_lenient(&graph.root);
        for node in &mut graph.nodes {
            node.dir = canonicalize_lenient(&node.dir);
            node.test_files = node
                .test_files
                .iter()
                .map(|p| canonicalize_lenient(p))
                .collect();
            node.build_files = node
                .build_files
                .iter()
                .map(|p| canonicalize_lenient(p))
                .collect();
        }
        graph.manifests = graph
            .manifests
            .iter()
            .map(|p| canonicalize_lenient(p))
            .collect();
        graph.module_dirs = graph
            .module_dirs
            .iter()
            .map(|p| canonicalize_lenient(p))
            .collect();
        graph.ignored_dirs = graph
            .ignored_dirs
            .iter()
            .map(|p| canonicalize_lenient(p))
            .collect();
        let mut nodes = BTreeMap::new();
        let mut embeds = BTreeMap::new();
        let mut compile_reverse: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut test_reverse: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in &graph.nodes {
            for dep in &node.deps {
                compile_reverse
                    .entry(dep.clone())
                    .or_default()
                    .push(node.name.clone());
                test_reverse
                    .entry(dep.clone())
                    .or_default()
                    .push(node.name.clone());
            }
            for dep in &node.test_deps {
                if node.deps.contains(dep) {
                    continue;
                }
                test_reverse
                    .entry(dep.clone())
                    .or_default()
                    .push(node.name.clone());
            }
            if !node.meta.embed_globs.is_empty() {
                match compile_embeds(&node.meta.embed_globs) {
                    Ok(set) => {
                        embeds.insert(node.name.clone(), set);
                    }
                    Err(err) => {
                        tracing::warn!(
                            "engine: bad embed patterns on {}: {err:#}",
                            node.name
                        );
                    }
                }
            }
            nodes.insert(node.name.clone(), node.clone());
        }
        let mut by_dir: Vec<(PathBuf, String)> = nodes
            .values()
            .map(|n| (n.dir.clone(), n.name.clone()))
            .collect();
        by_dir.sort_by(|a, b| b.0.components().count().cmp(&a.0.components().count()));
        EngineGraph {
            root: graph.root.clone(),
            store_root: canonicalize_lenient(store_root),
            language: graph.language,
            graph,
            nodes,
            by_dir,
            embeds,
            compile_reverse,
            test_reverse,
        }
    }

    /// Prime the memo with current manifest signatures after a (re)load so
    /// the first plan doesn't report a spurious reload for manifests that
    /// were merely *read* to build this graph.
    pub fn prime(&self, memo: &mut DigestMemo) {
        for path in &self.graph.manifests {
            memo.prime(path);
        }
    }

    /// Absolute manifest/toolchain paths tracked for rescan.
    pub fn manifests(&self) -> &BTreeSet<PathBuf> {
        &self.graph.manifests
    }

    /// Every action id in the graph (used for reload plans and status).
    pub fn all_actions(&self) -> BTreeSet<ActionId> {
        let mut out = BTreeSet::new();
        for node in self.nodes.values() {
            Self::node_actions(&mut out, node, true, true);
        }
        out
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn node_actions(out: &mut BTreeSet<ActionId>, node: &NormalizedNode, compile: bool, test: bool) {
        if compile {
            out.insert(ActionId { kind: ActionKind::Package, name: node.name.clone() });
            out.insert(ActionId { kind: ActionKind::Compile, name: node.name.clone() });
            for bin in &node.bins {
                out.insert(ActionId { kind: ActionKind::Link, name: bin.clone() });
                out.insert(ActionId { kind: ActionKind::Binary, name: bin.clone() });
            }
        }
        if test {
            out.insert(ActionId { kind: ActionKind::Test, name: node.name.clone() });
        }
    }

    /// Deepest package dir containing `path`.
    fn owner_of(&self, path: &Path) -> Option<&str> {
        self.by_dir
            .iter()
            .find(|(dir, _)| path.starts_with(dir))
            .map(|(_, name)| name.as_str())
    }

    /// `path` sits under a foreign module root: an ancestor dir below the
    /// project root carries a `module_marker` manifest but is not a known
    /// module — the project graph does not see inside it.
    fn in_foreign_module(&self, path: &Path) -> bool {
        let Some(marker) = &self.graph.module_marker else {
            return false;
        };
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == self.root || !dir.starts_with(&self.root) {
                return false;
            }
            if dir.join(marker).is_file() && !self.graph.module_dirs.contains(dir) {
                return true;
            }
            current = dir.parent();
        }
        false
    }

    fn ignored(&self, path: &Path) -> bool {
        if !path.starts_with(&self.root) {
            return true;
        }
        self.graph.ignored_dirs.iter().any(|dir| path.starts_with(dir))
            || path.components().any(|c| {
                matches!(c, Component::Normal(n) if n == ".git" || n == "node_modules")
            })
    }

    /// Unlisted compile side inputs (Go): embed globs and cgo sources.
    fn unlisted_compile_input(&self, node: &NormalizedNode, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&node.dir) else {
            return false;
        };
        if self
            .embeds
            .get(&node.name)
            .is_some_and(|set| set.is_match(rel))
        {
            return true;
        }
        node.meta.has_cgo
            && CGO_EXTENSIONS
                .iter()
                .any(|c| extension(path).eq_ignore_ascii_case(c))
    }

    /// Classify one changed absolute path. Order follows the adapters:
    /// manifest (incl. any manifest basename appearing where the graph
    /// doesn't know it — a new package forming or one deleted) → foreign
    /// module → test input → deleted `.go` → owned input → `.go` new
    /// file → unlisted side input → ignored.
    fn classify(&self, path: &Path, exists: bool) -> Classification {
        if self.graph.manifests.contains(path) || is_manifest_basename(path) {
            return Classification::Reload;
        }
        if self.in_foreign_module(path) {
            return Classification::Ignored;
        }
        let owner = self.owner_of(path).and_then(|name| self.nodes.get(name));
        if let Some(node) = owner
            && (node.test_files.contains(path)
                || node.meta.test_suffix.as_deref().is_some_and(|suffix| {
                    path.file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with(suffix))
                }))
        {
            return Classification::Test(node.name.clone());
        }
        if self.language == Language::Go && extension(path) == "go" && !exists {
            // A deleted .go file can change the package's file list.
            return Classification::Reload;
        }
        if let Some(node) = owner {
            if node.build_files.contains(path) || node.meta.owns_all_files {
                return Classification::Compile(node.name.clone());
            }
            if extension(path) == "go" {
                return Classification::Compile(node.name.clone());
            }
            if self.unlisted_compile_input(node, path) {
                return Classification::Compile(node.name.clone());
            }
            return Classification::Ignored;
        }
        if SOURCE_EXTENSIONS.contains(&extension(path)) || is_manifest_basename(path) {
            return Classification::Reload;
        }
        Classification::Ignored
    }

    /// Rescan manifest signatures: cheap `stat`s at each plan boundary so
    /// a lost or coalesced watch event cannot leave a stale graph.
    fn manifests_changed(&self, memo: &mut DigestMemo) -> bool {
        self.graph
            .manifests
            .iter()
            .any(|path| memo.probe(path) != FileProbe::Same)
    }

    /// Turn a change set into an invalidation plan. `changes` are absolute
    /// paths (watcher batches, journal entries, or an explicit rescan).
    pub fn plan(&self, changes: &[PathBuf], memo: &mut DigestMemo) -> Plan {
        if self.manifests_changed(memo) {
            return Plan { reload: true, actions: self.all_actions(), ignored: 0 };
        }
        let mut compile_seeds = BTreeSet::new();
        let mut test_seeds = BTreeSet::new();
        let mut ignored = 0;
        for path in changes {
            let path = &canonicalize_lenient(path);
            if self.ignored(path) {
                ignored += 1;
                continue;
            }
            match self.classify(path, path.exists()) {
                Classification::Reload => {
                    return Plan {
                        reload: true,
                        actions: self.all_actions(),
                        ignored,
                    };
                }
                Classification::Compile(name) => {
                    compile_seeds.insert(name);
                }
                Classification::Test(name) => {
                    test_seeds.insert(name);
                }
                Classification::Ignored => ignored += 1,
            }
        }
        let mut actions = BTreeSet::new();
        let compile = self.closure(&self.compile_reverse, &compile_seeds);
        let test = self.closure(
            &self.test_reverse,
            &compile_seeds.iter().chain(&test_seeds).cloned().collect(),
        );
        for name in &compile {
            if let Some(node) = self.nodes.get(name) {
                Self::node_actions(&mut actions, node, true, true);
            }
        }
        for name in &test {
            if !compile.contains(name)
                && let Some(node) = self.nodes.get(name)
            {
                Self::node_actions(&mut actions, node, false, true);
            }
        }
        Plan { reload: false, actions, ignored }
    }

    /// `seeds` plus every node that transitively depends on them,
    /// following `reverse` (precomputed per-edge-kind).
    fn closure(
        &self,
        reverse: &BTreeMap<String, Vec<String>>,
        seeds: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let mut out = seeds.clone();
        let mut stack: Vec<String> = seeds.iter().cloned().collect();
        while let Some(name) = stack.pop() {
            for dependent in reverse.get(&name).into_iter().flatten() {
                if out.insert(dependent.clone()) {
                    stack.push(dependent.clone());
                }
            }
        }
        out
    }
}

enum Classification {
    Reload,
    Compile(String),
    Test(String),
    Ignored,
}

/// Same compilation rule `go_dev` uses: every pattern plus its subtree
/// descendants, `all:` prefix stripped, literal separators.
fn compile_embeds(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for original in patterns {
        let pattern = original.strip_prefix("all:").unwrap_or(original);
        for glob_pattern in [pattern.to_string(), format!("{pattern}/**")] {
            let glob = GlobBuilder::new(&glob_pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .with_context(|| format!("invalid go:embed pattern {original:?}"))?;
            builder.add(glob);
        }
    }
    builder.build().context("compile go:embed patterns")
}
