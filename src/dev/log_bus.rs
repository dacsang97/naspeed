//! Ordered, cloneable event transport shared by the loop and output readers.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogSource {
    Ns,
    Build,
    Test,
    App(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub ts: SystemTime,
    pub source: LogSource,
    pub stream: Stream,
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Starting,
    Running { pid: u32 },
    Building,
    Crashed { code: Option<i32> },
    Stopped,
}

#[derive(Debug, Clone)]
pub enum DevEvent {
    Log(LogLine),
    Status {
        bin: String,
        state: AppState,
    },
    Cycle {
        ok: bool,
        elapsed: Duration,
        errors: usize,
    },
    Shutdown,
}

pub trait Sink: Send {
    fn write(&mut self, event: &DevEvent);

    fn finish(&mut self) {}
}

#[derive(Clone)]
pub struct LogBus(SyncSender<DevEvent>);

impl LogBus {
    pub fn channel() -> (LogBus, Receiver<DevEvent>) {
        Self::channel_with_capacity(EVENT_QUEUE_CAPACITY)
    }

    fn channel_with_capacity(capacity: usize) -> (LogBus, Receiver<DevEvent>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (LogBus(tx), rx)
    }

    pub fn log(&self, source: LogSource, stream: Stream, raw: impl Into<String>) {
        let _ = self.0.send(DevEvent::Log(LogLine {
            ts: SystemTime::now(),
            source,
            stream,
            raw: raw.into(),
        }));
    }

    /// Sends a diagnostic without blocking a bounded queue's consumer path.
    pub(super) fn try_log(
        &self,
        source: LogSource,
        stream: Stream,
        raw: impl Into<String>,
    ) -> bool {
        matches!(
            self.0.try_send(DevEvent::Log(LogLine {
                ts: SystemTime::now(),
                source,
                stream,
                raw: raw.into(),
            })),
            Ok(())
        )
    }

    pub fn status(&self, bin: impl Into<String>, state: AppState) {
        let _ = self.0.send(DevEvent::Status {
            bin: bin.into(),
            state,
        });
    }

    pub fn cycle(&self, ok: bool, elapsed: Duration, errors: usize) {
        let _ = self.0.send(DevEvent::Cycle {
            ok,
            elapsed,
            errors,
        });
    }

    pub fn shutdown(&self) {
        let _ = self.0.send(DevEvent::Shutdown);
    }
}

pub struct FanOut {
    sinks: Vec<Box<dyn Sink>>,
}

impl FanOut {
    pub fn new(sinks: Vec<Box<dyn Sink>>) -> FanOut {
        FanOut { sinks }
    }

    fn write(&mut self, event: &DevEvent) {
        for sink in &mut self.sinks {
            sink.write(event);
        }
    }

    fn finish(&mut self) {
        for sink in &mut self.sinks {
            sink.finish();
        }
    }
}

pub fn route(rx: Receiver<DevEvent>, sinks: Vec<Box<dyn Sink>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut fanout = FanOut::new(sinks);
        while let Ok(event) = rx.recv() {
            if matches!(event, DevEvent::Shutdown) {
                while let Ok(queued) = rx.try_recv() {
                    if !matches!(queued, DevEvent::Shutdown) {
                        fanout.write(&queued);
                    }
                }
                break;
            }
            fanout.write(&event);
        }
        fanout.finish();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn bounded_queue_rejects_best_effort_logs_and_preserves_controls() {
        let (bus, rx) = LogBus::channel_with_capacity(1);
        bus.log(LogSource::Build, Stream::Stdout, "first");
        assert!(!bus.try_log(LogSource::Build, Stream::Stdout, "dropped"));

        let producer = bus.clone();
        let writer = thread::spawn(move || {
            producer.status("api", AppState::Running { pid: 42 });
            producer.shutdown();
        });

        assert!(matches!(rx.recv().unwrap(), DevEvent::Log(line) if line.raw == "first"));
        assert!(
            matches!(rx.recv().unwrap(), DevEvent::Status { bin, state: AppState::Running { pid: 42 } } if bin == "api")
        );
        assert!(matches!(rx.recv().unwrap(), DevEvent::Shutdown));
        writer.join().unwrap();
    }

    #[derive(Clone)]
    struct RecordingSink(Arc<Mutex<Vec<&'static str>>>);

    impl Sink for RecordingSink {
        fn write(&mut self, event: &DevEvent) {
            if matches!(event, DevEvent::Log(_)) {
                self.0.lock().unwrap().push("log");
            }
        }

        fn finish(&mut self) {
            self.0.lock().unwrap().push("finish");
        }
    }

    #[test]
    fn route_drains_queued_events_before_finishing_sinks() {
        let (bus, rx) = LogBus::channel_with_capacity(4);
        let order = Arc::new(Mutex::new(Vec::new()));
        bus.log(LogSource::Build, Stream::Stdout, "before shutdown");
        bus.shutdown();
        bus.log(LogSource::Build, Stream::Stdout, "queued during shutdown");

        route(rx, vec![Box::new(RecordingSink(Arc::clone(&order)))])
            .join()
            .unwrap();

        assert_eq!(*order.lock().unwrap(), ["log", "log", "finish"]);
    }

    #[test]
    fn route_finishes_sinks_when_all_senders_disconnect() {
        let (bus, rx) = LogBus::channel_with_capacity(1);
        let order = Arc::new(Mutex::new(Vec::new()));
        drop(bus);

        route(rx, vec![Box::new(RecordingSink(Arc::clone(&order)))])
            .join()
            .unwrap();

        assert_eq!(*order.lock().unwrap(), ["finish"]);
    }
}
