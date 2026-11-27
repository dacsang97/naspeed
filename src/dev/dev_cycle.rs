//! Build and test cycles; app slots change only after a successful build.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;

use super::affected_bins::affected_bins;
use super::affected_tests::{self, Scope, TestRun};
use super::cargo_runner::{CargoOutcome, message_format, run_cargo};
use super::dev_loop::DevLoop;
use super::dev_settings::is_rust_input;
use super::go_dev::GoInputKind;
use super::log_bus::{AppState, LogBus};
use super::process_supervisor::shutdown_requested;
use super::status_line;

impl DevLoop {
    pub(super) fn cycle(
        &mut self,
        changes: BTreeSet<PathBuf>,
        initial: bool,
        pending: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        if self.toolchain.is_go() {
            self.cycle_go(changes, initial, pending)
        } else {
            self.cycle_cargo(changes, initial, pending)
        }
    }

    fn cycle_cargo(
        &mut self,
        changes: BTreeSet<PathBuf>,
        initial: bool,
        pending: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        let started = Instant::now();
        let rust: BTreeSet<PathBuf> = changes
            .iter()
            .filter(|path| is_rust_input(path))
            .cloned()
            .collect();
        let other: BTreeSet<PathBuf> = changes.difference(&rust).cloned().collect();
        if !initial && rust.is_empty() {
            status_line::info(
                &self.bus,
                format!("{} changed: restarting", self.describe(&changes)),
            );
            let affected = affected_bins(
                self.toolchain.cargo_graph().expect("cargo graph"),
                &self.settings.apps,
                &rust,
                &other,
            );
            return self.restart(&affected, started);
        }
        if let Some(tests) = self.tests.take() {
            self.untested.extend(tests.cancel());
        }
        self.untested.extend(rust.iter().cloned());
        for slot in self.apps.values() {
            self.bus
                .status(slot.settings.bin.clone(), AppState::Building);
        }
        // The pre-build check guards `ns test` scope and reports errors on
        // the startup pass; an edit rebuild lets `cargo build` report the
        // same errors instead of paying a second cargo scan per change.
        if initial {
            let check_scope = self.cargo_check_scope(&rust);
            status_line::info(
                &self.bus,
                format!("checking {} (startup)", check_scope.describe()),
            );
            let args = self.cargo_check_args(&check_scope);
            let check = self.cargo(&args, pending)?;
            if check.interrupted {
                return Ok(());
            }
            if !check.success {
                report_failure(&self.bus, "check", &check, started);
                self.cycle_done(false, started, check.errors.len());
                self.restore_app_statuses();
                return Ok(());
            }
        }
        let bin_names = self
            .settings
            .apps
            .iter()
            .map(|app| app.bin.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        status_line::info(
            &self.bus,
            if initial {
                format!(
                    "check ok in {}; building {bin_names}",
                    status_line::ms(started.elapsed())
                )
            } else {
                format!("building {bin_names} ({})", self.describe(&rust))
            },
        );
        // Live-patch fast path: when the only live app's selector bodies are
        // what changed, `ns patch` rebuilds them without rustc and the
        // running process flips its own dispatch table — no build, no
        // restart. Any drift outside that scope falls through to cargo.
        if !initial && self.try_live_patch(&rust, &other) {
            self.cycle_done(true, started, 0);
            if self.settings.test && !self.untested.is_empty() {
                let paths = std::mem::take(&mut self.untested);
                let scope = affected_tests::test_scope(
                    self.toolchain.cargo_graph().expect("cargo graph"),
                    &paths,
                );
                self.tests = Some(TestRun::start(
                    &self.settings.root,
                    scope,
                    paths,
                    self.bus.clone(),
                )?);
            }
            return Ok(());
        }
        let args = self.cargo_build_args();
        let build = self.cargo(&args, pending)?;
        if build.interrupted {
            return Ok(());
        }
        if !build.success {
            report_failure(&self.bus, "build", &build, started);
            self.cycle_done(false, started, build.errors.len());
            self.restore_app_statuses();
            return Ok(());
        }
        self.update_cargo_binaries(&build);
        let graph = self.toolchain.cargo_graph().expect("cargo graph");
        let mut affected = if initial {
            self.apps.keys().cloned().collect()
        } else {
            affected_bins(graph, &self.settings.apps, &rust, &other)
        };
        affected.extend(
            self.apps
                .iter()
                .filter(|(_, slot)| slot.binary.is_none())
                .map(|(bin, _)| bin.clone()),
        );
        self.restart(&affected, started)?;
        self.cycle_done(true, started, 0);
        if self.settings.test && !self.untested.is_empty() {
            let paths = std::mem::take(&mut self.untested);
            let scope = affected_tests::test_scope(
                self.toolchain.cargo_graph().expect("cargo graph"),
                &paths,
            );
            self.tests = Some(TestRun::start(
                &self.settings.root,
                scope,
                paths,
                self.bus.clone(),
            )?);
        }
        Ok(())
    }

    fn cycle_go(
        &mut self,
        changes: BTreeSet<PathBuf>,
        initial: bool,
        pending: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        let started = Instant::now();
        let before_reload = self.classify_go_changes(&changes);
        let mut kinds = before_reload.clone();
        if !kinds.sources.is_empty() || !kinds.tests.is_empty() || !kinds.manifests.is_empty() {
            let (old_go_files, old_go_dirs) = {
                let graph = self.toolchain.go_graph().expect("Go graph");
                (graph.watch_files(), graph.watch_dirs())
            };
            if let Err(error) = self.toolchain.go_mut().expect("Go toolchain").reload() {
                status_line::error(
                    &self.bus,
                    format!("{error:#}; previous processes keep running"),
                );
                self.cycle_done(false, started, 1);
                return Ok(());
            }
            let graph = self.toolchain.go_graph().expect("Go graph");
            let watch_changed = self.go_watcher_stale
                || old_go_files != graph.watch_files()
                || (self.settings.go_auto_watch && old_go_dirs != graph.watch_dirs());
            if watch_changed {
                match self.refresh_go_watcher(&old_go_files, pending) {
                    Ok(()) => self.go_watcher_stale = false,
                    Err(error) => {
                        self.go_watcher_stale = true;
                        status_line::warn(
                            &self.bus,
                            format!(
                                "refresh Go file watcher: {error:#}; will retry after the next change"
                            ),
                        );
                    }
                }
            }
            kinds = self.classify_go_changes(&changes);
            kinds.preserve_build_inputs(before_reload);
        }
        if let Some(tests) = self.tests.take() {
            self.untested.extend(tests.cancel());
        }
        self.untested.extend(kinds.sources.iter().cloned());
        self.untested.extend(kinds.tests.iter().cloned());
        self.untested.extend(kinds.manifests.iter().cloned());

        let mut affected = self.affected_go_apps(&kinds, initial);
        if !initial && kinds.sources.is_empty() && kinds.manifests.is_empty() {
            if !kinds.other.is_empty() {
                status_line::info(
                    &self.bus,
                    format!("{} changed: restarting", self.describe(&kinds.other)),
                );
                self.restart(&affected, started)?;
            }
            self.start_go_tests()?;
            return Ok(());
        }
        affected.extend(
            self.apps
                .iter()
                .filter(|(_, slot)| slot.binary.is_none())
                .map(|(bin, _)| bin.clone()),
        );
        if affected.is_empty() {
            self.start_go_tests()?;
            return Ok(());
        }
        for bin in &affected {
            self.bus.status(bin.clone(), AppState::Building);
        }
        let targets = self
            .settings
            .apps
            .iter()
            .filter(|app| affected.contains(&app.bin))
            .map(|app| (app.bin.clone(), app.owner.clone()))
            .collect::<Vec<_>>();
        status_line::info(
            &self.bus,
            format!(
                "building {} ({})",
                targets
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if initial {
                    "startup".to_string()
                } else {
                    self.describe(&kinds.sources)
                }
            ),
        );
        let watcher = &self.watcher;
        let build = self
            .toolchain
            .go()
            .expect("Go toolchain")
            .build(&targets, || {
                let _ = watcher.drain(pending);
                shutdown_requested()
            })?;
        if build.interrupted {
            return Ok(());
        }
        if !build.success {
            status_line::compile_errors(&self.bus, "build", &build.errors, started.elapsed());
            self.cycle_done(false, started, build.errors.len());
            self.restore_app_statuses();
            return Ok(());
        }
        for (bin, artifact) in build.artifacts {
            if let Some(slot) = self.apps.get_mut(&bin) {
                slot.binary = Some(artifact);
            }
        }
        self.restart(&affected, started)?;
        self.cycle_done(true, started, 0);
        self.start_go_tests()
    }

    fn classify_go_changes(&self, changes: &BTreeSet<PathBuf>) -> GoChanges {
        let graph = self.toolchain.go_graph().expect("Go graph");
        let mut out = GoChanges::default();
        for path in changes {
            if self.settings.apps.iter().any(|app| {
                app.env_file.as_ref().is_some_and(|env_file| {
                    env_file == path || self.settings.root.join(env_file) == *path
                })
            }) {
                out.other.insert(path.clone());
                continue;
            }
            let paths = match graph.input_kind(path) {
                GoInputKind::Source => &mut out.sources,
                GoInputKind::Test => &mut out.tests,
                GoInputKind::Manifest => &mut out.manifests,
                GoInputKind::Other => &mut out.other,
                GoInputKind::Ignored => continue,
            };
            paths.insert(path.clone());
        }
        out
    }

    fn affected_go_apps(&self, changes: &GoChanges, initial: bool) -> BTreeSet<String> {
        if initial || !changes.manifests.is_empty() {
            return self.apps.keys().cloned().collect();
        }
        let graph = self.toolchain.go_graph().expect("Go graph");
        let owners = changes
            .sources
            .iter()
            .filter_map(|path| graph.owner_for_path(path));
        let dependents = graph.with_dependents(owners);
        let mut affected = self
            .settings
            .apps
            .iter()
            .filter(|app| dependents.contains(&app.owner))
            .map(|app| app.bin.clone())
            .collect::<BTreeSet<_>>();
        for change in &changes.other {
            let env_apps = self
                .settings
                .apps
                .iter()
                .filter(|app| {
                    app.env_file.as_ref().is_some_and(|env_file| {
                        env_file == change || self.settings.root.join(env_file) == *change
                    })
                })
                .map(|app| app.bin.clone())
                .collect::<BTreeSet<_>>();
            if env_apps.is_empty() {
                affected.extend(self.apps.keys().cloned());
            } else {
                affected.extend(env_apps);
            }
        }
        affected
    }

    fn start_go_tests(&mut self) -> Result<()> {
        if !self.settings.test || self.untested.is_empty() {
            return Ok(());
        }
        let paths = std::mem::take(&mut self.untested);
        let graph = self.toolchain.go_graph().expect("Go graph");
        let manifest = paths
            .iter()
            .any(|path| graph.input_kind(path) == GoInputKind::Manifest);
        let packages = if manifest {
            graph.all_packages()
        } else {
            let owners = paths.iter().filter_map(|path| graph.owner_for_path(path));
            graph.with_test_dependents(owners)
        };
        if packages.is_empty() {
            return Ok(());
        }
        self.tests = Some(TestRun::start_go(
            &self.settings.root,
            packages,
            paths,
            self.bus.clone(),
        )?);
        Ok(())
    }

    fn cargo_check_scope(&self, rust: &BTreeSet<PathBuf>) -> Scope {
        match affected_tests::changed_scope(
            self.toolchain.cargo_graph().expect("cargo graph"),
            rust,
        ) {
            Scope::Workspace => Scope::Workspace,
            Scope::Packages(mut names) => {
                names.extend(self.settings.apps.iter().map(|app| app.owner.clone()));
                Scope::Packages(names)
            }
        }
    }

    fn cargo_check_args(&self, scope: &Scope) -> Vec<String> {
        let mut args = vec!["check".to_string(), message_format().to_string()];
        args.extend(scope.cargo_args());
        self.append_cargo_bins(&mut args);
        args
    }

    /// Attempt `ns patch` for the live app: every changed input must be a
    /// Rust source file owned by that app's package, and the patch must
    /// apply cleanly. Returns true when the running process picked up the
    /// new bodies and the build can be skipped entirely.
    fn try_live_patch(&self, rust: &BTreeSet<PathBuf>, other: &BTreeSet<PathBuf>) -> bool {
        if !other.is_empty() {
            return false;
        }
        let Some(app) = self.settings.apps.iter().find(|app| app.live.is_some()) else {
            return false;
        };
        let graph = self.toolchain.cargo_graph().expect("cargo graph");
        if rust
            .iter()
            .any(|path| graph.member_for_path(path).map(|m| m.name.as_str()) != Some(app.owner.as_str()))
        {
            return false;
        }
        let target_dir = graph.target_dir.join("debug").join("deps");
        match crate::commands::fast_patch::run(&crate::commands::fast_patch::PatchArgs {
            bin: Some(app.bin.clone()),
            target_dir: Some(target_dir),
        }) {
            Ok(0) => {
                status_line::info(&self.bus, format!("patched {} live", app.bin));
                true
            }
            Ok(_) | Err(_) => {
                status_line::info(&self.bus, "live patch out of scope; building".to_string());
                false
            }
        }
    }

    fn cargo_build_args(&self) -> Vec<String> {
        let mut args = vec!["build".to_string(), message_format().to_string()];
        let owners: BTreeSet<&str> = self
            .settings
            .apps
            .iter()
            .map(|app| app.owner.as_str())
            .collect();
        for owner in owners {
            args.extend(["-p".into(), owner.into()]);
        }
        self.append_cargo_bins(&mut args);
        args
    }

    fn append_cargo_bins(&self, args: &mut Vec<String>) {
        for app in &self.settings.apps {
            args.extend(["--bin".into(), app.bin.clone()]);
        }
    }

    fn cargo(&self, args: &[String], pending: &mut BTreeSet<PathBuf>) -> Result<CargoOutcome> {
        // Only `cargo build` publishes app patches; `cargo check` and test
        // builds keep patch mode off.
        let live = (args.first().map(String::as_str) == Some("build"))
            .then(|| self.settings.apps.iter().find_map(|app| app.live.as_deref()))
            .flatten();
        run_cargo(&self.settings.root, args, &self.bus, live, || {
            let _ = self.watcher.drain(pending);
            shutdown_requested()
        })
    }

    fn update_cargo_binaries(&mut self, outcome: &CargoOutcome) {
        let target_dir = self
            .toolchain
            .cargo_graph()
            .expect("cargo graph")
            .target_dir
            .clone();
        for (bin, slot) in &mut self.apps {
            let built = outcome
                .executables
                .iter()
                .find(|(name, _)| name == bin)
                .map(|(_, path)| path.clone());
            slot.binary = Some(built.unwrap_or_else(|| {
                target_dir
                    .join("debug")
                    .join(format!("{bin}{}", std::env::consts::EXE_SUFFIX))
            }));
        }
    }

    fn cycle_done(&self, ok: bool, started: Instant, errors: usize) {
        self.bus.cycle(ok, started.elapsed(), errors);
    }

    fn restore_app_statuses(&self) {
        for slot in self.apps.values() {
            if let Some(pid) = slot.pid() {
                self.bus
                    .status(slot.settings.bin.clone(), AppState::Running { pid });
            } else {
                self.bus
                    .status(slot.settings.bin.clone(), AppState::Stopped);
            }
        }
    }
}

#[derive(Clone, Default)]
struct GoChanges {
    sources: BTreeSet<PathBuf>,
    tests: BTreeSet<PathBuf>,
    manifests: BTreeSet<PathBuf>,
    other: BTreeSet<PathBuf>,
}

impl GoChanges {
    fn preserve_build_inputs(&mut self, previous: GoChanges) {
        for path in previous.manifests {
            if !self.sources.contains(&path)
                && !self.tests.contains(&path)
                && !self.manifests.contains(&path)
            {
                self.other.remove(&path);
                self.manifests.insert(path);
            }
        }
        for path in previous.sources {
            if !self.sources.contains(&path)
                && !self.tests.contains(&path)
                && !self.manifests.contains(&path)
            {
                self.other.remove(&path);
                self.sources.insert(path);
            }
        }
        for path in previous.tests {
            if !self.sources.contains(&path)
                && !self.tests.contains(&path)
                && !self.manifests.contains(&path)
            {
                self.other.remove(&path);
                self.tests.insert(path);
            }
        }
    }
}

/// Cargo's stderr (manifest or lock errors without a compiler diagnostic) was
/// already streamed to the build log while Cargo ran.
fn report_failure(bus: &LogBus, step: &str, out: &CargoOutcome, started: Instant) {
    status_line::compile_errors(bus, step, &out.errors, started.elapsed());
}
