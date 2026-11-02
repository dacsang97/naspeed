//! No-TTY renderer. Status and cargo lines stay on stderr; app output goes to stdout.

use std::borrow::Cow;
use std::io::{IsTerminal, Write, stderr, stdout};

use super::ansi_strip::strip_ansi;
use super::log_bus::{DevEvent, LogLine, LogSource, Sink};

#[derive(Default)]
pub struct PlainRenderer;

impl Sink for PlainRenderer {
    fn write(&mut self, event: &DevEvent) {
        let DevEvent::Log(line) = event else {
            return;
        };
        match &line.source {
            LogSource::App(bin) => render_app(line, bin),
            LogSource::Ns | LogSource::Build | LogSource::Test => {
                let mut out = stderr().lock();
                let _ = writeln!(out, "{}", line.raw);
            }
        }
    }

    fn finish(&mut self) {
        let _ = stdout().flush();
        let _ = stderr().flush();
    }
}

fn render_app(line: &LogLine, bin: &str) {
    let mut out = stdout().lock();
    let terminal = out.is_terminal();
    let color = terminal && std::env::var_os("NO_COLOR").is_none();
    let raw = if terminal {
        Cow::Borrowed(line.raw.as_str())
    } else {
        Cow::Owned(strip_ansi(&line.raw))
    };
    let _ = write!(out, "{}{}", app_prefix(bin, color), raw);
    let _ = writeln!(out);
}

fn app_prefix(bin: &str, color: bool) -> String {
    if color {
        format!("\x1b[1;{}m[{bin}]\x1b[0m ", color_for(bin))
    } else {
        format!("[{bin}] ")
    }
}

fn color_for(bin: &str) -> u8 {
    const COLORS: [u8; 6] = [31, 32, 33, 34, 35, 36];
    let hash = bin
        .bytes()
        .fold(0_u32, |hash, byte| hash.wrapping_mul(33) ^ u32::from(byte));
    COLORS[hash as usize % COLORS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_prefix_is_plain_when_color_is_disabled() {
        assert_eq!(app_prefix("api", false), "[api] ");
    }

    #[test]
    fn app_prefix_uses_a_stable_color_when_enabled() {
        let first = app_prefix("api", true);
        assert!(first.starts_with("\x1b[1;"));
        assert!(first.ends_with("\x1b[0m "));
        assert!(first.contains("[api]"));
        assert_eq!(first, app_prefix("api", true));
    }
}
