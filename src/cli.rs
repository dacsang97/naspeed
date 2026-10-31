//! Command-line interface for naspeed's own subcommands.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::commands;

const SUBCOMMANDS: &[&str] = &[
    "setup",
    "uninstall",
    "status",
    "gc",
    "build",
    "purge",
    "help",
    "__link",
    "__build-script",
    "zig",
    "dev",
    "release",
    "image",
    "ci",
    "cache",
    "test",
    "stats",
    "__test-run",
    "__warm-incremental",
];

/// Name of the front-end symlink `setup` installs next to the binary.
pub const NS_ALIAS: &str = "ns";

/// True when the process was started through the `ns` symlink. `ns` always
/// parses its arguments as a CLI, never as a `RUSTC_WRAPPER` call, so a typo
/// like `ns rustc` errors instead of compiling something.
pub fn invoked_as_ns(argv0: &std::ffi::OsStr) -> bool {
    std::path::Path::new(argv0)
        .file_stem()
        .is_some_and(|stem| stem == NS_ALIAS)
}

/// Wrapper mode is anything cargo passes as `argv[1]` (bare `rustc`, a path
/// to rustc, or a `RUSTC_WORKSPACE_WRAPPER` like clippy-driver). Our own
/// invocations are one of the subcommands or a flag.
pub fn is_naspeed_invocation(first: &OsString) -> bool {
    match first.to_str() {
        Some(s) => s.starts_with('-') || SUBCOMMANDS.contains(&s),
        None => false,
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "naspeed",
    version,
    about = "Transparent Rust build cache: registry crates stored once per volume and cloned copy-on-write into every target/"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Same commands, shown under the `ns` name.
#[derive(Debug, Parser)]
#[command(
    name = "ns",
    version,
    about = "Rust project front-end over cargo: dev loop, release, OCI images, CI cache (alias of naspeed)"
)]
pub struct NsCli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Make every cargo invocation go through naspeed (edits ~/.cargo/config.toml after a backup).
    Setup(SetupArgs),
    /// Undo `setup`, restoring each changed config key.
    Uninstall(UninstallArgs),
    /// Store size, logical vs physical usage, hit rate, rustc slots.
    Status(StatusArgs),
    /// Shrink the store (LRU), sweep stale artifacts in target dirs, dedup files.
    Gc(GcArgs),
    /// Builds cargo cannot do alone: universal2-apple-darwin and pinned glibc (`<triple>.2.17`).
    Build(BuildArgs),
    /// Remove artifacts naspeed placed into a target dir.
    Purge(PurgeArgs),
    /// Watch the workspace: check → build → restart the binary on every change.
    Dev(crate::dev::DevArgs),
    /// Build the release target matrix, archive, checksum, sign and publish.
    Release(crate::release::ReleaseArgs),
    /// Build and push a multi-arch OCI image from release binaries (no Docker daemon).
    Image(crate::image::ImageArgs),
    /// GitHub Actions workflow with the naspeed store cached.
    #[command(subcommand)]
    Ci(crate::ci::CiCommand),
    /// `cargo test` that replays unchanged passing test binaries (test result cache).
    Test(crate::testcache::TestArgs),
    /// Time saved by the build cache, the test cache and peer fetches.
    Stats(StatsArgs),
    /// Export/import store entries through a directory (CI cache transfer).
    #[command(subcommand)]
    Cache(crate::commands::cache_transfer::CacheCommand),
    /// Linker / C compiler shim for cross targets (used by generated scripts).
    #[command(name = "__link", hide = true)]
    Link(LinkArgs),
    /// `zig cc|c++|ar|ranlib|lib|dlltool` wrapper called back by the zig shims.
    #[command(subcommand, hide = true)]
    Zig(cargo_zigbuild::Zig),
    /// Background rebuild of a restored workspace crate's incremental state.
    #[command(name = "__warm-incremental", hide = true)]
    WarmIncremental(WarmIncrementalArgs),
    /// Test-binary runner installed by `ns test` (called by cargo).
    #[command(name = "__test-run", hide = true)]
    TestRun(crate::testcache::TestRunArgs),
    /// Cached run of a registry crate's build script (called by its launcher).
    #[command(name = "__build-script", hide = true)]
    BuildScript(BuildScriptArgs),
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Store root; must be on the same volume as your projects. Default: `<volume of cwd>/.naspeed`.
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// `build.jobs` written to ~/.cargo/config.toml.
    #[arg(long, default_value_t = 4)]
    pub jobs: u32,
    /// Machine-wide rustc slots (default: logical cores).
    #[arg(long)]
    pub slots: Option<usize>,
    /// Remove `build.incremental = false` (only when the disk budget allows it).
    #[arg(long)]
    pub enable_incremental: bool,
    /// Debug info for dependencies in the dev profile, written as
    /// `[profile.dev.package."*"] debug = <value>` (e.g. `line-tables-only`,
    /// `false`). Backtraces keep file:line; debuggers lose dependency locals.
    #[arg(long)]
    pub deps_debug: Option<String>,
    /// Also install cross linker shims for every non-host target in the matrix.
    #[arg(long)]
    pub cross: bool,
    /// Accept the Microsoft CRT/SDK license so the msvc SDK can be downloaded.
    #[arg(long)]
    pub accept_msvc_license: bool,
    /// Run `rustup target add` for missing cross targets.
    #[arg(long)]
    pub install_targets: bool,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Also delete the store.
    #[arg(long)]
    pub purge: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Also verify the wrapper is active by building a tiny fixture with `cargo build -v`.
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Window for the counters, in days.
    #[arg(long, default_value_t = 7)]
    pub days: i64,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Remove stale `-<hash>` variants and old incremental sessions in registered target dirs.
    #[arg(long)]
    pub targets: bool,
    /// Replace byte-identical files (>= 64 KiB) across registered target dirs with clones.
    #[arg(long)]
    pub dedup: bool,
    /// Store cap in GB for the LRU pass (default from config, 20).
    #[arg(long)]
    pub max_size_gb: Option<f64>,
    /// Report what would be freed without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Leave target dirs under this path alone (repeatable), e.g. a project in active development.
    #[arg(long)]
    pub exclude: Vec<PathBuf>,
    /// Also evict store entries unused for this many days (default from config, 14).
    #[arg(long)]
    pub max_age_days: Option<f64>,
    /// Daily background trim started by the wrapper: store LRU only, quiet.
    #[arg(long, hide = true)]
    pub auto: bool,
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// `universal2-apple-darwin` or `<triple>.<glibc>` such as `x86_64-unknown-linux-gnu.2.17`.
    #[arg(long)]
    pub target: String,
    /// Extra arguments passed to `cargo build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cargo_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {
    /// Target dir (the one holding debug/, release/, …).
    #[arg(long)]
    pub target: PathBuf,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct BuildScriptArgs {
    /// The real build-script binary (`<launcher>.naspeed-real`).
    pub real: PathBuf,
    /// Content hash of the real binary (part of the key).
    pub binary_hash: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct WarmIncrementalArgs {
    pub program: PathBuf,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct LinkArgs {
    pub triple: String,
    /// `cc`, `c++`, `ar` or `ld`.
    pub tool: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

pub fn run(args: Vec<OsString>) -> Result<u8> {
    let command = match args.first() {
        Some(argv0) if invoked_as_ns(argv0) => NsCli::parse_from(args).command,
        _ => Cli::parse_from(args).command,
    };
    match command {
        Command::Setup(a) => commands::setup::run(a),
        Command::Uninstall(a) => commands::uninstall::run(a),
        Command::Status(a) => commands::status::run(a),
        Command::Gc(a) => commands::gc::run(a),
        Command::Build(a) => commands::build::run(a),
        Command::Purge(a) => commands::purge::run(a),
        Command::Dev(a) => crate::dev::run(a),
        Command::Release(a) => crate::release::run(a),
        Command::Image(a) => crate::image::run(a),
        Command::Ci(c) => crate::ci::run(c),
        Command::Cache(c) => commands::cache_transfer::run(c),
        Command::Test(a) => crate::testcache::run(a),
        Command::Stats(a) => commands::stats::run(a),
        Command::WarmIncremental(a) => {
            Ok(crate::wrapper::incremental_warm::run(&a.program, &a.args).clamp(0, 255) as u8)
        }
        Command::TestRun(a) => {
            Ok(crate::testcache::run_binary(&a.binary, &a.args).clamp(0, 255) as u8)
        }
        Command::Link(a) => crate::cross::linker_shim::run(a),
        Command::BuildScript(a) => {
            Ok(
                crate::wrapper::build_script_run::run(&a.real, &a.binary_hash, &a.args)
                    .clamp(0, 255) as u8,
            )
        }
        Command::Zig(zig) => {
            zig.execute()?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_mode_detection() {
        for s in ["rustc", "/home/dev/.cargo/bin/rustc", "/x/clippy-driver"] {
            assert!(!is_naspeed_invocation(&OsString::from(s)), "{s}");
        }
        for s in ["setup", "status", "--help", "-V", "__link"] {
            assert!(is_naspeed_invocation(&OsString::from(s)), "{s}");
        }
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        NsCli::command().debug_assert();
    }

    #[test]
    fn ns_alias_detection() {
        use std::ffi::OsStr;
        assert!(invoked_as_ns(OsStr::new("ns")));
        assert!(invoked_as_ns(OsStr::new("/home/dev/.cargo/bin/ns")));
        assert!(invoked_as_ns(OsStr::new("ns.exe")));
        assert!(!invoked_as_ns(OsStr::new("/home/dev/.cargo/bin/naspeed")));
        assert!(!invoked_as_ns(OsStr::new("nsx")));
    }
}
