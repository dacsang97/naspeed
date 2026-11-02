//! One asynchronous writer owns each session log and its rotation state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use super::ansi_strip::strip_ansi;
use super::log_bus::{DevEvent, LogBus, LogLine, LogSource, Sink, Stream};

const ROTATE_AT: u64 = 50 * 1024 * 1024;
const FLUSH_EVERY: Duration = Duration::from_millis(200);
const FILE_QUEUE_CAPACITY: usize = 256;

enum Message {
    Line(String, LogLine),
    Finish(SyncSender<()>),
}

pub struct LogFiles {
    tx: SyncSender<Message>,
    worker: Option<JoinHandle<()>>,
    app_logs: Arc<BTreeMap<String, String>>,
}

pub struct LogFilesSink {
    tx: SyncSender<Message>,
    app_logs: Arc<BTreeMap<String, String>>,
}

struct LogFile {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    size: u64,
    limit: u64,
}

impl LogFiles {
    pub fn start(dir: &Path, bins: &[String], bus: LogBus) -> Result<LogFiles> {
        Self::start_with_limit(dir, bins, bus, ROTATE_AT)
    }

    fn start_with_limit(dir: &Path, bins: &[String], bus: LogBus, limit: u64) -> Result<LogFiles> {
        fs::create_dir_all(dir)
            .with_context(|| format!("create log directory {}", dir.display()))?;
        let mut files = BTreeMap::new();
        let app_logs = Arc::new(app_log_names(bins));
        let names = std::iter::once("build.log".to_string())
            .chain(app_logs.values().cloned())
            .collect::<BTreeSet<_>>();
        for name in names {
            let path = dir.join(&name);
            let backup = backup_path(&path);
            if let Err(err) = fs::remove_file(&backup)
                && err.kind() != io::ErrorKind::NotFound
            {
                bus.try_log(
                    LogSource::Ns,
                    Stream::Stderr,
                    format!("log writer: disable {}: {err}", path.display()),
                );
                continue;
            }
            match File::create(&path) {
                Ok(file) => {
                    files.insert(
                        name,
                        LogFile {
                            path,
                            writer: Some(BufWriter::new(file)),
                            size: 0,
                            limit,
                        },
                    );
                }
                Err(err) => {
                    bus.try_log(
                        LogSource::Ns,
                        Stream::Stderr,
                        format!("log writer: disable {}: {err}", path.display()),
                    );
                }
            }
        }
        let (tx, rx) = writer_channel(FILE_QUEUE_CAPACITY);
        let worker = thread::spawn(move || write_loop(rx, files, bus));
        Ok(LogFiles {
            tx,
            worker: Some(worker),
            app_logs,
        })
    }

    pub fn sink(&self) -> LogFilesSink {
        LogFilesSink {
            tx: self.tx.clone(),
            app_logs: Arc::clone(&self.app_logs),
        }
    }

    pub fn finish(&mut self) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(0);
        let _ = self.tx.send(Message::Finish(ack_tx));
        let _ = ack_rx.recv();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for LogFiles {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.finish();
        }
    }
}

impl Sink for LogFilesSink {
    fn write(&mut self, event: &DevEvent) {
        let DevEvent::Log(line) = event else {
            return;
        };
        let name = match &line.source {
            LogSource::App(bin) => {
                let Some(name) = self.app_logs.get(bin) else {
                    return;
                };
                name.clone()
            }
            LogSource::Ns | LogSource::Build | LogSource::Test => "build.log".into(),
        };
        let _ = self.tx.send(Message::Line(name, line.clone()));
    }

    fn finish(&mut self) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(0);
        let _ = self.tx.send(Message::Finish(ack_tx));
        let _ = ack_rx.recv();
    }
}

fn write_loop(rx: Receiver<Message>, mut files: BTreeMap<String, LogFile>, bus: LogBus) {
    loop {
        match rx.recv_timeout(FLUSH_EVERY) {
            Ok(Message::Line(name, line)) => {
                let Some(file) = files.get_mut(&name) else {
                    continue;
                };
                if let Err(err) = write_line(file, &line) {
                    file.writer = None;
                    bus.try_log(
                        LogSource::Ns,
                        Stream::Stderr,
                        format!("log writer: disable {}: {err}", file.path.display()),
                    );
                }
            }
            Ok(Message::Finish(ack)) => {
                flush_all(&mut files, &bus);
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => flush_all(&mut files, &bus),
            Err(RecvTimeoutError::Disconnected) => {
                flush_all(&mut files, &bus);
                return;
            }
        }
    }
}

fn write_line(file: &mut LogFile, line: &LogLine) -> io::Result<()> {
    let plain = strip_ansi(&line.raw);
    for text in plain.trim_end_matches('\n').split('\n') {
        let tag = if line.stream == Stream::Stderr {
            "[stderr] "
        } else {
            ""
        };
        let output = format!("{} {tag}{text}\n", timestamp(line.ts));
        if file.size > 0 && file.size.saturating_add(output.len() as u64) > file.limit {
            file.rotate()?;
        }
        let Some(writer) = file.writer.as_mut() else {
            return Ok(());
        };
        writer.write_all(output.as_bytes())?;
        file.size += output.len() as u64;
        if file.size > file.limit {
            file.rotate()?;
        }
    }
    Ok(())
}

fn flush_all(files: &mut BTreeMap<String, LogFile>, bus: &LogBus) {
    for file in files.values_mut() {
        if let Some(writer) = file.writer.as_mut()
            && let Err(err) = writer.flush()
        {
            file.writer = None;
            bus.try_log(
                LogSource::Ns,
                Stream::Stderr,
                format!("log writer: disable {}: {err}", file.path.display()),
            );
        }
    }
}

impl LogFile {
    fn rotate(&mut self) -> io::Result<()> {
        if let Some(writer) = self.writer.take() {
            writer.into_inner().map_err(|err| err.into_error())?;
        }
        let backup = backup_path(&self.path);
        if let Err(err) = fs::remove_file(&backup)
            && err.kind() != io::ErrorKind::NotFound
        {
            return Err(err);
        }
        fs::rename(&self.path, &backup)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)?;
        self.writer = Some(BufWriter::new(file));
        self.size = 0;
        Ok(())
    }
}

fn writer_channel(capacity: usize) -> (SyncSender<Message>, Receiver<Message>) {
    mpsc::sync_channel(capacity)
}

fn app_log_names(bins: &[String]) -> BTreeMap<String, String> {
    let bins = bins.iter().cloned().collect::<BTreeSet<_>>();
    let mut app_logs = BTreeMap::new();
    let mut used = BTreeSet::from([filename_key("build.log")]);

    for bin in &bins {
        let name = format!("{bin}.log");
        if used.insert(filename_key(&name)) {
            app_logs.insert(bin.clone(), name);
        }
    }
    for bin in bins {
        if app_logs.contains_key(&bin) {
            continue;
        }
        let mut name = format!("app-{bin}.log");
        while !used.insert(filename_key(&name)) {
            name = format!("app-{name}");
        }
        app_logs.insert(bin, name);
    }
    app_logs
}

fn filename_key(name: &str) -> String {
    name.to_lowercase()
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

fn timestamp(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let days = (elapsed.as_secs() / 86_400) as i64;
    let seconds = elapsed.as_secs() % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        elapsed.subsec_millis()
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TrySendError;

    #[test]
    fn truncates_session_logs_strips_ansi_and_rotates_once() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("api.log");
        let backup = backup_path(&active);
        fs::write(&active, "old session").unwrap();
        fs::write(&backup, "older rotation").unwrap();
        let (bus, _rx) = LogBus::channel();
        let mut logs = LogFiles::start_with_limit(dir.path(), &["api".into()], bus, 75).unwrap();
        let mut sink = logs.sink();
        for (raw, stream) in [
            ("\x1b[31mfirst\x1b[0m", Stream::Stderr),
            ("second", Stream::Stdout),
            ("third", Stream::Stderr),
        ] {
            sink.write(&DevEvent::Log(LogLine {
                ts: UNIX_EPOCH,
                source: LogSource::App("api".into()),
                stream,
                raw: raw.into(),
            }));
        }
        sink.finish();
        drop(sink);
        logs.finish();

        let current = fs::read_to_string(&active).unwrap();
        let rotated = fs::read_to_string(&backup).unwrap();
        let all = format!("{current}{rotated}");
        assert!(current.contains("third"), "{current:?}");
        assert!(rotated.contains("[stderr] first"), "{rotated:?}");
        assert!(rotated.contains("second"), "{rotated:?}");
        assert!(!all.contains("old session"));
        assert!(!all.contains("older rotation"));
        assert!(!all.contains('\x1b'));
        assert!(current.starts_with("1970-01-01T00:00:00.000Z "));
        assert!(dir.path().join("build.log").is_file());
    }

    #[test]
    fn build_binary_uses_a_distinct_file_from_the_build_log() {
        let dir = tempfile::tempdir().unwrap();
        let (bus, _rx) = LogBus::channel();
        let mut logs =
            LogFiles::start_with_limit(dir.path(), &["build".into()], bus, 1024).unwrap();
        let mut sink = logs.sink();
        sink.write(&DevEvent::Log(LogLine {
            ts: UNIX_EPOCH,
            source: LogSource::Build,
            stream: Stream::Stdout,
            raw: "cargo output".into(),
        }));
        sink.write(&DevEvent::Log(LogLine {
            ts: UNIX_EPOCH,
            source: LogSource::App("build".into()),
            stream: Stream::Stdout,
            raw: "app output".into(),
        }));
        sink.finish();
        logs.finish();

        let build_log = fs::read_to_string(dir.path().join("build.log")).unwrap();
        let app_log = fs::read_to_string(dir.path().join("app-build.log")).unwrap();
        assert!(build_log.contains("cargo output"), "{build_log:?}");
        assert!(!build_log.contains("app output"), "{build_log:?}");
        assert!(app_log.contains("app output"), "{app_log:?}");
        assert!(!app_log.contains("cargo output"), "{app_log:?}");
    }

    #[test]
    fn colliding_app_log_filenames_are_prefixed_until_unique() {
        let dir = tempfile::tempdir().unwrap();
        let bins = ["build".into(), "app-build".into()];
        let mapping = app_log_names(&bins);
        let reverse_mapping = app_log_names(&["app-build".into(), "build".into()]);
        assert_eq!(mapping, reverse_mapping);
        assert_eq!(
            mapping.get("build").map(String::as_str),
            Some("app-app-build.log")
        );
        assert_eq!(
            mapping.get("app-build").map(String::as_str),
            Some("app-build.log")
        );

        let (bus, _rx) = LogBus::channel();
        let mut logs = LogFiles::start_with_limit(dir.path(), &bins, bus, 1024).unwrap();
        let mut sink = logs.sink();
        sink.write(&DevEvent::Log(LogLine {
            ts: UNIX_EPOCH,
            source: LogSource::Build,
            stream: Stream::Stdout,
            raw: "pipeline output".into(),
        }));
        for (bin, raw) in [("build", "build app"), ("app-build", "prefixed app")] {
            sink.write(&DevEvent::Log(LogLine {
                ts: UNIX_EPOCH,
                source: LogSource::App(bin.into()),
                stream: Stream::Stdout,
                raw: raw.into(),
            }));
        }
        sink.finish();
        logs.finish();

        let pipeline = fs::read_to_string(dir.path().join("build.log")).unwrap();
        let first = fs::read_to_string(dir.path().join("app-build.log")).unwrap();
        let second = fs::read_to_string(dir.path().join("app-app-build.log")).unwrap();
        assert!(pipeline.contains("pipeline output"), "{pipeline:?}");
        assert!(!pipeline.contains("build app"), "{pipeline:?}");
        assert!(first.contains("prefixed app"), "{first:?}");
        assert!(!first.contains("build app"), "{first:?}");
        assert!(second.contains("build app"), "{second:?}");
        assert!(!second.contains("prefixed app"), "{second:?}");
    }

    #[test]
    fn writer_queue_has_a_fixed_capacity() {
        let (tx, _rx) = writer_channel(1);
        let message = || {
            Message::Line(
                "build.log".into(),
                LogLine {
                    ts: UNIX_EPOCH,
                    source: LogSource::Build,
                    stream: Stream::Stdout,
                    raw: "line".into(),
                },
            )
        };

        tx.try_send(message()).unwrap();
        assert!(matches!(tx.try_send(message()), Err(TrySendError::Full(_))));
    }
}
