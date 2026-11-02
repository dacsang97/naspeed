//! Filesystem watching for `ns dev`: `notify` (FSEvents / inotify) events,
//! filtered by the workspace `.gitignore`, `[dev].ignore` and the dirs that
//! are always skipped (`target*`, `.git`, `node_modules`), then debounced so
//! an event storm (formatter, checkout, `touch` of many files) becomes one
//! batch.

use super::log_bus::LogBus;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period that closes a batch of changes.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Decides which changed paths matter. Paths are judged relative to the
/// workspace root, so a workspace that itself lives under some `target/`
/// directory (e.g. a test sandbox) still works.
pub struct WatchFilter {
    /// The root as given plus its canonical form (FSEvents reports
    /// canonical paths such as `/private/var/...`).
    roots: Vec<PathBuf>,
    /// Watched subtrees, relative to the root; an empty path is the root itself.
    dirs: Vec<PathBuf>,
    /// Individual root-relative files watched outside `dirs` (Cargo.toml, .env...).
    files: Vec<PathBuf>,
    ignore: Gitignore,
}

fn always_skipped(component: &Component) -> bool {
    let Component::Normal(name) = component else {
        return false;
    };
    let name = name.to_string_lossy();
    name.starts_with("target") || name == ".git" || name == "node_modules"
}

impl WatchFilter {
    pub fn new(
        root: &Path,
        dirs: &[PathBuf],
        files: &[PathBuf],
        ignore_globs: &[String],
    ) -> Result<WatchFilter> {
        let mut builder = GitignoreBuilder::new(root);
        let gitignore = root.join(".gitignore");
        if gitignore.is_file()
            && let Some(err) = builder.add(&gitignore)
        {
            return Err(anyhow!("{err}")).with_context(|| format!("read {}", gitignore.display()));
        }
        for glob in ignore_globs {
            builder
                .add_line(None, glob)
                .with_context(|| format!("invalid [dev].ignore pattern {glob:?}"))?;
        }
        let ignore = builder.build().context("build ignore rules")?;
        let mut roots = vec![root.to_path_buf()];
        if let Ok(canonical) = root.canonicalize()
            && canonical != root
        {
            roots.push(canonical);
        }
        let clean = |p: &PathBuf| {
            p.components()
                .filter(|c| *c != Component::CurDir)
                .collect::<PathBuf>()
        };
        Ok(WatchFilter {
            roots,
            dirs: dirs.iter().map(clean).collect(),
            files: files.iter().map(clean).collect(),
            ignore,
        })
    }

    /// The path relative to the root when a change to it should be acted on.
    pub fn relevant(&self, path: &Path) -> Option<PathBuf> {
        let rel = self.roots.iter().find_map(|r| path.strip_prefix(r).ok())?;
        if rel.as_os_str().is_empty() || rel.components().any(|c| always_skipped(&c)) {
            return None;
        }
        let explicitly_listed = self.files.iter().any(|file| file == rel);
        let listed = explicitly_listed || self.dirs.iter().any(|dir| rel.starts_with(dir));
        if !listed
            || (!explicitly_listed
                && self
                    .ignore
                    .matched_path_or_any_parents(rel, path.is_dir())
                    .is_ignore())
        {
            return None;
        }
        Some(rel.to_path_buf())
    }
}

/// Live watcher; dropping it stops watching.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<notify::Event>>,
    filter: WatchFilter,
    root: PathBuf,
    bus: LogBus,
}

impl FileWatcher {
    /// Watches `dirs` recursively and the root itself non-recursively (for
    /// the root-level files the filter lists).
    pub fn start(
        root: &Path,
        dirs: &[PathBuf],
        filter: WatchFilter,
        bus: LogBus,
    ) -> Result<FileWatcher> {
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |ev| {
            let _ = tx.send(ev);
        })
        .context("create file watcher")?;
        let mut root_recursive = false;
        let mut watched_paths = BTreeSet::new();
        for dir in dirs {
            let abs = root.join(dir);
            if !abs.is_dir() {
                super::status_line::warn(
                    &bus,
                    format!("watch path {} does not exist; skipped", abs.display()),
                );
                continue;
            }
            root_recursive |= abs == root || dir.as_os_str().is_empty() || dir == Path::new(".");
            watcher
                .watch(&abs, RecursiveMode::Recursive)
                .with_context(|| format!("watch {}", abs.display()))?;
            watched_paths.insert(abs);
        }
        if !root_recursive {
            watcher
                .watch(root, RecursiveMode::NonRecursive)
                .with_context(|| format!("watch {}", root.display()))?;
            watched_paths.insert(root.to_path_buf());
        }
        if !root_recursive {
            for file in &filter.files {
                if dirs.iter().any(|dir| file.starts_with(dir)) {
                    continue;
                }
                let parent = file.parent().unwrap_or(Path::new(""));
                let abs_parent = root.join(parent);
                if watched_paths.contains(&abs_parent) || !abs_parent.is_dir() {
                    continue;
                }
                watcher
                    .watch(&abs_parent, RecursiveMode::NonRecursive)
                    .with_context(|| format!("watch {}", abs_parent.display()))?;
                watched_paths.insert(abs_parent);
            }
        }
        Ok(FileWatcher {
            _watcher: watcher,
            rx,
            filter,
            root: root.to_path_buf(),
            bus,
        })
    }

    /// Adds the relevant paths of one notify message to `into` (absolute,
    /// under the root as given). Reads/opens are not changes.
    fn absorb(&self, msg: notify::Result<notify::Event>, into: &mut BTreeSet<PathBuf>) -> bool {
        let event = match msg {
            Ok(event) => event,
            Err(err) => {
                super::status_line::warn(&self.bus, format!("file watcher: {err}"));
                return false;
            }
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        let mut added = false;
        for path in &event.paths {
            if let Some(rel) = self.filter.relevant(path) {
                added |= into.insert(self.root.join(rel));
            }
        }
        added
    }

    /// Records every change already queued, without waiting.
    pub fn drain(&self, into: &mut BTreeSet<PathBuf>) -> Result<()> {
        loop {
            match self.rx.try_recv() {
                Ok(msg) => {
                    self.absorb(msg, into);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(anyhow!("file watcher stopped"));
                }
            }
        }
    }

    /// Keeps collecting into `into` until no relevant change arrived for
    /// [`DEBOUNCE`].
    pub fn settle(&self, into: &mut BTreeSet<PathBuf>) -> Result<()> {
        let mut quiet_since = Instant::now();
        loop {
            let left = DEBOUNCE.saturating_sub(quiet_since.elapsed());
            if left.is_zero() {
                return Ok(());
            }
            match self.rx.recv_timeout(left) {
                Ok(msg) => {
                    if self.absorb(msg, into) {
                        quiet_since = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("file watcher stopped")),
            }
        }
    }

    /// Waits up to `timeout` for a first relevant change, then debounces.
    /// Returns an empty set when nothing changed.
    pub fn next_batch(&self, timeout: Duration) -> Result<BTreeSet<PathBuf>> {
        let mut batch = BTreeSet::new();
        let deadline = Instant::now() + timeout;
        while batch.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(batch);
            }
            match self.rx.recv_timeout(left) {
                Ok(msg) => {
                    self.absorb(msg, &mut batch);
                }
                Err(RecvTimeoutError::Timeout) => return Ok(batch),
                Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("file watcher stopped")),
            }
        }
        self.settle(&mut batch)?;
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(dir: &Path) -> WatchFilter {
        std::fs::write(dir.join(".gitignore"), "generated/\n*.log\n").unwrap();
        WatchFilter::new(
            dir,
            &[PathBuf::from("crates"), PathBuf::from("./apps")],
            &[PathBuf::from("Cargo.toml"), PathBuf::from(".env")],
            &["**/*.md".to_string()],
        )
        .unwrap()
    }

    #[test]
    fn filters_by_location_and_ignore_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let f = filter(root);
        let yes = |p: &str| f.relevant(&root.join(p)).is_some();
        assert!(yes("crates/core/src/lib.rs"));
        assert!(yes("apps/api/templates/index.html"));
        assert!(yes("Cargo.toml"));
        assert!(yes(".env"));
        assert!(!yes("README.md"), "root file not listed");
        assert!(!yes("crates/core/README.md"), "[dev].ignore");
        assert!(!yes("crates/core/generated/x.rs"), ".gitignore dir");
        assert!(!yes("apps/api/server.log"), ".gitignore glob");
        assert!(!yes("crates/core/target/debug/x"), "target always skipped");
        assert!(!yes("crates/target-x86/a.rs"), "target* always skipped");
        assert!(!yes("apps/web/node_modules/a/index.js"));
        assert!(!yes(".git/HEAD"));
        assert!(f.relevant(Path::new("/elsewhere/crates/a.rs")).is_none());
    }

    #[test]
    fn explicitly_watched_files_override_ignore_rules_but_not_always_skipped_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "config/\n").unwrap();
        let filter = WatchFilter::new(
            root,
            &[],
            &[
                PathBuf::from("config/api.env"),
                PathBuf::from("target/api.env"),
                PathBuf::from(".git/api.env"),
                PathBuf::from("node_modules/api.env"),
            ],
            &["config/api.env".to_string()],
        )
        .unwrap();

        assert!(filter.relevant(&root.join("config/api.env")).is_some());
        assert!(filter.relevant(&root.join("target/api.env")).is_none());
        assert!(filter.relevant(&root.join(".git/api.env")).is_none());
        assert!(
            filter
                .relevant(&root.join("node_modules/api.env"))
                .is_none()
        );
    }

    #[test]
    fn root_inside_a_target_dir_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("target/tmp/ws");
        std::fs::create_dir_all(&root).unwrap();
        let f = filter(&root);
        assert!(f.relevant(&root.join("crates/a/src/lib.rs")).is_some());
    }

    #[test]
    fn storm_of_changes_is_one_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        let f = filter(&root);
        let (bus, _rx) = LogBus::channel();
        let w = FileWatcher::start(&root, &[PathBuf::from("crates")], f, bus).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        for i in 0..50 {
            std::fs::write(root.join(format!("crates/a/f{i}.rs")), "x").unwrap();
        }
        let batch = w.next_batch(Duration::from_secs(5)).unwrap();
        assert!(batch.len() >= 40, "got {} paths", batch.len());
        assert!(w.next_batch(Duration::from_millis(500)).unwrap().is_empty());
    }

    #[test]
    fn watches_nested_whitelisted_files_outside_recursive_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/api")).unwrap();
        std::fs::create_dir_all(root.join("config")).unwrap();
        let env_file = root.join("config/api.env");
        std::fs::write(&env_file, "PORT=1\n").unwrap();
        let filter = WatchFilter::new(
            &root,
            &[PathBuf::from("crates")],
            &[PathBuf::from("Cargo.toml"), PathBuf::from("config/api.env")],
            &[],
        )
        .unwrap();
        let (bus, _rx) = LogBus::channel();
        let watcher = FileWatcher::start(&root, &[PathBuf::from("crates")], filter, bus).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(&env_file, "PORT=2\n").unwrap();
        let changes = watcher.next_batch(Duration::from_secs(3)).unwrap();
        assert_eq!(changes, [env_file].into_iter().collect());
    }
}
