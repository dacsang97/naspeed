//! `ns dev`: check and build Rust changes once, then restart affected app groups.
//!
//! A failed check or build never stops a running app. Cargo is allowed to finish
//! so incremental work is retained; changes received during a cycle are replayed.
//! Every app, cargo and test process is stopped as a whole group on shutdown.

mod affected_bins;
mod affected_tests;
mod ansi_strip;
mod app_slot;
mod cargo_runner;
mod dev_cycle;
mod dev_loop;
mod dev_restart;
mod dev_settings;
mod env_file;
mod file_watcher;
mod line_reader;
mod log_bus;
mod log_files;
mod plain_renderer;
mod process_supervisor;
mod status_line;
mod tui;

use std::io::IsTerminal;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use clap::Args;

use self::dev_loop::UiCommand;
use self::dev_settings::{Settings, TuiMode};
use self::log_bus::{LogBus, Sink};
use self::plain_renderer::PlainRenderer;
use self::process_supervisor::{install_shutdown_handler, request_shutdown};
use crate::project::Project;

#[derive(Debug, Args)]
pub struct DevArgs {
    /// Binary to run; repeat to select multiple workspace binaries.
    #[arg(long, action = clap::ArgAction::Append)]
    pub bin: Vec<String>,
    /// Select every binary in the workspace.
    #[arg(long)]
    pub all: bool,
    /// Disable the interactive terminal interface.
    #[arg(long)]
    pub no_tui: bool,
    /// Run tests of changed crates and their dependents after each clean build.
    #[arg(long)]
    pub test: bool,
    /// Arguments passed to the only selected binary (after `--`); replace `[dev].args`.
    #[arg(last = true)]
    pub args: Vec<String>,
}

pub fn run(args: DevArgs) -> Result<u8> {
    let project = Project::load_cwd()?;
    let settings = Settings::resolve(args, &project)?;
    install_shutdown_handler()?;
    let bins: Vec<String> = settings.apps.iter().map(|app| app.bin.clone()).collect();
    let use_tui = settings.tui == TuiMode::Auto
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let (bus, rx) = LogBus::channel();
    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    let mut files = settings.log_dir.as_ref().and_then(|dir| {
        match log_files::LogFiles::start(dir, &bins, bus.clone()) {
            Ok(files) => Some(files),
            Err(err) => {
                status_line::warn(&bus, format!("cannot enable session logs: {err:#}"));
                None
            }
        }
    });
    if let Some(files) = &files {
        sinks.push(Box::new(files.sink()));
    }
    let router = if use_tui {
        let (sink, events) = tui::tui_sink::channel();
        sinks.push(Box::new(sink));
        let router = log_bus::route(rx, sinks);
        run_interactive(project, settings, bus.clone(), events, bins, router)?
    } else {
        sinks.push(Box::new(PlainRenderer));
        let router = log_bus::route(rx, sinks);
        let result = catch_unwind(AssertUnwindSafe(|| {
            dev_loop::run_loop(project, settings, bus.clone(), None, false)
        }));
        bus.shutdown();
        join_router(router)?;
        match result {
            Ok(result) => result?,
            Err(payload) => resume_unwind(payload),
        }
    };
    if let Some(files) = files.as_mut() {
        files.finish();
    }
    Ok(router)
}

fn run_interactive(
    project: Project,
    settings: Settings,
    bus: LogBus,
    events: mpsc::Receiver<log_bus::DevEvent>,
    bins: Vec<String>,
    router: thread::JoinHandle<()>,
) -> Result<u8> {
    let (commands, command_rx) = mpsc::channel::<UiCommand>();
    let loop_bus = bus.clone();
    let loop_thread = thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            dev_loop::run_loop(project, settings, loop_bus.clone(), Some(command_rx), true)
        }));
        loop_bus.shutdown();
        result
    });
    let tui_result = catch_unwind(AssertUnwindSafe(|| tui::run_tui(events, &bins, commands)));
    if !matches!(tui_result, Ok(Ok(()))) {
        request_shutdown();
    }
    let loop_result = loop_thread
        .join()
        .map_err(|_| anyhow::anyhow!("dev loop thread panicked"))?;
    let router_result = join_router(router);
    match tui_result {
        Ok(result) => result.context("interactive terminal stopped unexpectedly")?,
        Err(payload) => resume_unwind(payload),
    }
    router_result.context("log router failed while joining interactive terminal")?;
    let loop_code = match loop_result {
        Ok(result) => result?,
        Err(payload) => resume_unwind(payload),
    };
    Ok(loop_code)
}

fn join_router(router: thread::JoinHandle<()>) -> Result<()> {
    router
        .join()
        .map_err(|_| anyhow::anyhow!("dev log router thread panicked"))
}
