//! `ns check` end to end on the ns_project fixture: a clean workspace
//! passes, a formatting slip or a clippy warning fails the run (both are
//! reported in one go), and `--fix` repairs the formatting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

use common::{NASPEED, Sandbox};

fn project(name: &str) -> (Sandbox, PathBuf) {
    let sb = Sandbox::new(name);
    let dir = sb.root.join("proj");
    common::copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ns_project"),
        &dir,
    );
    (sb, dir)
}

fn ns_check(sb: &Sandbox, dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(NASPEED);
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if (k.starts_with("CARGO_") && k != "CARGO_HOME")
            || k.starts_with("NASPEED_")
            || k.starts_with("RUSTC_")
        {
            cmd.env_remove(k.as_ref());
        }
    }
    cmd.arg("check")
        .args(args)
        .current_dir(dir)
        .env("NASPEED_CONFIG_DIR", &sb.config)
        .env("NASPEED_STORE", &sb.store)
        .env("NASPEED_NO_AUTO_GC", "1")
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn clean_workspace_passes_and_slips_fail() {
    let (sb, dir) = project("check");
    let out = ns_check(&sb, &dir, &["--offline"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("ns: fmt ok"), "{}", stderr(&out));
    assert!(stderr(&out).contains("ns: clippy ok"), "{}", stderr(&out));

    // Nothing changed: formatting is skipped, from the store's record.
    let again = ns_check(&sb, &dir, &["--offline"]);
    assert!(again.status.success());
    assert!(
        stderr(&again).contains("0 packages checked"),
        "{}",
        stderr(&again)
    );

    // A formatting slip (before the test module, so clippy has nothing to add).
    let lib = dir.join("crates/greeter/src/lib.rs");
    let text = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(
        &lib,
        format!("pub fn   badly_spaced() -> bool {{ let x = 1; x == 1 }}\n{text}"),
    )
    .unwrap();
    let out = ns_check(&sb, &dir, &["--offline"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("ns: fmt FAILED"), "{err}");
    assert!(err.contains("ns check: failed: fmt"), "{err}");

    // --fix formats in place; what remains is clippy's verdict alone.
    let out = ns_check(&sb, &dir, &["--fix", "--offline"]);
    assert!(stderr(&out).contains("ns: fmt ok"), "{}", stderr(&out));
    let out = ns_check(&sb, &dir, &["--offline"]);
    assert!(stderr(&out).contains("ns: fmt ok"), "{}", stderr(&out));
}

#[test]
fn clippy_warnings_fail_the_check() {
    let (sb, dir) = project("checkclippy");
    let lib = dir.join("crates/greeter/src/lib.rs");
    let text = std::fs::read_to_string(&lib).unwrap();
    // `clippy::needless_return`, formatted so that only clippy objects.
    std::fs::write(
        &lib,
        format!("pub fn answer() -> u32 {{\n    return 42;\n}}\n\n{text}"),
    )
    .unwrap();
    let out = ns_check(&sb, &dir, &["--offline"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("ns: fmt ok"), "{err}");
    assert!(err.contains("ns: clippy FAILED"), "{err}");
    assert!(err.contains("needless_return"), "{err}");
}

#[test]
fn ns_fmt_reports_with_check_and_rewrites_without() {
    let (sb, dir) = project("nsfmt");
    let lib = dir.join("crates/greeter/src/lib.rs");
    let text = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("pub fn   spaced() {{}}\n{text}")).unwrap();
    let run = |args: &[&str]| {
        let mut cmd = Command::new(NASPEED);
        cmd.arg("fmt").args(args).current_dir(&dir);
        cmd.env("NASPEED_CONFIG_DIR", &sb.config).output().unwrap()
    };
    let out = run(&["--check"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("ns: fmt FAILED"), "{}", stderr(&out));
    assert!(
        std::fs::read_to_string(&lib)
            .unwrap()
            .contains("fn   spaced")
    );

    let out = run(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        std::fs::read_to_string(&lib)
            .unwrap()
            .contains("pub fn spaced() {}")
    );
    assert!(run(&["--check"]).status.success());
}
