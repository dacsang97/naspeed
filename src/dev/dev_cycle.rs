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
use super::log_bus::{AppState, LogBus};
use super::process_supervisor::shutdown_requested;
use super::status_line;

impl DevLoop<'_> {
    pub(super) fn cycle(
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
            let affected = affected_bins(self.graph, &self.settings.apps, &rust, &other);
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
        let check_scope = self.check_scope(&rust);
        let what = if initial {
            "startup".to_string()
        } else {
            self.describe(&rust)
        };
        status_line::info(
            &self.bus,
            format!("checking {} ({what})", check_scope.describe()),
        );
        let args = self.check_args(&check_scope);
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
        let bin_names = self
            .settings
            .apps
            .iter()
            .map(|app| app.bin.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        status_line::info(
            &self.bus,
            format!(
                "check ok in {}; building {bin_names}",
                status_line::ms(started.elapsed())
            ),
        );
        let args = self.build_args();
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
        self.update_binaries(&build);
        let mut affected = if initial {
            self.apps.keys().cloned().collect()
        } else {
            affected_bins(self.graph, &self.settings.apps, &rust, &other)
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
            let scope = affected_tests::test_scope(self.graph, &paths);
            self.tests = Some(TestRun::start(
                &self.settings.root,
                scope,
                paths,
                self.bus.clone(),
            )?);
        }
        Ok(())
    }

    fn check_scope(&self, rust: &BTreeSet<PathBuf>) -> Scope {
        match affected_tests::changed_scope(self.graph, rust) {
            Scope::Workspace => Scope::Workspace,
            Scope::Packages(mut names) => {
                names.extend(self.settings.apps.iter().map(|app| app.owner.clone()));
                Scope::Packages(names)
            }
        }
    }

    fn check_args(&self, scope: &Scope) -> Vec<String> {
        let mut args = vec!["check".to_string(), message_format().to_string()];
        args.extend(scope.cargo_args());
        self.append_bins(&mut args);
        args
    }

    fn build_args(&self) -> Vec<String> {
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
        self.append_bins(&mut args);
        args
    }

    fn append_bins(&self, args: &mut Vec<String>) {
        for app in &self.settings.apps {
            args.extend(["--bin".into(), app.bin.clone()]);
        }
    }

    fn cargo(&self, args: &[String], pending: &mut BTreeSet<PathBuf>) -> Result<CargoOutcome> {
        run_cargo(&self.settings.root, args, || {
            let _ = self.watcher.drain(pending);
            shutdown_requested()
        })
    }

    fn update_binaries(&mut self, outcome: &CargoOutcome) {
        for (bin, slot) in &mut self.apps {
            let built = outcome
                .executables
                .iter()
                .find(|(name, _)| name == bin)
                .map(|(_, path)| path.clone());
            slot.binary = Some(built.unwrap_or_else(|| {
                self.graph
                    .target_dir
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

fn report_failure(bus: &LogBus, step: &str, out: &CargoOutcome, started: Instant) {
    if out.errors.is_empty() {
        let lines: Vec<&str> = out.stderr.lines().collect();
        bus.log(
            super::log_bus::LogSource::Build,
            super::log_bus::Stream::Stderr,
            lines[lines.len().saturating_sub(20)..].join("\n"),
        );
    }
    status_line::compile_errors(bus, step, &out.errors, started.elapsed());
}
