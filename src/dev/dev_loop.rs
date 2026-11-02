//! Watch, check, build once, then restart only affected application groups.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::Result;

use super::app_slot::AppSlot;
use super::dev_settings::Settings;
use super::file_watcher::{FileWatcher, WatchFilter};
use super::log_bus::LogBus;
use super::process_supervisor::shutdown_requested;
use super::status_line;
use crate::project::{Project, WorkspaceGraph};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(super) enum UiCommand {
    Restart(Option<String>),
    Quit,
}

pub(super) struct DevLoop<'a> {
    pub(super) graph: &'a WorkspaceGraph,
    pub(super) settings: Settings,
    pub(super) watcher: FileWatcher,
    pub(super) apps: BTreeMap<String, AppSlot>,
    pub(super) tests: Option<super::affected_tests::TestRun>,
    pub(super) ui_rx: Option<Receiver<UiCommand>>,
    pub(super) manual_restarts: BTreeSet<String>,
    /// Rust changes not yet covered by a started test run.
    pub(super) untested: BTreeSet<PathBuf>,
    pub(super) bus: LogBus,
}

pub fn run_loop(
    project: Project,
    settings: Settings,
    bus: LogBus,
    ui_rx: Option<Receiver<UiCommand>>,
    colorize: bool,
) -> Result<u8> {
    let filter = WatchFilter::new(
        &settings.root,
        &settings.watch_dirs,
        &settings.watch_files,
        &settings.ignore,
    )?;
    let watcher = FileWatcher::start(&settings.root, &settings.watch_dirs, filter, bus.clone())?;
    let bins = settings
        .apps
        .iter()
        .map(|app| app.bin.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    status_line::info(
        &bus,
        format!("watching {} for {bins}", settings.root.display()),
    );
    let apps = settings
        .apps
        .iter()
        .cloned()
        .map(|app| {
            let bin = app.bin.clone();
            (bin, AppSlot::new(app, bus.clone(), colorize))
        })
        .collect();
    let mut dev = DevLoop {
        graph: &project.graph,
        settings,
        watcher,
        apps,
        tests: None,
        ui_rx,
        manual_restarts: BTreeSet::new(),
        untested: BTreeSet::new(),
        bus,
    };
    let mut pending = BTreeSet::new();
    dev.cycle(BTreeSet::new(), true, &mut pending)?;
    dev.run_pending(pending)?;
    while !shutdown_requested() {
        dev.poll_background()?;
        dev.process_ui_commands();
        dev.restart_manual()?;
        if shutdown_requested() {
            break;
        }
        let batch = dev.watcher.next_batch(POLL_INTERVAL)?;
        dev.run_pending(batch)?;
    }
    dev.shutdown();
    Ok(0)
}

impl DevLoop<'_> {
    fn run_pending(&mut self, mut pending: BTreeSet<PathBuf>) -> Result<()> {
        while !pending.is_empty() && !shutdown_requested() {
            let changes = std::mem::take(&mut pending);
            self.cycle(changes, false, &mut pending)?;
            if !pending.is_empty() {
                self.watcher.settle(&mut pending)?;
                status_line::info(&self.bus, "files changed during the build; running again");
            }
        }
        self.process_ui_commands();
        self.restart_manual()?;
        Ok(())
    }
}
