//! Runs `cargo check` / `cargo build` with JSON messages: collects rendered
//! compiler errors and the built executable, while the caller keeps polling
//! (to record file changes as "dirty" and to honor Ctrl-C) instead of
//! blocking on cargo. cargo's own stderr (`Checking`/`Compiling` progress,
//! build-script warnings, manifest errors) streams live to the build log, so
//! a cold workspace shows what it is compiling instead of sitting silent.

use std::io::{BufRead, BufReader, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use super::line_reader;
use super::log_bus::{LogBus, LogSource, Stream};
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
/// The rustc-workspace driver `ns dev` hands to cargo: `naspeed-rustc-demand`
/// (demand-IR launchers, no link step) when it exists, else the lazy or
/// split-link driver. The env overrides below win; discovery checks next to
/// this binary, its function-tools sibling dir, and the naspeed config dir.
fn dev_driver() -> Option<PathBuf> {
    for (var, file_name) in [
        ("NASPEED_LAZY_RUN_DRIVER", "naspeed-rustc-demand"),
        ("NASPEED_SPLIT_LINK_DRIVER", "naspeed-rustc-split"),
    ] {
        if let Some(driver) = std::env::var_os(var) {
            let driver = PathBuf::from(driver);
            let name_ok = driver
                .file_name()
                .is_some_and(|n| n == file_name || (var == "NASPEED_LAZY_RUN_DRIVER" && n == "naspeed-rustc-lazy"));
            if driver.is_absolute() && name_ok && driver.is_file() {
                // `ns dev` needs demand IR for dep-image natives; a lazy
                // path upgrades to its demand sibling when one was built.
                if var == "NASPEED_LAZY_RUN_DRIVER" {
                    let demand = driver.with_file_name("naspeed-rustc-demand");
                    if demand.is_file() {
                        return Some(demand);
                    }
                    continue;
                }
                return Some(driver);
            }
        }
    }
    for file_name in ["naspeed-rustc-demand", "naspeed-rustc-lazy", "naspeed-rustc-split"] {
        for dir in [
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.to_path_buf())),
            // A dev checkout's `make lazy-driver`/`function-driver` output
            // lives in target/function-tools, a sibling of target/<profile>.
            std::env::current_exe().ok().and_then(|e| {
                e.parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.join("function-tools"))
            }),
            Some(crate::config::config_dir().join("bin")),
        ] {
            if let Some(candidate) = dir.map(|d| d.join(file_name)) {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[derive(Debug, Default)]
pub struct CargoOutcome {
    pub success: bool,
    /// Rendered `error`-level diagnostics, in cargo's order.
    pub errors: Vec<String>,
    /// Executables produced by bin targets: (target name, path).
    pub executables: Vec<(String, PathBuf)>,
    /// The caller aborted the run (shutdown requested).
    pub interrupted: bool,
}

#[derive(Debug, Default)]
struct Messages {
    errors: Vec<String>,
    executables: Vec<(String, PathBuf)>,
    /// Artifact paths of every unit cargo reported, fresh or rebuilt.
    used: Vec<PathBuf>,
}

/// Folds one line of cargo's JSON stream into `out`.
fn parse_message(line: &str, out: &mut Messages) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return;
    };
    out.used
        .extend(crate::gc::target_usage::message_paths(&msg));
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

/// Runs `cargo <args>` in `root`, streaming cargo's stderr to `bus` as build
/// lines. `tick` is called every few milliseconds while cargo runs; returning
/// `true` aborts the run (whole process group).
pub fn run_cargo(
    root: &Path,
    args: &[String],
    bus: &LogBus,
    live: Option<&str>,
    mut tick: impl FnMut() -> bool,
) -> Result<CargoOutcome> {
    let mut cmd = cargo_command();
    // A driver from `make lazy-driver`/`function-driver` routes dev
    // executables through demand IR, a cached dependency dylib or, with
    // `live`, an app patch the running process applies in place.
    if let Some(driver) = dev_driver() {
        cmd.env("RUSTC_WORKSPACE_WRAPPER", &driver);
        if let Some(selectors) = live {
            cmd.env("NASPEED_LAZY_APP_PATCH", selectors);
        }
        // The driver only engages for the pinned rustc it was built
        // against; the `.toolchain` marker `make function-driver` writes
        // names it, so `ns dev` needs no exported RUSTC. Lazy and split
        // drivers are symlinks to the shared function driver, so the marker
        // resolves through the real file. An explicit RUSTC wins.
        if std::env::var_os("RUSTC").is_none() {
            let marker = driver
                .canonicalize()
                .unwrap_or(driver)
                .with_extension("toolchain");
            if let Ok(rustc) = std::fs::read_to_string(marker) {
                let rustc = rustc.trim();
                if !rustc.is_empty() && Path::new(rustc).is_file() {
                    cmd.env("RUSTC", rustc);
                }
            }
        }
    }
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
    let stderr = line_reader::spawn(stderr, bus.clone(), LogSource::Build, Stream::Stderr);

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
    let _ = stderr.join();
    // Lets the background target sweep tell unchanged units from unused ones.
    crate::gc::target_usage::mark_used(&messages.used);
    Ok(CargoOutcome {
        success: !interrupted && status.is_some_and(|s| s.success()),
        errors: messages.errors,
        executables: messages.executables,
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
