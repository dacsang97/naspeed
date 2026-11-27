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

impl DevLoop {
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
            let mut env = match &slot.settings.env_file {
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
            if slot.settings.live.is_some() {
                // The launcher watches the generation's live feed and
                // applies each published patch in the running process.
                env.push(("NASPEED_LAZY_APP_LIVE".into(), "1".into()));
            }
            if let Err(err) = self.toolchain.validate_staged_binary(bin, &binary) {
                status_line::error(&self.bus, format!("{err:#}"));
                continue;
            }
            let previous_binary = self.toolchain.final_binary_path(bin, &binary);
            let was_running = slot.pid().is_some();
            if slot.settings.live.is_some()
                && was_running
                && !slot.live_rejected.swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                // The demand build already published this generation's patch
                // plan through the launcher's live feed; the process applies
                // it in place when the base and slot layout still match. A
                // previously rejected patch means stale code is serving, so
                // that build restarts instead.
                status_line::ok(
                    &self.bus,
                    format!(
                        "live patch for {bin} queued {} after the change",
                        status_line::ms(since_change.elapsed())
                    ),
                );
                continue;
            }
            if let Some(pid) = slot.pid() {
                status_line::info(&self.bus, format!("stopping {bin} (pid {pid})"));
                slot.stop(STOP_GRACE);
            }
            let binary = match self.toolchain.finalize_binary(bin, &binary) {
                Ok(binary) => {
                    slot.binary = Some(binary.clone());
                    binary
                }
                Err(err) => {
                    status_line::error(&self.bus, format!("{err:#}"));
                    if was_running && previous_binary.is_file() {
                        slot.binary = Some(previous_binary.clone());
                        if let Err(restart_error) =
                            slot.start(&self.settings.root, previous_binary, env.clone())
                        {
                            status_line::error(
                                &self.bus,
                                format!(
                                    "cannot restore {bin} after failed binary install: {restart_error:#}"
                                ),
                            );
                        }
                    }
                    continue;
                }
            };
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
        // A live patch the launcher rejected leaves the old code serving;
        // restart that app so the new generation takes over.
        let stale: Vec<String> = self
            .apps
            .values()
            .filter(|slot| {
                slot.settings.live.is_some()
                    && slot.pid().is_some()
                    && slot.live_rejected.load(std::sync::atomic::Ordering::Relaxed)
            })
            .map(|slot| slot.settings.bin.clone())
            .collect();
        for bin in stale {
            status_line::info(&self.bus, format!("live patch rejected; restarting {bin}"));
            self.restart(&BTreeSet::from([bin]), Instant::now())?;
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

impl Drop for DevLoop {
    fn drop(&mut self) {
        if let Some(tests) = self.tests.take() {
            tests.cancel();
        }
        if !self.apps.is_empty() {
            self.stop_apps_parallel();
        }
    }
}
