//! Builds the test-input tracer (`trace/naspeed_trace.c`) as a shared library
//! for the target and hands its path to the crate as `NASPEED_TRACE_LIB`
//! (embedded with `include_bytes!`). Linux builds always produce a glibc
//! library, since that is what test binaries link against, even when naspeed
//! itself is a static musl binary (then through `zig cc`). Without a usable C
//! compiler the library is empty and `ns test` falls back to declared inputs.

use std::path::{Path, PathBuf};
use std::process::Command;

fn compile(cmd: &mut Command, out: &Path) -> bool {
    match cmd.status() {
        Ok(s) if s.success() && out.is_file() => true,
        Ok(s) => {
            println!(
                "cargo:warning=naspeed test tracer not built ({s}); ns test uses declared inputs only"
            );
            false
        }
        Err(e) => {
            println!(
                "cargo:warning=naspeed test tracer not built ({e}); ns test uses declared inputs only"
            );
            false
        }
    }
}

fn main() {
    let src = Path::new("trace/naspeed_trace.c");
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-env-changed=NASPEED_TRACE_CC");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let lib = out_dir.join("naspeed_trace.lib");
    let _ = std::fs::remove_file(&lib);

    let override_cc = std::env::var("NASPEED_TRACE_CC").ok();
    let built = match os.as_str() {
        "macos" => {
            let cc = override_cc.unwrap_or_else(|| "cc".into());
            let arch = if arch == "aarch64" {
                "arm64"
            } else {
                arch.as_str()
            };
            compile(
                Command::new(cc)
                    .args([
                        "-dynamiclib",
                        "-O1",
                        "-arch",
                        arch,
                        "-mmacosx-version-min=11.0",
                    ])
                    .arg("-o")
                    .arg(&lib)
                    .arg(src),
                &lib,
            )
        }
        "linux" => {
            let triple = format!("{arch}-linux-gnu");
            let mut cmd = match override_cc {
                Some(cc) => Command::new(cc),
                // A native glibc build can use the system compiler.
                None if env == "gnu" && host == target => Command::new("cc"),
                // musl or cross: a glibc library from zig, if it is there.
                None => {
                    let mut c = Command::new("zig");
                    c.args(["cc", "-target", &format!("{triple}.2.17")]);
                    c
                }
            };
            cmd.args(["-shared", "-fPIC", "-O1", "-o"])
                .arg(&lib)
                .arg(src)
                .arg("-ldl");
            compile(&mut cmd, &lib)
        }
        _ => false,
    };
    if !built {
        std::fs::write(&lib, b"").expect("write empty tracer");
    }
    println!("cargo:rustc-env=NASPEED_TRACE_LIB={}", lib.display());
}
