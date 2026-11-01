//! Rebuild rustc's incremental state for a workspace crate that was just
//! restored from the store.
//!
//! A hit puts the crate's outputs in place but leaves `-C incremental`'s
//! session dir empty, so the first edit afterwards recompiles that crate
//! and everything depending on it from scratch (a deep edit in a fresh
//! worktree took 29 s instead of 11 s). After a hit the wrapper starts
//! `naspeed __warm-incremental <rustc> <args>` detached: it waits for a
//! machine slot, runs the same compile at low priority with `--out-dir`
//! pointed at a throwaway dir, and so leaves a finalized incremental session
//! behind. Cargo never sees it; the restored outputs stay untouched.
//!
//! rustc locks its session dirs, so a foreground compile started while the
//! warm-up runs simply does not reuse the unfinished session.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::Settings;
use crate::jobserver;

pub const SUBCOMMAND: &str = "__warm-incremental";

/// `args` with every `--out-dir` pointed at `out_dir`.
pub fn redirect_out_dir(args: &[OsString], out_dir: &Path) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--out-dir" {
            out.push(arg.clone());
            it.next();
            out.push(out_dir.as_os_str().to_owned());
        } else if arg.to_str().is_some_and(|a| a.starts_with("--out-dir=")) {
            let mut v = OsString::from("--out-dir=");
            v.push(out_dir);
            out.push(v);
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Start the warm-up and return at once (the wrapper must not wait).
/// `args` are the expanded rustc arguments the wrapper would have run.
pub fn spawn(program: &Path, args: &[OsString]) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg(SUBCOMMAND)
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Cargo's jobserver fds are not ours to use once the wrapper exits.
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS");
    if let Err(e) = cmd.spawn() {
        tracing::debug!("incremental warm-up not started: {e}");
    }
}

/// Body of `naspeed __warm-incremental <rustc> <args…>`.
pub fn run(program: &Path, args: &[OsString]) -> i32 {
    // Low priority: never slow the build the user is waiting for.
    crate::platform::lower_priority();
    let settings = Settings::load();
    let _slot = jobserver::acquire(&settings);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out_dir = std::env::temp_dir().join(format!("naspeed-warm-{}-{nanos}", std::process::id()));
    if std::fs::create_dir_all(&out_dir).is_err() {
        return 1;
    }
    let status = Command::new(program)
        .args(redirect_out_dir(args, &out_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&out_dir);
    match status {
        Ok(s) if s.success() => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_dir_is_redirected_in_both_spellings() {
        let args: Vec<OsString> = [
            "--crate-name",
            "x",
            "--out-dir",
            "/t/debug/deps",
            "-C",
            "incremental=/t/debug/incremental",
            "--out-dir=/t/other",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let got = redirect_out_dir(&args, Path::new("/tmp/w"));
        let got: Vec<&str> = got.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            got,
            [
                "--crate-name",
                "x",
                "--out-dir",
                "/tmp/w",
                "-C",
                "incremental=/t/debug/incremental",
                "--out-dir=/tmp/w"
            ]
        );
    }
}
