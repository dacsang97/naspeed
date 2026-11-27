//! Filesystem watching for `ns dev`: `notify` (FSEvents / inotify) events,
//! filtered by the workspace `.gitignore`, `[dev].ignore` and the dirs that
//! are always skipped (`target*`, `.git`, `node_modules`), then debounced so
//! an event storm (formatter, checkout, `touch` of many files) becomes one
//! batch.

use super::log_bus::LogBus;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period that closes a batch of changes.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Decides which changed paths matter. Paths are judged relative to the
/// workspace root, so a workspace that itself lives under some `target/`
/// directory (e.g. a test sandbox) still works.
#[derive(Clone)]
pub struct WatchFilter {
    /// The root as given plus its canonical form (FSEvents reports
    /// canonical paths such as `/private/var/...`).
    roots: Vec<PathBuf>,
    /// Watched subtrees, relative to the root; an empty path is the root itself.
    dirs: Vec<PathBuf>,
    /// Individual root-relative files watched outside `dirs` (Cargo.toml, .env...).
    files: Vec<PathBuf>,
    ignore: Gitignore,
    extra_skipped: BTreeSet<OsString>,
    skip_target_prefix: bool,
}

fn always_skipped(component: &Component) -> bool {
    let Component::Normal(name) = component else {
        return false;
    };
    let name = name.to_string_lossy();
    name == ".git" || name == "node_modules"
}

impl WatchFilter {
    #[cfg(test)]
    pub fn new(
        root: &Path,
        dirs: &[PathBuf],
        files: &[PathBuf],
        ignore_globs: &[String],
    ) -> Result<WatchFilter> {
        Self::new_with_skips(root, dirs, files, ignore_globs, &["target*".into()])
    }

    pub fn new_with_skips(
        root: &Path,
        dirs: &[PathBuf],
        files: &[PathBuf],
        ignore_globs: &[String],
        extra_skipped: &[String],
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
        let skip_target_prefix = extra_skipped.iter().any(|name| name == "target*");
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
            extra_skipped: extra_skipped.iter().map(OsString::from).collect(),
            skip_target_prefix,
        })
    }

    /// The path relative to the root when a change to it should be acted on.
    pub fn relevant(&self, path: &Path) -> Option<PathBuf> {
        let rel = self.roots.iter().find_map(|r| path.strip_prefix(r).ok())?;
        if rel.as_os_str().is_empty()
            || rel.components().any(|component| {
                always_skipped(&component)
                    || matches!(component, Component::Normal(name)
                        if self.extra_skipped.contains(name)
                            || (self.skip_target_prefix
                                && name.to_string_lossy().starts_with("target")))
            })
        {
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

type FileMetadata = (u64, SystemTime);

#[derive(Default)]
struct FileSnapshot(BTreeMap<PathBuf, FileMetadata>);

impl FileSnapshot {
    fn build(root: &Path, filter: &WatchFilter) -> FileSnapshot {
        let mut snapshot = FileSnapshot::default();
        for dir in &filter.dirs {
            snapshot.scan_dir(root, dir, filter);
        }
        for file in &filter.files {
            snapshot.record(root, file, filter);
        }
        snapshot
    }

    fn scan_dir(&mut self, root: &Path, dir: &Path, filter: &WatchFilter) {
        let abs = root.join(dir);
        if !abs.is_dir() {
            return;
        }
        let walk_filter = filter.clone();
        let mut builder = WalkBuilder::new(&abs);
        builder
            .standard_filters(false)
            .hidden(false)
            .follow_links(false)
            .filter_entry(move |entry| {
                entry.depth() == 0 || walk_filter.relevant(entry.path()).is_some()
            });
        for entry in builder.build().filter_map(std::result::Result::ok) {
            self.record(root, entry.path(), filter);
        }
    }

    fn record(&mut self, root: &Path, path: &Path, filter: &WatchFilter) {
        let abs = root.join(path);
        let Some(rel) = filter.relevant(&abs) else {
            return;
        };
        let Ok(metadata) = abs.symlink_metadata() else {
            return;
        };
        if (metadata.file_type().is_file() || metadata.file_type().is_symlink())
            && let Ok(modified) = metadata.modified()
        {
            self.0.insert(rel, (metadata.len(), modified));
        }
    }

    fn changed_paths(&self, next: &FileSnapshot, root: &Path) -> BTreeSet<PathBuf> {
        let modified = next
            .0
            .iter()
            .filter(|(path, metadata)| self.0.get(*path) != Some(*metadata))
            .map(|(path, _)| path);
        let removed = self.0.keys().filter(|path| !next.0.contains_key(*path));
        modified
            .chain(removed)
            .map(|path| root.join(path))
            .collect()
    }

    fn refresh(&mut self, root: &Path, paths: &BTreeSet<PathBuf>, filter: &WatchFilter) {
        for path in paths {
            let Some(rel) = filter.relevant(path) else {
                continue;
            };
            self.0
                .retain(|known, _| known != &rel && !known.starts_with(&rel));
            if path.is_dir() {
                self.scan_dir(root, &rel, filter);
            } else {
                self.record(root, &rel, filter);
            }
        }
    }
}

/// One debounced watcher batch, root-relative. The journal only observes;
/// nothing in `ns dev` acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    /// Root-relative paths that changed. Empty when the batch was a
    /// rescan that produced no visible diff.
    pub paths: BTreeSet<PathBuf>,
    /// A rescan (event queue overflow etc.) produced this batch — treat
    /// any missing paths as unknown, not unchanged.
    pub rescan: bool,
}

/// Bounded in-memory journal of watcher batches. The graph service (phase
/// 4+) drains it to keep its change set current between plans; `ns dev`
/// itself does not consume it yet.
#[derive(Default)]
pub struct ChangeJournal(VecDeque<ChangeBatch>);

/// Oldest batches dropped beyond this depth — the service polls between
/// plans and the journal exists to cover plan boundaries, not history.
pub const JOURNAL_CAPACITY: usize = 1024;

#[allow(dead_code)] // drained by the graph service, not `ns dev` yet.
impl ChangeJournal {
    fn push(&mut self, batch: ChangeBatch) {
        self.0.push_back(batch);
        while self.0.len() > JOURNAL_CAPACITY {
            self.0.pop_front();
        }
    }

    /// All batches since the last drain, in order.
    pub fn drain(&mut self) -> Vec<ChangeBatch> {
        self.0.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

struct WatchState {
    snapshot: FileSnapshot,
    rescan_pending: bool,
    journal: ChangeJournal,
}

/// Live watcher; dropping it stops watching.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<notify::Event>>,
    filter: WatchFilter,
    root: PathBuf,
    state: RefCell<WatchState>,
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
        let snapshot = FileSnapshot::build(root, &filter);
        Ok(FileWatcher {
            _watcher: watcher,
            rx,
            filter,
            root: root.to_path_buf(),
            state: RefCell::new(WatchState {
                snapshot,
                rescan_pending: false,
                journal: ChangeJournal::default(),
            }),
            bus,
        })
    }

    /// Adds the relevant paths of one notify message to `into` (absolute,
    /// under the root as given). Reads/opens are not changes. Returns whether
    /// the message should extend the debounce window.
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
        if event.need_rescan() {
            self.state.borrow_mut().rescan_pending = true;
            return true;
        }
        let mut added = false;
        for path in &event.paths {
            if let Some(rel) = self.filter.relevant(path) {
                added |= into.insert(self.root.join(rel));
            }
        }
        added
    }

    fn journal_batch(&self, paths: &BTreeSet<PathBuf>, rescan: bool) {
        if paths.is_empty() && !rescan {
            return;
        }
        let rel: BTreeSet<PathBuf> = paths
            .iter()
            .filter_map(|p| p.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .collect();
        self.state
            .borrow_mut()
            .journal
            .push(ChangeBatch { paths: rel, rescan });
    }

    fn finish_batch(&self, into: &mut BTreeSet<PathBuf>) {
        let rescan_pending = {
            let mut state = self.state.borrow_mut();
            std::mem::take(&mut state.rescan_pending)
        };
        if rescan_pending {
            let next = FileSnapshot::build(&self.root, &self.filter);
            let mut state = self.state.borrow_mut();
            into.extend(state.snapshot.changed_paths(&next, &self.root));
            state.snapshot = next;
        } else {
            self.state
                .borrow_mut()
                .snapshot
                .refresh(&self.root, into, &self.filter);
        }
        self.journal_batch(into, rescan_pending);
    }

    /// Observation-only journal of every settled/drained batch since the
    /// last call. `ns dev` does not consume this; the graph service does.
    #[allow(dead_code)] // drained by the graph service, not `ns dev` yet.
    pub fn drain_journal(&self) -> Vec<ChangeBatch> {
        self.state.borrow_mut().journal.drain()
    }

    /// Records every change already queued, without waiting.
    pub fn drain(&self, into: &mut BTreeSet<PathBuf>) -> Result<()> {
        let mut delta = BTreeSet::new();
        loop {
            match self.rx.try_recv() {
                Ok(msg) => {
                    self.absorb(msg, &mut delta);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(anyhow!("file watcher stopped"));
                }
            }
        }
        into.extend(delta.iter().cloned());
        self.journal_batch(&delta, false);
        Ok(())
    }

    /// Keeps collecting into `into` until no relevant change arrived for
    /// [`DEBOUNCE`], then updates the filesystem snapshot.
    pub fn settle(&self, into: &mut BTreeSet<PathBuf>) -> Result<()> {
        let mut quiet_since = Instant::now();
        loop {
            let left = DEBOUNCE.saturating_sub(quiet_since.elapsed());
            if left.is_zero() {
                break;
            }
            match self.rx.recv_timeout(left) {
                Ok(msg) => {
                    if self.absorb(msg, into) {
                        quiet_since = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("file watcher stopped"));
                }
            }
        }
        self.finish_batch(into);
        Ok(())
    }

    /// Waits up to `timeout` for a first relevant change or rescan, then
    /// debounces and resolves the batch against the filesystem snapshot.
    /// Returns an empty set when nothing changed.
    pub fn next_batch(&self, timeout: Duration) -> Result<BTreeSet<PathBuf>> {
        let mut batch = BTreeSet::new();
        let deadline = Instant::now() + timeout;
        let mut active = self.state.borrow().rescan_pending;
        while !active {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(batch);
            }
            match self.rx.recv_timeout(left) {
                Ok(msg) => {
                    active = self.absorb(msg, &mut batch);
                }
                Err(RecvTimeoutError::Timeout) => return Ok(batch),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("file watcher stopped"));
                }
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

    /// Swallow events for changes made before the watcher started. FSEvents
    /// can deliver those (e.g. the creation of the watched dirs) a moment
    /// after start, where they would land in the batch under test.
    fn drain_startup_events(watcher: &FileWatcher) {
        for _ in 0..10 {
            if watcher
                .next_batch(Duration::from_millis(300))
                .unwrap()
                .is_empty()
            {
                return;
            }
        }
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
    fn go_skips_do_not_hide_target_prefixed_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let filter = WatchFilter::new_with_skips(
            root,
            &[PathBuf::new()],
            &[],
            &[],
            &["vendor".into(), "testdata".into(), ".ns-dev".into()],
        )
        .unwrap();
        assert!(
            filter
                .relevant(&root.join("internal/targets/package.go"))
                .is_some()
        );
        assert!(
            filter
                .relevant(&root.join("cmd/target-sync/main.go"))
                .is_some()
        );
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
        drain_startup_events(&watcher);

        std::fs::write(&env_file, "PORT=2\n").unwrap();
        let changes = watcher.next_batch(Duration::from_secs(3)).unwrap();
        assert_eq!(changes, [env_file].into_iter().collect());
    }

    fn synthetic_rescan(path: &Path) -> notify::Event {
        use notify::event::Flag;

        notify::Event::new(EventKind::Other)
            .add_path(path.to_path_buf())
            .set_flag(Flag::Rescan)
    }

    fn resolve_rescan(watcher: &FileWatcher, path: &Path) -> BTreeSet<PathBuf> {
        let mut batch = BTreeSet::new();
        assert!(watcher.absorb(Ok(synthetic_rescan(path)), &mut batch));
        watcher.finish_batch(&mut batch);
        batch
    }

    #[test]
    fn rescan_with_no_change_is_an_empty_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        std::fs::write(root.join("crates/a/lib.rs"), "old").unwrap();
        let (bus, _rx) = LogBus::channel();
        let watcher =
            FileWatcher::start(&root, &[PathBuf::from("crates")], filter(&root), bus).unwrap();

        assert!(resolve_rescan(&watcher, &root.join("crates")).is_empty());
    }

    #[test]
    fn rescan_reports_only_files_that_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let changed = root.join("crates/a/lib.rs");
        let unchanged = root.join("crates/b/lib.rs");
        std::fs::create_dir_all(changed.parent().unwrap()).unwrap();
        std::fs::create_dir_all(unchanged.parent().unwrap()).unwrap();
        std::fs::write(&changed, "old").unwrap();
        std::fs::write(&unchanged, "same").unwrap();
        let (bus, _rx) = LogBus::channel();
        let watcher =
            FileWatcher::start(&root, &[PathBuf::from("crates")], filter(&root), bus).unwrap();

        std::fs::write(&changed, "new and a different size").unwrap();

        assert_eq!(
            resolve_rescan(&watcher, &root.join("crates")),
            [changed].into_iter().collect()
        );
    }

    #[test]
    fn rescan_sees_deleted_and_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let deleted = root.join("crates/a/deleted.rs");
        let added = root.join("crates/a/added.rs");
        std::fs::create_dir_all(deleted.parent().unwrap()).unwrap();
        std::fs::write(&deleted, "old").unwrap();
        let (bus, _rx) = LogBus::channel();
        let watcher =
            FileWatcher::start(&root, &[PathBuf::from("crates")], filter(&root), bus).unwrap();

        std::fs::remove_file(&deleted).unwrap();
        std::fs::write(&added, "new").unwrap();

        assert_eq!(
            resolve_rescan(&watcher, &root.join("crates")),
            [added, deleted].into_iter().collect()
        );
    }

    #[test]
    fn rescan_ignores_target_and_gitignored_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        let (bus, _rx) = LogBus::channel();
        let watcher =
            FileWatcher::start(&root, &[PathBuf::from("crates")], filter(&root), bus).unwrap();

        std::fs::create_dir_all(root.join("crates/a/target/tmp")).unwrap();
        std::fs::write(root.join("crates/a/target/tmp/output"), "ignored").unwrap();
        std::fs::create_dir_all(root.join("crates/a/generated")).unwrap();
        std::fs::write(root.join("crates/a/generated/output.rs"), "ignored").unwrap();

        assert!(resolve_rescan(&watcher, &root.join("crates")).is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn rescan_sees_replaced_watched_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates")).unwrap();
        let first = root.join("one");
        let second = root.join("a-much-longer-name");
        let env = root.join(".env");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        symlink(&first, &env).unwrap();
        let (bus, _rx) = LogBus::channel();
        let watcher =
            FileWatcher::start(&root, &[PathBuf::from("crates")], filter(&root), bus).unwrap();

        std::fs::remove_file(&env).unwrap();
        symlink(&second, &env).unwrap();

        assert_eq!(resolve_rescan(&watcher, &root), [env].into_iter().collect());
    }
}
