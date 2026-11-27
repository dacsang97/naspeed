#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::process::Command;

#[test]
fn exec_runs_in_the_foreground_with_the_real_wrapper_and_intact_arguments() {
    let binary = env!("CARGO_BIN_EXE_naspeed");
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("ns");
    symlink(binary, &alias).unwrap();

    let output = Command::new(alias)
        .current_dir(temp.path())
        .args([
            "dev",
            "--no-tui",
            "--exec",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s\n%s\n' \"$RUSTC_WRAPPER\" \"$1\"; printf 'err\n' >&2; exit 42",
            "fixture",
            "--argument with spaces",
        ])
        .env("RUSTC_WRAPPER", "old-wrapper")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(42));
    let expected = format!(
        "{}\n--argument with spaces\n",
        std::fs::canonicalize(binary).unwrap().display()
    );
    assert_eq!(output.stdout, expected.as_bytes());
    assert_eq!(output.stderr, b"err\n");
}

#[test]
fn foreground_dev_rejects_missing_command_or_managed_mode_flags() {
    let binary = env!("CARGO_BIN_EXE_naspeed");
    for args in [
        vec!["dev", "--no-tui", "--exec"],
        vec!["dev", "--exec", "--bin", "app", "--", "/bin/true"],
        vec!["dev", "--exec", "--test", "--", "/bin/true"],
    ] {
        let output = Command::new(binary).args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
    }
}
