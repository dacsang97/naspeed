//! `ns check`: format, lint and type check in one command. Runs
//! `cargo fmt --check`, then `cargo clippy` with warnings as errors over
//! every target of the whole workspace (clippy type-checks too), reports
//! each step and fails when any step fails. Both go through naspeed's cache.
//! `--fix` formats in place and applies clippy's machine-applicable fixes.
//! `ns fmt` is the formatting step alone.

use std::ffi::OsString;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args;

use crate::commands::stats::human_ms;

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Format in place and apply clippy's automatic fixes instead of failing.
    #[arg(long)]
    pub fix: bool,
    /// Skip the formatting step.
    #[arg(long)]
    pub no_fmt: bool,
    /// Extra arguments for `cargo clippy` (e.g. `-p api --features x`);
    /// lint flags go after `--` and replace the default `-D warnings`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cargo_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct FmtArgs {
    /// Report unformatted files instead of rewriting them.
    #[arg(long)]
    pub check: bool,
    /// Extra arguments for `cargo fmt` (e.g. `-p api`; rustfmt flags after `--`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cargo_args: Vec<OsString>,
}

fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// `cargo fmt` arguments: every package unless the caller picked some,
/// `--check` (a rustfmt flag) when not writing.
pub fn fmt_args(fix: bool, user: &[OsString]) -> Vec<OsString> {
    let split = user.iter().position(|a| a == "--");
    let (cargo_part, rustfmt_part) = match split {
        Some(i) => (&user[..i], &user[i + 1..]),
        None => (user, &user[user.len()..]),
    };
    let picks_packages = cargo_part.iter().any(|a| {
        let a = a.to_string_lossy();
        a == "-p" || a == "--package" || a.starts_with("--package=") || a == "--all"
    });
    let mut args: Vec<OsString> = vec!["fmt".into()];
    if !picks_packages {
        args.push("--all".into());
    }
    args.extend(cargo_part.iter().cloned());
    if !fix || !rustfmt_part.is_empty() {
        args.push("--".into());
    }
    if !fix {
        args.push("--check".into());
    }
    args.extend(rustfmt_part.iter().cloned());
    args
}

/// `ns fmt [--check]`.
pub fn run_fmt(args: FmtArgs) -> Result<u8> {
    Ok(if step("fmt", &fmt_args(!args.check, &args.cargo_args))? {
        0
    } else {
        1
    })
}

/// `cargo clippy` arguments: the whole workspace and every target unless
/// the caller picked packages or targets, and `-D warnings` unless the
/// caller passed lint flags after `--`.
pub fn clippy_args(fix: bool, user: &[OsString]) -> Vec<OsString> {
    let split = user.iter().position(|a| a == "--");
    let (cargo_part, lint_part) = match split {
        Some(i) => (&user[..i], Some(&user[i + 1..])),
        None => (user, None),
    };
    let has = |flags: &[&str]| {
        cargo_part.iter().any(|a| {
            let a = a.to_string_lossy();
            flags
                .iter()
                .any(|f| a == *f || a.starts_with(&format!("{f}=")))
        })
    };
    let mut args: Vec<OsString> = vec!["clippy".into()];
    if !has(&["-p", "--package", "--workspace", "--all"]) {
        args.push("--workspace".into());
    }
    if !has(&[
        "--lib",
        "--bins",
        "--bin",
        "--tests",
        "--test",
        "--examples",
        "--example",
        "--benches",
        "--bench",
        "--all-targets",
    ]) {
        args.push("--all-targets".into());
    }
    if fix {
        args.extend([
            "--fix".into(),
            "--allow-dirty".into(),
            "--allow-staged".into(),
        ]);
    }
    args.extend(cargo_part.iter().cloned());
    args.push("--".into());
    match lint_part {
        Some(lints) => args.extend(lints.iter().cloned()),
        None => args.extend(["-D".into(), "warnings".into()]),
    }
    args
}

/// Run one step with inherited stdio; returns whether it passed.
fn step(name: &str, args: &[OsString]) -> Result<bool> {
    let started = Instant::now();
    let status = cargo()
        .args(args)
        .status()
        .with_context(|| format!("run cargo {name}"))?;
    let took = human_ms(started.elapsed().as_millis() as u64);
    if status.success() {
        eprintln!("ns: {name} ok ({took})");
    } else {
        eprintln!("ns: {name} FAILED ({took})");
    }
    Ok(status.success())
}

pub fn run(args: CheckArgs) -> Result<u8> {
    let mut failed = Vec::new();
    if !args.no_fmt && !step("fmt", &fmt_args(args.fix, &[]))? {
        failed.push("fmt");
    }
    // Lint even when formatting failed: one run reports everything.
    if !step("clippy", &clippy_args(args.fix, &args.cargo_args))? {
        failed.push("clippy");
    }
    if failed.is_empty() {
        Ok(0)
    } else {
        eprintln!("ns check: failed: {}", failed.join(", "));
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_cover_the_whole_workspace_with_warnings_denied() {
        assert_eq!(
            s(&clippy_args(false, &[])),
            [
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ]
        );
        assert_eq!(s(&fmt_args(false, &[])), ["fmt", "--all", "--", "--check"]);
        assert_eq!(s(&fmt_args(true, &[])), ["fmt", "--all"]);
        assert_eq!(
            s(&fmt_args(
                true,
                &os(&["-p", "api", "--", "--edition", "2021"])
            )),
            ["fmt", "-p", "api", "--", "--edition", "2021"]
        );
    }

    #[test]
    fn caller_choices_win() {
        assert_eq!(
            s(&clippy_args(
                false,
                &os(&["-p", "api", "--lib", "--", "-W", "clippy::pedantic"])
            )),
            [
                "clippy",
                "-p",
                "api",
                "--lib",
                "--",
                "-W",
                "clippy::pedantic"
            ]
        );
        assert_eq!(
            s(&clippy_args(true, &os(&["--package=api"]))),
            [
                "clippy",
                "--all-targets",
                "--fix",
                "--allow-dirty",
                "--allow-staged",
                "--package=api",
                "--",
                "-D",
                "warnings"
            ]
        );
    }
}
