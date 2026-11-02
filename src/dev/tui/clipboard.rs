use std::io::{self, Write};
use std::process::{Command, Stdio};

use base64::{Engine as _, engine::general_purpose::STANDARD};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeCommand {
    program: &'static str,
    args: &'static [&'static str],
    label: &'static str,
}

#[derive(Debug, Default)]
pub(super) struct CopyOutcome {
    pub(super) osc52: bool,
    pub(super) native: Option<&'static str>,
    pub(super) error: Option<String>,
}

pub(super) fn copy(payload: &str, tmux: bool, writer: &mut impl Write) -> CopyOutcome {
    let mut outcome = CopyOutcome::default();
    match writer
        .write_all(&osc52_sequence(payload, tmux))
        .and_then(|()| writer.flush())
    {
        Ok(()) => outcome.osc52 = true,
        Err(error) => outcome.error = Some(format!("OSC 52: {error}")),
    }

    match native_copy(
        payload,
        current_platform(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
    ) {
        Ok(label) => outcome.native = Some(label),
        Err(error) => {
            if let Some(previous) = outcome.error.take() {
                outcome.error = Some(format!("{previous}; native: {error}"));
            } else {
                outcome.error = Some(format!("native: {error}"));
            }
        }
    }
    outcome
}

fn osc52_sequence(payload: &str, tmux: bool) -> Vec<u8> {
    let encoded = STANDARD.encode(payload);
    let mut sequence = format!("\x1b]52;c;{encoded}\x07").into_bytes();
    if !tmux {
        return sequence;
    }
    let mut wrapped = Vec::with_capacity(sequence.len() + 16);
    wrapped.extend_from_slice(b"\x1bPtmux;");
    for byte in sequence.drain(..) {
        wrapped.push(byte);
        if byte == 0x1b {
            wrapped.push(byte);
        }
    }
    wrapped.extend_from_slice(b"\x1b\\");
    wrapped
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Other
    }
}

fn native_commands(platform: Platform, wayland: bool) -> Vec<NativeCommand> {
    const PBCOPY: NativeCommand = NativeCommand {
        program: "pbcopy",
        args: &[],
        label: "pbcopy",
    };
    const WL_COPY: NativeCommand = NativeCommand {
        program: "wl-copy",
        args: &[],
        label: "wl-copy",
    };
    const XCLIP: NativeCommand = NativeCommand {
        program: "xclip",
        args: &["-selection", "clipboard"],
        label: "xclip",
    };
    const XSEL: NativeCommand = NativeCommand {
        program: "xsel",
        args: &["-b"],
        label: "xsel",
    };
    const CLIP: NativeCommand = NativeCommand {
        program: "clip.exe",
        args: &[],
        label: "clip.exe",
    };

    match platform {
        Platform::MacOs => vec![PBCOPY],
        Platform::Linux if wayland => vec![WL_COPY, XCLIP, XSEL],
        Platform::Linux => vec![XCLIP, XSEL],
        Platform::Windows => vec![CLIP],
        Platform::Other => Vec::new(),
    }
}

fn native_copy(payload: &str, platform: Platform, wayland: bool) -> Result<&'static str, String> {
    let commands = native_commands(platform, wayland);
    if commands.is_empty() {
        return Err("no supported native clipboard command".to_string());
    }

    let mut failures = Vec::new();
    for command in commands {
        match run_command(command, payload) {
            Ok(true) => return Ok(command.label),
            Ok(false) => failures.push(format!("{} exited unsuccessfully", command.label)),
            Err(error) => failures.push(format!("{}: {error}", command.label)),
        }
    }
    Err(failures.join(", "))
}

fn run_command(command: NativeCommand, payload: &str) -> io::Result<bool> {
    let mut child = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("clipboard stdin unavailable"));
    };
    if let Err(error) = stdin.write_all(payload.as_bytes()) {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    drop(stdin);
    child.wait().map(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_payload_and_tmux_passthrough() {
        assert_eq!(
            osc52_sequence("api: hi", false),
            b"\x1b]52;c;YXBpOiBoaQ==\x07"
        );
        assert_eq!(
            osc52_sequence("x", true),
            b"\x1bPtmux;\x1b\x1b]52;c;eA==\x07\x1b\\"
        );
    }

    #[test]
    fn native_command_order_matches_platform_and_wayland() {
        let wayland = native_commands(Platform::Linux, true);
        assert_eq!(
            wayland.iter().map(|item| item.label).collect::<Vec<_>>(),
            ["wl-copy", "xclip", "xsel"]
        );
        let x11 = native_commands(Platform::Linux, false);
        assert_eq!(
            x11.iter().map(|item| item.label).collect::<Vec<_>>(),
            ["xclip", "xsel"]
        );
        assert_eq!(native_commands(Platform::MacOs, false)[0].label, "pbcopy");
        assert_eq!(
            native_commands(Platform::Windows, false)[0].label,
            "clip.exe"
        );
        assert!(native_commands(Platform::Other, false).is_empty());
    }
}
