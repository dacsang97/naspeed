//! One application process group and the readers that drain its output pipes.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};

use super::dev_settings::AppSettings;
use super::line_reader;
use super::log_bus::{AppState, LogBus, LogSource, Stream};
use super::process_supervisor::{GroupChild, STOP_GRACE};

pub struct AppSlot {
    pub settings: AppSettings,
    pub binary: Option<PathBuf>,
    child: Option<GroupChild>,
    readers: Vec<JoinHandle<()>>,
    bus: LogBus,
    colorize: bool,
}

impl AppSlot {
    pub fn new(settings: AppSettings, bus: LogBus, colorize: bool) -> AppSlot {
        AppSlot {
            settings,
            binary: None,
            child: None,
            readers: Vec::new(),
            bus,
            colorize,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(GroupChild::pid)
    }

    pub fn start(
        &mut self,
        root: &Path,
        binary: PathBuf,
        env: Vec<(String, String)>,
    ) -> Result<u32> {
        let mut command = Command::new(&binary);
        command
            .args(&self.settings.args)
            .current_dir(root)
            .envs(env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_color_environment(&mut command, &env, self.colorize);
        self.bus
            .status(self.settings.bin.clone(), AppState::Starting);
        let mut child = GroupChild::spawn(command)?;
        let pid = child.pid();
        let stdout = child.child_mut().stdout.take().context("app stdout pipe")?;
        let stderr = child.child_mut().stderr.take().context("app stderr pipe")?;
        let source = LogSource::App(self.settings.bin.clone());
        self.readers.push(line_reader::spawn(
            stdout,
            self.bus.clone(),
            source.clone(),
            Stream::Stdout,
        ));
        self.readers.push(line_reader::spawn(
            stderr,
            self.bus.clone(),
            source,
            Stream::Stderr,
        ));
        self.binary = Some(binary);
        self.child = Some(child);
        self.bus
            .status(self.settings.bin.clone(), AppState::Running { pid });
        Ok(pid)
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        child.stop(STOP_GRACE);
        self.child = None;
        self.join_readers();
        Ok(Some(status))
    }

    pub fn stop(&mut self, grace: Duration) {
        if let Some(mut child) = self.child.take() {
            child.stop(grace);
            self.bus
                .status(self.settings.bin.clone(), AppState::Stopped);
        }
        self.join_readers();
    }

    fn join_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl Drop for AppSlot {
    fn drop(&mut self) {
        self.stop(STOP_GRACE);
    }
}

fn apply_color_environment(command: &mut Command, env: &[(String, String)], colorize: bool) {
    let no_color =
        std::env::var_os("NO_COLOR").is_some() || env.iter().any(|(key, _)| key == "NO_COLOR");
    if !colorize || no_color {
        return;
    }
    for key in ["CLICOLOR_FORCE", "FORCE_COLOR"] {
        if !env.iter().any(|(name, _)| name == key) {
            command.env(key, "1");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    use crate::dev::log_bus::DevEvent;

    #[test]
    fn child_output_caps_long_lines_and_flushes_final_partial_line() {
        let root = tempfile::tempdir().unwrap();
        let settings = AppSettings {
            bin: "fixture".into(),
            owner: "fixture".into(),
            args: vec![
                "-c".into(),
                "printf '%70000d\\n' 0; printf 'unterminated'".into(),
            ],
            env_file: None,
            ports: Vec::new(),
        };
        let (bus, rx) = LogBus::channel();
        let mut slot = AppSlot::new(settings, bus, false);
        slot.start(root.path(), PathBuf::from("/bin/sh"), Vec::new())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if slot.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "fixture child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }

        let lines: Vec<String> = rx
            .try_iter()
            .filter_map(|event| match event {
                DevEvent::Log(line) => Some(line.raw),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].len(), 65_536 + "…[truncated]".len());
        assert!(lines[0].ends_with("…[truncated]"));
        assert_eq!(lines[1], "unterminated");
    }
}
