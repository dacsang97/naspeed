use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::SystemTime;

use super::super::log_bus::{DevEvent, LogLine, LogSource, Sink, Stream};

const TUI_EVENT_CAPACITY: usize = 256;
const DROP_NOTICE_EVERY: usize = 256;

pub(in crate::dev) struct TuiSink {
    tx: SyncSender<DevEvent>,
    dropped_logs: usize,
}

pub(in crate::dev) fn channel() -> (TuiSink, Receiver<DevEvent>) {
    channel_with_capacity(TUI_EVENT_CAPACITY)
}

fn channel_with_capacity(capacity: usize) -> (TuiSink, Receiver<DevEvent>) {
    let (tx, rx) = mpsc::sync_channel(capacity);
    (
        TuiSink {
            tx,
            dropped_logs: 0,
        },
        rx,
    )
}

impl TuiSink {
    fn dropped_notice(&self) -> DevEvent {
        DevEvent::Log(LogLine {
            ts: SystemTime::now(),
            source: LogSource::Ns,
            stream: Stream::Stderr,
            raw: format!(
                "TUI dropped {} log lines because its event queue was full",
                self.dropped_logs
            ),
        })
    }

    fn report_dropped(&mut self) {
        if self.dropped_logs == 0 {
            return;
        }
        let notice = self.dropped_notice();
        self.dropped_logs = 0;
        let _ = self.tx.send(notice);
    }

    fn try_report_dropped(&mut self) {
        if self.dropped_logs < DROP_NOTICE_EVERY {
            return;
        }
        match self.tx.try_send(self.dropped_notice()) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => self.dropped_logs = 0,
            Err(TrySendError::Full(_)) => {}
        }
    }
}

impl Sink for TuiSink {
    fn write(&mut self, event: &DevEvent) {
        if matches!(event, DevEvent::Log(_)) {
            self.try_report_dropped();
            if let Err(TrySendError::Full(_)) = self.tx.try_send(event.clone()) {
                self.dropped_logs = self.dropped_logs.saturating_add(1);
            }
            return;
        }

        self.report_dropped();
        let _ = self.tx.send(event.clone());
    }

    fn finish(&mut self) {
        self.report_dropped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::log_bus::AppState;
    use std::thread;
    use std::time::UNIX_EPOCH;

    fn log(raw: &str) -> DevEvent {
        DevEvent::Log(LogLine {
            ts: UNIX_EPOCH,
            source: LogSource::App("api".into()),
            stream: Stream::Stdout,
            raw: raw.into(),
        })
    }

    #[test]
    fn bounded_queue_drops_only_logs_and_reports_the_count_before_status() {
        let (mut sink, rx) = channel_with_capacity(2);
        sink.write(&log("first"));
        sink.write(&log("second"));
        sink.write(&log("dropped"));

        let writer = thread::spawn(move || {
            sink.write(&DevEvent::Status {
                bin: "api".into(),
                state: AppState::Running { pid: 42 },
            });
            sink.finish();
        });

        assert!(matches!(rx.recv().unwrap(), DevEvent::Log(line) if line.raw == "first"));
        assert!(matches!(rx.recv().unwrap(), DevEvent::Log(line) if line.raw == "second"));
        assert!(
            matches!(rx.recv().unwrap(), DevEvent::Log(line) if line.raw.contains("dropped 1 log lines"))
        );
        assert!(
            matches!(rx.recv().unwrap(), DevEvent::Status { bin, state: AppState::Running { pid: 42 } } if bin == "api")
        );
        writer.join().unwrap();
    }

    #[test]
    fn reports_dropped_lines_on_finish_when_no_control_event_follows() {
        let (mut sink, rx) = channel_with_capacity(1);
        sink.write(&log("kept"));
        sink.write(&log("dropped"));

        let writer = thread::spawn(move || sink.finish());
        assert!(matches!(rx.recv().unwrap(), DevEvent::Log(line) if line.raw == "kept"));
        assert!(
            matches!(rx.recv().unwrap(), DevEvent::Log(line) if line.raw.contains("dropped 1 log lines"))
        );
        writer.join().unwrap();
    }
}
