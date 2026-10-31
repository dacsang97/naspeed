//! naspeed: transparent `RUSTC_WRAPPER` that stores registry/git crate
//! artifacts once per volume and clones them copy-on-write into every
//! `target/` dir, plus a machine-wide rustc slot pool, gc/dedup and cross
//! linker shims. Invoked as `ns`, it is also the project front-end:
//! `ns dev`, `ns test`, `ns release`, `ns image`, `ns ci`, `ns cache`.

pub mod ci;
pub mod cli;
pub mod commands;
pub mod config;
pub mod cross;
pub mod dev;
pub mod gc;
pub mod image;
pub mod jobserver;
pub mod project;
pub mod release;
pub mod remote;
pub mod setup;
pub mod store;
pub mod testcache;
pub mod wrapper;

/// Logging is off unless `NASPEED_LOG` is set. In wrapper mode stderr belongs
/// to rustc's JSON stream, so logs go to `NASPEED_LOG_FILE` when it is given.
pub fn init_logging() {
    let Ok(filter) = std::env::var("NASPEED_LOG") else {
        return;
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_ansi(false);
    let file = std::env::var_os("NASPEED_LOG_FILE").and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    let _ = match file {
        Some(file) => builder.with_writer(std::sync::Mutex::new(file)).try_init(),
        None => builder.with_writer(std::io::stderr).try_init(),
    };
}
