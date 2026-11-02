//! The terminal's default background, asked once with OSC 11, so the
//! composer can sit on a surface shaded from it in either theme (as Codex
//! shades its composer). Terminals that do not answer get no surface.

use std::time::Duration;

use ratatui::style::Color;

/// How long startup waits for the terminal to answer.
pub(super) const QUERY_TIMEOUT: Duration = Duration::from_millis(150);

/// The composer surface for a terminal background: white over a dark
/// background, black over a light one, blended thinly.
pub(super) fn surface(background: (u8, u8, u8), truecolor: bool) -> Color {
    let (top, alpha) = if luma(background) > 128.0 {
        ((0, 0, 0), 0.06)
    } else {
        ((255, 255, 255), 0.10)
    };
    let shaded = blend(top, background, alpha);
    if truecolor {
        Color::Rgb(shaded.0, shaded.1, shaded.2)
    } else {
        Color::Indexed(ansi256(shaded))
    }
}

/// Whether the terminal advertises 24-bit colour.
pub(super) fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"))
}

/// Ask the terminal for its default background. Raw mode must be on, so
/// the reply is neither echoed nor line-buffered.
#[cfg(unix)]
pub(super) fn query(timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    tty.write_all(b"\x1b]11;?\x1b\\").ok()?;
    tty.flush().ok()?;
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || reply.len() > 256 {
            return None;
        }
        let mut poll = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: `poll` points at one initialised pollfd for an open fd.
        if unsafe { libc::poll(&raw mut poll, 1, millis) } <= 0 {
            return None;
        }
        let mut chunk = [0u8; 64];
        let read = tty.read(&mut chunk).ok().filter(|read| *read > 0)?;
        reply.extend_from_slice(&chunk[..read]);
        if let Some(color) = parse_reply(&reply) {
            return Some(color);
        }
    }
}

#[cfg(not(unix))]
pub(super) fn query(_timeout: Duration) -> Option<(u8, u8, u8)> {
    None
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` ended by BEL or `ESC \`; each channel
/// has 1–4 hex digits.
fn parse_reply(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(reply).ok()?;
    let body = &text[text.find("]11;rgb:")? + "]11;rgb:".len()..];
    let end = body.find(['\x07', '\x1b'])?;
    let mut channels = body[..end].split('/').map(|hex| {
        let digits = u32::try_from(hex.len())
            .ok()
            .filter(|len| (1..=4).contains(len))?;
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = 16u32.pow(digits) - 1;
        u8::try_from(value * 255 / max).ok()
    });
    let color = (channels.next()??, channels.next()??, channels.next()??);
    channels.next().is_none().then_some(color)
}

fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
}

fn blend(top: (u8, u8, u8), bottom: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let mix = |a: u8, b: u8| (f32::from(a) * alpha + f32::from(b) * (1.0 - alpha)).round() as u8;
    (
        mix(top.0, bottom.0),
        mix(top.1, bottom.1),
        mix(top.2, bottom.2),
    )
}

/// The nearest xterm-256 colour: the 6×6×6 cube or the grey ramp.
fn ansi256(rgb: (u8, u8, u8)) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |c: u8| {
        (0..6)
            .min_by_key(|&i| (i32::from(LEVELS[i]) - i32::from(c)).abs())
            .unwrap_or(0)
    };
    let (r, g, b) = (level(rgb.0), level(rgb.1), level(rgb.2));
    let cube = (LEVELS[r], LEVELS[g], LEVELS[b]);
    let average = (u32::from(rgb.0) + u32::from(rgb.1) + u32::from(rgb.2)) / 3;
    let grey_step = ((average.saturating_sub(8) + 5) / 10).min(23) as u8;
    let grey_value = 8 + 10 * grey_step;
    let distance = |c: (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(c.0, rgb.0) + d(c.1, rgb.1) + d(c.2, rgb.2)
    };
    if distance((grey_value, grey_value, grey_value)) < distance(cube) {
        232 + grey_step
    } else {
        16 + 36 * r as u8 + 6 * g as u8 + b as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_four_two_and_one_digit_channels_with_either_terminator() {
        assert_eq!(
            parse_reply(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"),
            Some((30, 30, 46))
        );
        assert_eq!(
            parse_reply(b"\x1b]11;rgb:ff/80/00\x07"),
            Some((255, 128, 0))
        );
        assert_eq!(parse_reply(b"\x1b]11;rgb:f/0/f\x07"), Some((255, 0, 255)));
        assert_eq!(parse_reply(b"\x1b]11;rgb:ffff/ffff"), None, "incomplete");
        assert_eq!(parse_reply(b"\x1b]11;rgb:1/2\x07"), None);
    }

    #[test]
    fn the_surface_lightens_a_dark_background_and_darkens_a_light_one() {
        assert_eq!(surface((0, 0, 0), true), Color::Rgb(26, 26, 26));
        assert_eq!(surface((255, 255, 255), true), Color::Rgb(240, 240, 240));
    }

    #[test]
    fn without_truecolor_the_surface_takes_the_nearest_palette_entry() {
        assert_eq!(ansi256((26, 26, 26)), 234);
        assert_eq!(ansi256((255, 0, 0)), 196);
        assert_eq!(surface((30, 30, 46), false), Color::Indexed(237));
    }
}
