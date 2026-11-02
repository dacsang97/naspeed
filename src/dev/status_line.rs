//! `[ns]` status output is carried as events so plain, file and TUI sinks agree.

use std::fmt::Display;
use std::io::IsTerminal;
use std::time::Duration;

use super::log_bus::{LogBus, LogSource, Stream};

fn prefix(color: &str) -> String {
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[1;{color}m[ns]\x1b[0m")
    } else {
        "[ns]".to_string()
    }
}

fn emit(bus: &LogBus, source: LogSource, color: &str, msg: impl Display) {
    bus.log(source, Stream::Stderr, format!("{} {msg}", prefix(color)));
}

pub fn info(bus: &LogBus, msg: impl Display) {
    emit(bus, LogSource::Ns, "36", msg);
}

pub fn ok(bus: &LogBus, msg: impl Display) {
    emit(bus, LogSource::Ns, "32", msg);
}

pub fn warn(bus: &LogBus, msg: impl Display) {
    emit(bus, LogSource::Ns, "33", msg);
}

pub fn error(bus: &LogBus, msg: impl Display) {
    emit(bus, LogSource::Ns, "31", msg);
}

/// Prints the first rendered compiler error and the total count.
pub fn compile_errors(bus: &LogBus, step: &str, errors: &[String], elapsed: Duration) {
    if let Some(first) = errors.first() {
        bus.log(
            LogSource::Build,
            Stream::Stderr,
            first.trim_end_matches('\n'),
        );
    }
    let n = errors.len();
    error(
        bus,
        format!(
            "{step} failed: {n} error{} in {} (previous process keeps running)",
            if n == 1 { "" } else { "s" },
            ms(elapsed)
        ),
    );
}

/// Human-friendly duration: `850 ms` or `3.2 s`.
pub fn ms(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(ms(Duration::from_millis(850)), "850 ms");
        assert_eq!(ms(Duration::from_millis(3240)), "3.2 s");
    }
}
