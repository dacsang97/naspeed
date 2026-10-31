//! Tree-shaped log lines on stderr:
//!
//! ```text
//! • exporting store                        dir=/home/runner/.naspeed-ci
//!   • exported                             entries=347 objects=812 size=1.2 GiB
//!     • took: 12s
//! ```
//!
//! Top-level steps are bold, nested lines come from [`indent`], and fields
//! line up in one column. Colors are on for a terminal and in GitHub Actions,
//! off with `NO_COLOR`. Wrapper mode never prints through here: its stderr
//! belongs to rustc.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Column where fields start, counted from the line start.
const FIELD_COLUMN: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
    /// The italic `took: 12s` line under a slow step.
    Took,
}

/// Process-wide, so lines from worker threads (parallel release builds) nest
/// under the step that started them.
static DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Nests the lines printed while the guard lives one level deeper.
#[must_use = "the indent ends when the guard is dropped"]
pub struct Indent(());

impl Drop for Indent {
    fn drop(&mut self) {
        // Each guard comes from one `indent()`, so this never goes below zero.
        DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn indent() -> Indent {
    DEPTH.fetch_add(1, Ordering::Relaxed);
    Indent(())
}

/// Starting depth handed down by a parent process (naspeed-action, `ns test`
/// for the test runners cargo starts), so a child's lines nest under the
/// parent's step.
pub const INDENT_ENV: &str = "NASPEED_UI_INDENT";

fn base_depth() -> usize {
    static BASE: OnceLock<usize> = OnceLock::new();
    *BASE.get_or_init(|| {
        std::env::var(INDENT_ENV)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map_or(0, |d| d.min(8))
    })
}

pub fn depth() -> usize {
    base_depth() + DEPTH.load(Ordering::Relaxed)
}

/// `NO_COLOR` wins, then an explicit force (`CLICOLOR_FORCE`,
/// `CARGO_TERM_COLOR`), then GitHub Actions (its log viewer renders ANSI),
/// then whether stderr is a terminal.
fn decide_color(env: impl Fn(&str) -> Option<String>, terminal: bool) -> bool {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if set("NO_COLOR") {
        return false;
    }
    if env("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    match env("CARGO_TERM_COLOR").as_deref() {
        Some("always") => return true,
        Some("never") => return false,
        _ => {}
    }
    env("GITHUB_ACTIONS").as_deref() == Some("true") || terminal
}

pub fn color_enabled() -> bool {
    static COLOR: OnceLock<bool> = OnceLock::new();
    *COLOR.get_or_init(|| decide_color(|n| std::env::var(n).ok(), std::io::stderr().is_terminal()))
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// One rendered line, without the trailing newline.
pub fn format_line(
    depth: usize,
    level: Level,
    msg: &str,
    fields: &[(&str, &dyn Display)],
    color: bool,
) -> String {
    let (bullet, bullet_color) = match level {
        Level::Info | Level::Took => ("•", "34"),
        Level::Warn => ("•", "33"),
        Level::Error => ("⨯", "31"),
    };
    let lead = "  ".repeat(depth);
    let mut line = format!("{lead}{} ", paint(bullet, bullet_color, color));
    let msg_style = match (level, depth) {
        (Level::Error, _) => "1;31",
        (Level::Warn, _) => "33",
        (Level::Took, _) => "3",
        (Level::Info, 0) => "1",
        (Level::Info, _) => "",
    };
    line.push_str(&if msg_style.is_empty() {
        msg.to_string()
    } else {
        paint(msg, msg_style, color)
    });
    if !fields.is_empty() {
        // Pad on the visible width; escape codes take no columns.
        let visible = lead.len() + 2 + msg.chars().count();
        line.push_str(&" ".repeat(FIELD_COLUMN.saturating_sub(visible).max(1)));
        let rendered: Vec<String> = fields
            .iter()
            .map(|(k, v)| format!("{}={v}", paint(k, "1;34", color)))
            .collect();
        line.push_str(&rendered.join(" "));
    }
    line
}

pub fn emit(level: Level, msg: &dyn Display, fields: &[(&str, &dyn Display)]) {
    eprintln!(
        "{}",
        format_line(depth(), level, &msg.to_string(), fields, color_enabled())
    );
}

/// `850ms`, `12s`, `4m21s`, `1h02m`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{}ms", d.as_millis()),
        1..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// `• took: 12s` one level below the current one, for steps of a second or
/// more (shorter ones are not worth a line).
pub fn took(start: Instant) {
    let elapsed = start.elapsed();
    if elapsed < Duration::from_secs(1) {
        return;
    }
    let msg = format!("took: {}", format_duration(elapsed));
    eprintln!(
        "{}",
        format_line(depth() + 1, Level::Took, &msg, &[], color_enabled())
    );
}

/// `ui_info!("exported"; entries = n, size = human)`: a log line at the
/// current depth with optional `key = value` fields.
#[macro_export]
macro_rules! ui_info {
    ($msg:expr $(; $($k:ident = $v:expr),+ $(,)?)?) => {
        $crate::ui::emit($crate::ui::Level::Info, &$msg,
            &[$($((stringify!($k), &$v as &dyn ::std::fmt::Display)),+)?])
    };
}

#[macro_export]
macro_rules! ui_warn {
    ($msg:expr $(; $($k:ident = $v:expr),+ $(,)?)?) => {
        $crate::ui::emit($crate::ui::Level::Warn, &$msg,
            &[$($((stringify!($k), &$v as &dyn ::std::fmt::Display)),+)?])
    };
}

#[macro_export]
macro_rules! ui_error {
    ($msg:expr $(; $($k:ident = $v:expr),+ $(,)?)?) => {
        $crate::ui::emit($crate::ui::Level::Error, &$msg,
            &[$($((stringify!($k), &$v as &dyn ::std::fmt::Display)),+)?])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_nest_and_align_fields() {
        let top = format_line(0, Level::Info, "exporting store", &[("dir", &"/ci")], false);
        assert_eq!(top, format!("• exporting store{}dir=/ci", " ".repeat(23)));
        let n = 3;
        let nested = format_line(
            1,
            Level::Info,
            "exported",
            &[("entries", &n), ("size", &"1 KiB")],
            false,
        );
        assert_eq!(
            nested,
            format!("  • exported{}entries=3 size=1 KiB", " ".repeat(28))
        );
        assert_eq!(
            format_line(2, Level::Warn, "skipped", &[], false),
            "    • skipped"
        );
        assert_eq!(
            format_line(0, Level::Error, "failed", &[], false),
            "⨯ failed"
        );
    }

    #[test]
    fn long_messages_keep_one_space_before_fields() {
        let msg = "m".repeat(60);
        let line = format_line(0, Level::Info, &msg, &[("k", &1)], false);
        assert_eq!(line, format!("• {msg} k=1"));
    }

    #[test]
    fn colors_wrap_bullet_message_and_keys_only() {
        let line = format_line(0, Level::Info, "step", &[("k", &"v")], true);
        assert!(
            line.starts_with("\x1b[34m•\x1b[0m \x1b[1mstep\x1b[0m"),
            "{line:?}"
        );
        assert!(line.ends_with("\x1b[1;34mk\x1b[0m=v"), "{line:?}");
        // Alignment ignores escape codes.
        let plain = format_line(0, Level::Info, "step", &[("k", &"v")], false);
        let stripped = line
            .replace("\x1b[34m", "")
            .replace("\x1b[1m", "")
            .replace("\x1b[1;34m", "")
            .replace("\x1b[0m", "");
        assert_eq!(stripped, plain);
    }

    #[test]
    fn indent_guard_restores_depth() {
        let base = depth();
        {
            let _a = indent();
            let _b = indent();
            assert_eq!(depth(), base + 2);
        }
        assert_eq!(depth(), base);
    }

    #[test]
    fn durations_are_short_and_readable() {
        assert_eq!(format_duration(Duration::from_millis(850)), "850ms");
        assert_eq!(format_duration(Duration::from_secs(12)), "12s");
        assert_eq!(format_duration(Duration::from_secs(261)), "4m21s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h02m");
    }

    #[test]
    fn color_rules() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |n: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(!decide_color(env(&[]), false));
        assert!(decide_color(env(&[]), true));
        assert!(decide_color(env(&[("GITHUB_ACTIONS", "true")]), false));
        assert!(!decide_color(
            env(&[("NO_COLOR", "1"), ("GITHUB_ACTIONS", "true")]),
            true
        ));
        assert!(!decide_color(env(&[("CARGO_TERM_COLOR", "never")]), true));
        assert!(decide_color(env(&[("CARGO_TERM_COLOR", "always")]), false));
        assert!(decide_color(env(&[("CLICOLOR_FORCE", "1")]), false));
        assert!(!decide_color(env(&[("CLICOLOR_FORCE", "0")]), false));
    }
}
