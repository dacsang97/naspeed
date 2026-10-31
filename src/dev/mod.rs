//! `ns dev`: watch → `cargo check` → build → restart.
//!
//! Invariants:
//! - at most one app process (group) exists: the old one is stopped and its
//!   `[dev].ports` released before the new one starts;
//! - a failing check or build never stops the running app;
//! - cargo is never killed because of a new change (that would waste
//!   incremental work): changes seen meanwhile mark the cycle dirty and it
//!   reruns right after;
//! - every child (app, cargo, tests) is stopped as a whole process group when
//!   `ns dev` ends, whether by Ctrl-C, SIGTERM, an error or a panic.

mod affected_tests;
mod cargo_runner;
mod dev_settings;
mod env_file;
mod file_watcher;
mod process_supervisor;
mod status_line;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Args;

use crate::project::{Project, WorkspaceGraph};
use affected_tests::{Scope, TestRun};
use cargo_runner::{CargoOutcome, message_format, run_cargo};
use dev_settings::{Settings, is_rust_input};
use file_watcher::{FileWatcher, WatchFilter};
use process_supervisor::{GroupChild, PORT_WAIT, STOP_GRACE, shutdown_requested};
use status_line::ms;

#[derive(Debug, Args)]
pub struct DevArgs {
    /// Binary to run (default: `[dev].bin` in ns.toml, else the only workspace binary).
    #[arg(long)]
    pub bin: Option<String>,
    /// Run tests of changed crates and their dependents after each clean build.
    #[arg(long)]
    pub test: bool,
    /// Arguments passed to the binary (after `--`); replace `[dev].args`.
    #[arg(last = true)]
    pub args: Vec<String>,
}

struct DevLoop<'a> {
    graph: &'a WorkspaceGraph,
    s: Settings,
    watcher: FileWatcher,
    app: Option<GroupChild>,
    binary: Option<PathBuf>,
    tests: Option<TestRun>,
    /// Rust changes not yet covered by a started test run.
    untested: BTreeSet<PathBuf>,
}

pub fn run(args: DevArgs) -> Result<u8> {
    let project = Project::load_cwd()?;
    let s = Settings::resolve(args, &project)?;
    let filter = WatchFilter::new(&s.root, &s.watch_dirs, &s.watch_files, &s.ignore)?;
    process_supervisor::install_shutdown_handler()?;
    let watcher = FileWatcher::start(&s.root, &s.watch_dirs, filter)?;
    status_line::info(format!("watching {} for {}", s.root.display(), s.bin));

    let mut dev = DevLoop {
        graph: &project.graph,
        s,
        watcher,
        app: None,
        binary: None,
        tests: None,
        untested: BTreeSet::new(),
    };
    let mut pending = BTreeSet::new();
    dev.cycle(BTreeSet::new(), true, &mut pending)?;
    dev.run_pending(pending)?;
    while !shutdown_requested() {
        dev.poll_background()?;
        let batch = dev.watcher.next_batch(Duration::from_millis(100))?;
        dev.run_pending(batch)?;
    }
    dev.shutdown();
    Ok(0)
}

impl DevLoop<'_> {
    /// Runs cycles until no change is left (changes during a cycle make it dirty).
    fn run_pending(&mut self, mut pending: BTreeSet<PathBuf>) -> Result<()> {
        while !pending.is_empty() && !shutdown_requested() {
            let changes = std::mem::take(&mut pending);
            self.cycle(changes, false, &mut pending)?;
            if !pending.is_empty() {
                self.watcher.settle(&mut pending)?;
                status_line::info("files changed during the build; running again");
            }
        }
        Ok(())
    }

    fn cycle(
        &mut self,
        changes: BTreeSet<PathBuf>,
        initial: bool,
        pending: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        let started = Instant::now();
        let rust: BTreeSet<PathBuf> = changes
            .iter()
            .filter(|p| is_rust_input(p))
            .cloned()
            .collect();
        if !initial && rust.is_empty() {
            status_line::info(format!("{} changed: restarting", self.describe(&changes)));
            return self.restart(started);
        }
        if let Some(tests) = self.tests.take() {
            self.untested.extend(tests.cancel());
        }
        self.untested.extend(rust.iter().cloned());

        let check_scope = match affected_tests::changed_scope(self.graph, &rust) {
            Scope::Workspace => Scope::Workspace,
            Scope::Packages(mut names) => {
                names.insert(self.s.owner.clone());
                Scope::Packages(names)
            }
        };
        let what = if initial {
            "startup".to_string()
        } else {
            self.describe(&rust)
        };
        status_line::info(format!("checking {} ({what})", check_scope.describe()));
        let mut args = vec!["check".to_string(), message_format().to_string()];
        args.extend(check_scope.cargo_args());
        let out = self.cargo(&args, pending)?;
        if out.interrupted {
            return Ok(());
        }
        if !out.success {
            report_failure("check", &out, started);
            return Ok(());
        }

        status_line::info(format!(
            "check ok in {}; building {}",
            ms(started.elapsed()),
            self.s.bin
        ));
        let args = [
            "build",
            message_format(),
            "-p",
            &self.s.owner,
            "--bin",
            &self.s.bin,
        ]
        .map(String::from);
        let out = self.cargo(&args, pending)?;
        if out.interrupted {
            return Ok(());
        }
        if !out.success {
            report_failure("build", &out, started);
            return Ok(());
        }
        let built = out.executables.iter().find(|(name, _)| *name == self.s.bin);
        self.binary = Some(match built {
            Some((_, exe)) => exe.clone(),
            None => self.graph.target_dir.join("debug").join(&self.s.bin),
        });
        self.restart(started)?;

        if self.s.test && !self.untested.is_empty() {
            let paths = std::mem::take(&mut self.untested);
            let scope = affected_tests::test_scope(self.graph, &paths);
            self.tests = Some(TestRun::start(&self.s.root, scope, paths)?);
        }
        Ok(())
    }

    /// cargo, recording changes that arrive meanwhile and aborting on shutdown.
    fn cargo(&self, args: &[String], pending: &mut BTreeSet<PathBuf>) -> Result<CargoOutcome> {
        let watcher = &self.watcher;
        run_cargo(&self.s.root, args, || {
            let _ = watcher.drain(pending);
            shutdown_requested()
        })
    }

    /// Stops the old app, waits for its ports, starts the freshly built one.
    fn restart(&mut self, since_change: Instant) -> Result<()> {
        let Some(binary) = self.binary.clone() else {
            status_line::warn("no successful build yet; waiting for changes");
            return Ok(());
        };
        // Validate .env before touching the running app.
        let env = match &self.s.env_file {
            Some(path) => match env_file::load(path) {
                Ok(vars) => vars,
                Err(err) => {
                    status_line::error(format!("{err:#}; previous process keeps running"));
                    return Ok(());
                }
            },
            None => Vec::new(),
        };
        if let Some(mut app) = self.app.take() {
            status_line::info(format!("stopping {} (pid {})", self.s.bin, app.pid()));
            app.stop(STOP_GRACE);
        }
        for port in process_supervisor::wait_ports_free(&self.s.ports, PORT_WAIT) {
            status_line::warn(format!(
                "port {port} still in use after {}; owner:\n{}",
                ms(PORT_WAIT),
                process_supervisor::port_owner(port)
            ));
        }
        let mut cmd = Command::new(&binary);
        cmd.args(&self.s.args)
            .current_dir(&self.s.root)
            .envs(env)
            .stdin(Stdio::null());
        match GroupChild::spawn(cmd) {
            Ok(app) => {
                status_line::ok(format!(
                    "started {} (pid {}) {} after the change",
                    self.s.bin,
                    app.pid(),
                    ms(since_change.elapsed())
                ));
                self.app = Some(app);
            }
            Err(err) => status_line::error(format!("{err:#}")),
        }
        Ok(())
    }

    fn poll_background(&mut self) -> Result<()> {
        if let Some(app) = self.app.as_mut()
            && let Some(status) = app.try_wait()?
        {
            status_line::warn(format!(
                "{} exited ({status}); waiting for changes",
                self.s.bin
            ));
            // Dropping stops whatever the app left behind in its group.
            self.app = None;
        }
        if let Some(tests) = self.tests.as_mut()
            && tests.poll()?.is_some()
        {
            self.tests = None;
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        status_line::info("shutting down");
        if let Some(tests) = self.tests.take() {
            tests.cancel();
        }
        if let Some(mut app) = self.app.take() {
            app.stop(STOP_GRACE);
        }
        status_line::info("stopped");
    }

    /// `crates/x/src/lib.rs` or `crates/x/src/lib.rs and 3 more`, root-relative.
    fn describe(&self, paths: &BTreeSet<PathBuf>) -> String {
        let Some(first) = paths.iter().next() else {
            return "nothing".into();
        };
        let first = first.strip_prefix(&self.s.root).unwrap_or(first).display();
        match paths.len() {
            1 => first.to_string(),
            n => format!("{first} and {} more", n - 1),
        }
    }
}

fn report_failure(step: &str, out: &CargoOutcome, started: Instant) {
    if out.errors.is_empty() {
        let lines: Vec<&str> = out.stderr.lines().collect();
        eprintln!("{}", lines[lines.len().saturating_sub(20)..].join("\n"));
    }
    status_line::compile_errors(step, &out.errors, started.elapsed());
}
