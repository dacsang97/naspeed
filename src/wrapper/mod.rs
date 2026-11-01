//! Wrapper mode: `naspeed <rustc> <args…>` as invoked by cargo through
//! `build.rustc-wrapper`.

pub mod build_script_run;
pub mod cache_key;
pub mod cacheability;
pub mod execute;
pub mod extern_identity;
pub mod incremental_warm;
pub mod rustc_args;
pub mod toolchain_fingerprint;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitStatus};

use crate::config::Settings;
use crate::jobserver;

pub type Env = BTreeMap<String, String>;

/// The process environment. Windows variable names are case-insensitive
/// (`Path` is `PATH`), so they are upper-cased there to make lookups exact.
pub fn current_env() -> Env {
    std::env::vars_os()
        .map(|(k, v)| {
            let k = k.to_string_lossy();
            let k = if cfg!(windows) {
                k.to_uppercase()
            } else {
                k.into_owned()
            };
            (k, v.to_string_lossy().into_owned())
        })
        .collect()
}

pub fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + sig;
    }
    1
}

/// Run rustc untouched, inheriting stdio.
pub fn passthrough(program: &Path, args: &[OsString]) -> i32 {
    match Command::new(program).args(args).status() {
        Ok(status) => exit_code(status),
        Err(err) => {
            eprintln!("naspeed: failed to run {}: {err}", program.display());
            1
        }
    }
}

pub fn run(argv: &[OsString]) -> i32 {
    let settings = Settings::load();
    let program = Path::new(&argv[0]);
    let args = &argv[1..];
    if settings.disabled {
        return passthrough(program, args);
    }
    match execute::run(&settings, program, args) {
        Some(code) => code,
        None => {
            let _permit = jobserver::acquire(&settings);
            tracing::debug!("rustc start");
            let code = passthrough(program, args);
            tracing::debug!("rustc end");
            code
        }
    }
}
