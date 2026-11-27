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
    "run",
    "help",
    "__link",
    "__sweep-target",
    "__build-script",
    "zig",
    "dev",
    "release",
    "image",
    "ci",
    "cache",
    "test",
    "check",
    "fmt",
    "stats",
    "update",
    "__test-run",
    "__warm-incremental",
];

/// Name of the front-end symlink `setup` installs next to the binary.
pub const NS_ALIAS: &str = "ns";

/// True when the process was started through the `ns` symlink. `ns` always
/// parses its arguments as a CLI, never as a `RUSTC_WRAPPER` call, so a typo
/// like `ns rustc` errors instead of compiling something.
pub fn invoked_as_ns(argv0: &std::ffi::OsStr) -> bool {
    std::path::Path::new(argv0).file_stem().is_some_and(|stem| {
        // File names are case-insensitive on Windows (`NS.EXE`).
        stem == NS_ALIAS || (cfg!(windows) && stem.eq_ignore_ascii_case(NS_ALIAS))
    })
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
    about = "Rust build cache and project tooling, including Cargo/Go development loops"
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
    about = "Project front-end: Cargo build tooling and Cargo/Go development loops (alias of naspeed)"
)]
pub struct NsCli {
    #[command(subcommand)]
    command: NsCommand,
}

#[derive(Debug, Subcommand)]
enum NsCommand {
    /// Build a Cargo project, optionally publishing an app patch with --lazy-ir.
    Build(ForwardCargoArgs),
    /// Run a Cargo target, optionally generating Rust IR on first call with --lazy-ir.
    Run(ForwardCargoArgs),
    /// Rebuild an app patch without rustc; repatches only selector bodies.
    Patch(commands::fast_patch::PatchArgs),
    #[command(flatten)]
    Shared(SharedCommand),
}

#[derive(Debug, Args)]
struct ForwardCargoArgs {
    /// Arguments passed directly to Cargo.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Builds cargo cannot do alone: universal2-apple-darwin and pinned glibc (`<triple>.2.17`).
    Build(BuildArgs),
    #[command(flatten)]
    Shared(SharedCommand),
}

#[derive(Debug, Subcommand)]
pub enum SharedCommand {
    /// Make every cargo invocation go through naspeed (edits ~/.cargo/config.toml after a backup).
    Setup(SetupArgs),
    /// Undo `setup`, restoring each changed config key.
    Uninstall(UninstallArgs),
    /// Store size, logical vs physical usage, hit rate, rustc slots.
    Status(StatusArgs),
    /// Shrink the store (LRU), sweep stale artifacts in target dirs, dedup files.
    Gc(GcArgs),
    /// Remove artifacts naspeed placed into a target dir.
    Purge(PurgeArgs),
    /// Watch a Cargo or Go project: build, test, and selectively restart binaries on change.
    Dev(crate::dev::DevArgs),
    /// Build the release target matrix, archive, checksum, sign and publish.
    Release(crate::release::ReleaseArgs),
    /// Build and push a multi-arch OCI image from release binaries (no Docker daemon).
    Image(crate::image::ImageArgs),
    /// Check for a new release and update through the channel naspeed was
    /// installed with (Homebrew tap or cargo install).
    Update(UpdateArgs),
    /// GitHub Actions workflow with the naspeed store cached.
    #[command(subcommand)]
    Ci(crate::ci::CiCommand),
    /// `cargo test` that replays unchanged passing test binaries (test result cache).
    Test(crate::testcache::TestArgs),
    /// Format check, then clippy with warnings denied over the whole workspace.
    Check(crate::commands::check::CheckArgs),
    /// `cargo fmt` over the whole workspace (`--check` to only report).
    Fmt(crate::commands::check::FmtArgs),
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
    /// Opportunistic sweep+dedup of one profile dir (spawned by the wrapper).
    #[command(name = "__sweep-target", hide = true)]
    SweepTarget(SweepTargetArgs),
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only report whether an update exists and the command that would run.
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Store root; must be on the same volume as your projects. Default: `~/.naspeed`, or `<volume of cwd>/.naspeed` when run on another volume.
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
    /// Also evict store entries (and, with --targets, target-dir units) unused for this many days (default from config, 14).
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
pub struct SweepTargetArgs {
    /// Cargo profile dir (e.g. target/debug) to sweep and dedup.
    pub profile_dir: PathBuf,
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
    let as_ns = args.first().is_some_and(|a| invoked_as_ns(a));
    let command = if as_ns && !legacy_ns_cross_build(&args) {
        // Clap consumes a leading `--` before the first forwarded argument.
        // Cargo needs it to separate its own flags from program arguments.
        let separator = args.get(2).is_some_and(|a| a == "--");
        match NsCli::parse_from(args).command {
            NsCommand::Build(a) => return forward_cargo("build", a, separator),
            NsCommand::Run(a) => return forward_cargo("run", a, separator),
            NsCommand::Patch(a) => return commands::fast_patch::run(&a).map_err(Into::into),
            NsCommand::Shared(command) => command,
        }
    } else {
        match Cli::parse_from(args).command {
            Command::Build(a) => {
                reject_go_only("ns build")?;
                return commands::build::run(a);
            }
            Command::Shared(command) => command,
        }
    };
    match command {
        SharedCommand::Setup(a) => commands::setup::run(a),
        SharedCommand::Uninstall(a) => commands::uninstall::run(a),
        SharedCommand::Status(a) => commands::status::run(a),
        SharedCommand::Gc(a) => commands::gc::run(a),
        SharedCommand::Purge(a) => commands::purge::run(a),
        SharedCommand::Dev(a) => crate::dev::run(a),
        SharedCommand::Update(a) => commands::update::run(a),
        SharedCommand::Release(a) => crate::release::run(a),
        SharedCommand::Image(a) => crate::image::run(a),
        SharedCommand::Ci(c) => {
            reject_go_only("ns ci")?;
            crate::ci::run(c)
        }
        SharedCommand::Cache(c) => commands::cache_transfer::run(c),
        SharedCommand::Test(a) => {
            reject_go_only("ns test")?;
            crate::testcache::run(a)
        }
        SharedCommand::Check(a) => {
            reject_go_only("ns check")?;
            commands::check::run(a)
        }
        SharedCommand::Fmt(a) => {
            reject_go_only("ns fmt")?;
            commands::check::run_fmt(a)
        }
        SharedCommand::Stats(a) => commands::stats::run(a),
        SharedCommand::WarmIncremental(a) => {
            Ok(crate::wrapper::incremental_warm::run(&a.program, &a.args).clamp(0, 255) as u8)
        }
        SharedCommand::TestRun(a) => {
            Ok(crate::testcache::run_binary(&a.binary, &a.args).clamp(0, 255) as u8)
        }
        SharedCommand::Link(a) => crate::cross::linker_shim::run(a),
        SharedCommand::BuildScript(a) => {
            Ok(
                crate::wrapper::build_script_run::run(&a.real, &a.binary_hash, &a.args)
                    .clamp(0, 255) as u8,
            )
        }
        SharedCommand::Zig(zig) => {
            zig.execute()?;
            Ok(0)
        }
        SharedCommand::SweepTarget(a) => crate::commands::sweep_target::run(&a.profile_dir),
    }
}

// Before native `ns build`, the alias also exposed naspeed's special targets.
fn legacy_ns_cross_build(args: &[OsString]) -> bool {
    if !args.get(1).is_some_and(|a| a == "build") {
        return false;
    }
    let target = match args.get(2).and_then(|a| a.to_str()) {
        Some("--target") => args.get(3).and_then(|a| a.to_str()),
        Some(arg) => arg.strip_prefix("--target="),
        None => None,
    };
    target.is_some_and(|t| {
        t == "universal2-apple-darwin" || commands::build::split_glibc(t).is_some()
    })
}

fn forward_cargo(verb: &str, args: ForwardCargoArgs, separator: bool) -> Result<u8> {
    use anyhow::Context;

    let mut cmd = crate::dev::cargo_runner::cargo_command();
    cmd.arg(verb);
    let mut forwarded = args.args.into_iter().peekable();
    if !separator
        && matches!(verb, "build" | "run")
        && forwarded.next_if(|a| a == "--lazy-ir").is_some()
    {
        anyhow::ensure!(
            cfg!(all(target_os = "macos", target_arch = "aarch64")),
            "ns {verb} --lazy-ir requires Apple Silicon macOS"
        );
        let driver = lazy_driver()?;
        if verb == "run"
            && forwarded.peek().is_some_and(|arg| {
                std::path::Path::new(arg)
                    .extension()
                    .is_some_and(|ext| ext == "rs")
            })
        {
            let mut driver = std::process::Command::new(driver);
            driver.arg("--naspeed-demand-ir").args(forwarded);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                return Err(driver.exec()).context("run the naspeed demand IR driver");
            }
            #[cfg(not(unix))]
            unreachable!();
        }
        if verb == "build" {
            anyhow::ensure!(
                std::env::var("NASPEED_LAZY_APP_PATCH").is_ok_and(|value| !value.is_empty()),
                "ns build --lazy-ir requires nonempty NASPEED_LAZY_APP_PATCH"
            );
        }
        let driver = driver.with_file_name("naspeed-rustc-demand");
        anyhow::ensure!(
            driver.is_file(),
            "run make lazy-driver to install naspeed-rustc-demand"
        );
        cmd.env("RUSTC_WORKSPACE_WRAPPER", driver);
    } else if verb == "run" && !separator && forwarded.next_if(|a| a == "--lazy").is_some() {
        anyhow::ensure!(
            cfg!(all(target_os = "macos", target_arch = "aarch64")),
            "ns run --lazy requires Apple Silicon macOS"
        );
        cmd.env("RUSTC_WORKSPACE_WRAPPER", lazy_driver()?);
    } else if verb == "run" && !separator && forwarded.next_if(|a| a == "--split-link").is_some() {
        anyhow::ensure!(
            cfg!(target_os = "macos"),
            "ns run --split-link is supported on macOS only"
        );
        let driver = std::env::var_os("NASPEED_SPLIT_LINK_DRIVER")
            .context("set NASPEED_SPLIT_LINK_DRIVER to the naspeed-rustc-split path from make function-driver")?;
        let driver = PathBuf::from(driver);
        anyhow::ensure!(
            driver.is_absolute()
                && driver
                    .file_name()
                    .is_some_and(|name| name == "naspeed-rustc-split")
                && driver.is_file(),
            "NASPEED_SPLIT_LINK_DRIVER must be an existing absolute path to naspeed-rustc-split"
        );
        cmd.env("RUSTC_WORKSPACE_WRAPPER", driver);
    }
    if separator {
        cmd.arg("--");
    }
    cmd.args(forwarded);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec()).with_context(|| format!("run cargo {verb}"))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().with_context(|| format!("run cargo {verb}"))?;
        Ok(crate::wrapper::exit_code(status).clamp(0, 255) as u8)
    }
}

fn lazy_driver() -> Result<PathBuf> {
    use anyhow::Context;

    let driver = std::env::var_os("NASPEED_LAZY_RUN_DRIVER").context(
        "set NASPEED_LAZY_RUN_DRIVER to the naspeed-rustc-lazy path from make lazy-driver",
    )?;
    let driver = PathBuf::from(driver);
    anyhow::ensure!(
        driver.is_absolute()
            && driver
                .file_name()
                .is_some_and(|name| name == "naspeed-rustc-lazy")
            && driver.is_file(),
        "NASPEED_LAZY_RUN_DRIVER must be an existing absolute path to naspeed-rustc-lazy"
    );
    Ok(driver)
}

fn reject_go_only(command: &str) -> Result<()> {
    let current = std::env::current_dir()?;
    if crate::dev::toolchain::is_go_only_dir(&current) {
        anyhow::bail!(
            "Go projects are supported by `ns dev` only for now; {command} requires a Cargo project"
        );
    }
    Ok(())
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
