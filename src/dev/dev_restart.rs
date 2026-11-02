//! Per-app restart, background process polling, and group-safe shutdown.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::Instant;

use anyhow::Result;

use super::dev_loop::{DevLoop, UiCommand};
use super::env_file;
use super::log_bus::AppState;
use super::process_supervisor::{self, PORT_WAIT, STOP_GRACE, shutdown_requested};
use super::status_line;

impl DevLoop<'_> {
    pub(super) fn restart(&mut self, bins: &BTreeSet<String>, since_change: Instant) -> Result<()> {
        for bin in bins {
            let Some(slot) = self.apps.get_mut(bin) else {
                continue;
            };
            let Some(binary) = slot.binary.clone() else {
                status_line::warn(
                    &self.bus,
                    format!("no successful build for {bin} yet; waiting for changes"),
                );
                continue;
            };
            let env = match &slot.settings.env_file {
                Some(path) => match env_file::load(path) {
                    Ok(vars) => vars,
                    Err(err) => {
                        status_line::error(
                            &self.bus,
                            format!("{err:#}; previous process keeps running"),
                        );
                        continue;
                    }
                },
                None => Vec::new(),
            };
            if let Some(pid) = slot.pid() {
                status_line::info(&self.bus, format!("stopping {bin} (pid {pid})"));
                slot.stop(STOP_GRACE);
            }
            for port in process_supervisor::wait_ports_free(&slot.settings.ports, PORT_WAIT) {
                status_line::warn(
                    &self.bus,
                    format!(
                        "port {port} still in use after {}; owner:\n{}",
                        status_line::ms(PORT_WAIT),
                        process_supervisor::port_owner(port)
                    ),
                );
            }
            match slot.start(&self.settings.root, binary, env) {
                Ok(pid) => status_line::ok(
                    &self.bus,
                    format!(
                        "started {bin} (pid {pid}) {} after the change",
                        status_line::ms(since_change.elapsed())
                    ),
                ),
                Err(err) => status_line::error(&self.bus, format!("{err:#}")),
            }
        }
        Ok(())
    }

    pub(super) fn poll_background(&mut self) -> Result<()> {
        for slot in self.apps.values_mut() {
            if let Some(status) = slot.try_wait()? {
                let code = status.code();
                status_line::warn(
                    &self.bus,
                    format!(
                        "{} exited ({status}); waiting for changes",
                        slot.settings.bin
                    ),
                );
                self.bus
                    .status(slot.settings.bin.clone(), AppState::Crashed { code });
            }
        }
        if let Some(tests) = self.tests.as_mut()
            && tests.poll()?.is_some()
        {
            self.tests = None;
        }
        Ok(())
    }

    pub(super) fn process_ui_commands(&mut self) {
        let Some(rx) = &self.ui_rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(UiCommand::Restart(Some(bin))) => {
                    if self.apps.contains_key(&bin) {
                        self.manual_restarts.insert(bin);
                    }
                }
                Ok(UiCommand::Restart(None)) => {
                    self.manual_restarts.extend(self.apps.keys().cloned());
                }
                Ok(UiCommand::Quit) => process_supervisor::request_shutdown(),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    pub(super) fn restart_manual(&mut self) -> Result<()> {
        if self.manual_restarts.is_empty() || shutdown_requested() {
            return Ok(());
        }
        let bins = std::mem::take(&mut self.manual_restarts);
        self.restart(&bins, Instant::now())
    }

    pub(super) fn shutdown(&mut self) {
        status_line::info(&self.bus, "shutting down");
        if let Some(tests) = self.tests.take() {
            tests.cancel();
        }
        self.stop_apps_parallel();
        status_line::info(&self.bus, "stopped");
    }

    fn stop_apps_parallel(&mut self) {
        let apps = std::mem::take(&mut self.apps);
        thread::scope(|scope| {
            for (_, mut slot) in apps {
                scope.spawn(move || slot.stop(STOP_GRACE));
            }
        });
    }

    pub(super) fn describe(&self, paths: &BTreeSet<PathBuf>) -> String {
        let Some(first) = paths.iter().next() else {
            return "nothing".into();
        };
        let first = first
            .strip_prefix(&self.settings.root)
            .unwrap_or(first)
            .display();
        match paths.len() {
            1 => first.to_string(),
            n => format!("{first} and {} more", n - 1),
        }
    }
}

impl Drop for DevLoop<'_> {
    fn drop(&mut self) {
        if let Some(tests) = self.tests.take() {
            tests.cancel();
        }
        if !self.apps.is_empty() {
            self.stop_apps_parallel();
        }
    }
}
