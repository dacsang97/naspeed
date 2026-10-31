//! Runs `cargo check` / `cargo build` with JSON messages: collects rendered
//! compiler errors and the built executable, while the caller keeps polling
//! (to record file changes as "dirty" and to honor Ctrl-C) instead of
//! blocking on cargo.

use std::io::{BufRead, BufReader, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use super::process_supervisor::{GroupChild, STOP_GRACE};

/// `cargo` from `$CARGO` (set when run under cargo) or `PATH`.
pub fn cargo_command() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// Colored rendered diagnostics when a human is watching, plain otherwise
/// (logs, CI, tests).
pub fn message_format() -> &'static str {
    if std::io::stderr().is_terminal() {
        "--message-format=json-diagnostic-rendered-ansi"
    } else {
        "--message-format=json"
    }
}

#[derive(Debug, Default)]
pub struct CargoOutcome {
    pub success: bool,
    /// Rendered `error`-level diagnostics, in cargo's order.
    pub errors: Vec<String>,
    /// Executables produced by bin targets: (target name, path).
    pub executables: Vec<(String, PathBuf)>,
    /// cargo's own stderr, for failures that carry no compiler diagnostic
    /// (bad manifest, unknown package, lock contention errors...).
    pub stderr: String,
    /// The caller aborted the run (shutdown requested).
    pub interrupted: bool,
}

#[derive(Debug, Default)]
struct Messages {
    errors: Vec<String>,
    executables: Vec<(String, PathBuf)>,
}

/// Folds one line of cargo's JSON stream into `out`.
fn parse_message(line: &str, out: &mut Messages) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match msg["reason"].as_str() {
        Some("compiler-message") => {
            let diag = &msg["message"];
            let level = diag["level"].as_str().unwrap_or_default();
            let text = diag["message"].as_str().unwrap_or_default();
            // The trailing "aborting due to N previous errors" is a summary, not an error.
            if level.starts_with("error") && !text.starts_with("aborting due to") {
                let rendered = diag["rendered"].as_str().unwrap_or(text);
                out.errors.push(rendered.to_string());
            }
        }
        Some("compiler-artifact") => {
            if let (Some(name), Some(exe)) =
                (msg["target"]["name"].as_str(), msg["executable"].as_str())
            {
                out.executables.push((name.to_string(), PathBuf::from(exe)));
            }
        }
        _ => {}
    }
}

fn spawn_reader<R: Read + Send + 'static, T: Send + 'static>(
    pipe: R,
    mut fold: impl FnMut(&mut T, String) + Send + 'static,
    init: T,
) -> JoinHandle<T> {
    std::thread::spawn(move || {
        let mut acc = init;
        for line in BufReader::new(pipe).lines() {
            match line {
                Ok(line) => fold(&mut acc, line),
                Err(_) => break,
            }
        }
        acc
    })
}

/// Runs `cargo <args>` in `root`. `tick` is called every few milliseconds
/// while cargo runs; returning `true` aborts the run (whole process group).
pub fn run_cargo(
    root: &Path,
    args: &[String],
    mut tick: impl FnMut() -> bool,
) -> Result<CargoOutcome> {
    let mut cmd = cargo_command();
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = GroupChild::spawn(cmd)?;
    let stdout = child.child_mut().stdout.take().context("cargo stdout")?;
    let stderr = child.child_mut().stderr.take().context("cargo stderr")?;
    let messages = spawn_reader(
        stdout,
        |acc: &mut Messages, line| parse_message(&line, acc),
        Messages::default(),
    );
    let stderr_text = spawn_reader(
        stderr,
        |acc: &mut String, line| {
            acc.push_str(&line);
            acc.push('\n');
        },
        String::new(),
    );

    let mut interrupted = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if tick() {
            interrupted = true;
            break child.stop(STOP_GRACE);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Leftover members (e.g. a stuck build script) must not hold the pipes open.
    child.stop(STOP_GRACE);
    let messages = messages.join().unwrap_or_default();
    let stderr = stderr_text.join().unwrap_or_default();
    Ok(CargoOutcome {
        success: !interrupted && status.is_some_and(|s| s.success()),
        errors: messages.errors,
        executables: messages.executables,
        stderr,
        interrupted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_errors_and_executables() {
        let lines = [
            r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused","rendered":"warning: unused\n"}}"#,
            r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","rendered":"error[E0308]: mismatched types\n"}}"#,
            r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error","rendered":"error: aborting\n"}}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"api","kind":["bin"]},"executable":"/t/debug/api"}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"core","kind":["lib"]},"executable":null}"#,
            "not json",
        ];
        let mut out = Messages::default();
        for l in lines {
            parse_message(l, &mut out);
        }
        assert_eq!(
            out.errors,
            vec!["error[E0308]: mismatched types\n".to_string()]
        );
        assert_eq!(
            out.executables,
            vec![("api".to_string(), PathBuf::from("/t/debug/api"))]
        );
    }
}
