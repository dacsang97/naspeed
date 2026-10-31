//! `[ns]`-prefixed status lines on stderr, kept visually apart from the app's
//! own stdout/stderr (which the app writes directly to the terminal).

use std::fmt::Display;
use std::io::IsTerminal;
use std::time::Duration;

fn prefix(color: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!("\x1b[1;{color}m[ns]\x1b[0m")
    } else {
        "[ns]".to_string()
    }
}

pub fn info(msg: impl Display) {
    eprintln!("{} {msg}", prefix("36"));
}

pub fn ok(msg: impl Display) {
    eprintln!("{} {msg}", prefix("32"));
}

pub fn warn(msg: impl Display) {
    eprintln!("{} {msg}", prefix("33"));
}

pub fn error(msg: impl Display) {
    eprintln!("{} {msg}", prefix("31"));
}

/// Prints the first rendered compiler error and the total count. Rendered
/// diagnostics already end with a newline and carry their own colors.
pub fn compile_errors(step: &str, errors: &[String], elapsed: Duration) {
    if let Some(first) = errors.first() {
        eprintln!("{}", first.trim_end_matches('\n'));
    }
    let n = errors.len();
    error(format!(
        "{step} failed: {n} error{} in {} (previous process keeps running)",
        if n == 1 { "" } else { "s" },
        ms(elapsed)
    ));
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
