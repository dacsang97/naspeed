//! `ns test`: `cargo test` with a test result cache.
//!
//! cargo builds as usual; naspeed is installed as the target runner
//! (`--config target.<triple>.runner`), so cargo starts every test binary as
//! `naspeed __test-run <binary> <args…>` with the package's env and cwd. The
//! runner keys the run (see `test_key`) and either replays the stored output
//! of an identical passing run or runs the binary and stores a pass.
//! Doctests run under rustdoc, not the runner, and always run.

pub mod test_key;
pub mod test_plan;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::project::Project;
use crate::store::usage_log::Record;
use crate::store::{Store, now_secs, volume_root};
use crate::wrapper::execute::run_teed;
use crate::wrapper::{current_env, exit_code};
use test_key::RunContext;
use test_plan::{PLAN_ENV, TestPlan};

pub const TEST_RESULT_VERSION: u32 = 1;

#[derive(Debug, Args)]
pub struct TestArgs {
    /// Run every test binary even when an identical run already passed.
    #[arg(long)]
    pub no_cache: bool,
    /// Arguments for `cargo test` (put test-binary arguments after `--`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cargo_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct TestRunArgs {
    pub binary: PathBuf,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

/// A stored passing run, replayed verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestResult {
    pub version: u32,
    pub key: String,
    pub binary: String,
    pub stdout: String,
    pub stderr: String,
    pub run_ms: u64,
    pub created: u64,
}

/// `--target` values among the cargo args (before `--`).
fn targets(args: &[OsString]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter().map(|a| a.to_string_lossy());
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "--target" {
            out.extend(it.next().map(|t| t.into_owned()));
        } else if let Some(t) = a.strip_prefix("--target=") {
            out.push(t.to_owned());
        }
    }
    out
}

/// `target.<triple>.runner=["<naspeed>", "__test-run"]` (JSON strings are
/// valid TOML strings, so paths with spaces or quotes survive).
fn runner_config(triple: &str, exe: &Path) -> Result<String> {
    Ok(format!(
        "target.{triple}.runner=[{}, \"__test-run\"]",
        serde_json::to_string(&exe.to_string_lossy())?
    ))
}

/// `cargo test` with naspeed as the runner when the cache applies, up to
/// (not including) the caller's `cargo_args`; the plan file must outlive the
/// returned command.
pub fn cargo_test_command(
    project: &Project,
    no_cache: bool,
    cargo_args: &[OsString],
) -> Result<(Command, Option<PlanFile>)> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    if no_cache || !project.config.test.cache {
        cmd.arg("test");
        return Ok((cmd, None));
    }
    let settings = Settings::load();
    let Some(store_root) = volume_root::existing_ancestor(&project.graph.target_dir)
        .and_then(|dir| volume_root::store_for(dir, &settings.stores))
    else {
        tracing::debug!("no store on the target dir's volume: test cache off");
        cmd.arg("test");
        return Ok((cmd, None));
    };
    let plan = TestPlan::build(project, &store_root);
    let plan_file = PlanFile(plan.write()?);
    let exe = std::env::current_exe().context("locate naspeed")?;
    let mut triples = vec![crate::cross::host_triple()?];
    triples.extend(targets(cargo_args));
    triples.sort();
    triples.dedup();
    // A runner the project already uses (wasm, qemu, a DB wrapper) must
    // keep running its tests; those triples run uncached.
    let cargo_config = cargo_config2::Config::load_with_cwd(&project.graph.root).ok();
    for t in &triples {
        let own_runner = cargo_config
            .as_ref()
            .is_some_and(|c| c.runner(t.as_str()).ok().flatten().is_some());
        if own_runner {
            eprintln!("naspeed: {t} has its own target runner; its tests run uncached");
        } else {
            cmd.arg("--config").arg(runner_config(t, &exe)?);
        }
    }
    cmd.arg("test").env(PLAN_ENV, &plan_file.0);
    Ok((cmd, Some(plan_file)))
}

/// Removes the plan file when the run is over.
pub struct PlanFile(pub PathBuf);

impl Drop for PlanFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run(args: TestArgs) -> Result<u8> {
    let project = Project::load_cwd()?;
    let (mut cmd, plan) = cargo_test_command(&project, args.no_cache, &args.cargo_args)?;
    let status = cmd
        .args(&args.cargo_args)
        .status()
        .context("run cargo test")?;
    drop(plan);
    Ok(exit_code(status).clamp(0, 255) as u8)
}

fn passthrough(binary: &Path, args: &[OsString]) -> i32 {
    crate::wrapper::passthrough(binary, args)
}

/// `naspeed __test-run <binary> <args…>`, started by cargo.
pub fn run_binary(binary: &Path, args: &[OsString]) -> i32 {
    match try_cached_run(binary, args) {
        Some(code) => code,
        None => passthrough(binary, args),
    }
}

fn try_cached_run(binary: &Path, args: &[OsString]) -> Option<i32> {
    let plan = TestPlan::read(Path::new(&std::env::var_os(PLAN_ENV)?)).ok()?;
    let env = current_env();
    let manifest_dir = PathBuf::from(env.get("CARGO_MANIFEST_DIR")?);
    let package = plan.packages.get(&manifest_dir)?;
    let str_args: Vec<String> = args
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect::<Option<_>>()?;
    if !test_key::cacheable_args(&str_args) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let key = test_key::key(
        &plan,
        package,
        &RunContext {
            binary,
            args: &str_args,
            env: &env,
            cwd: &cwd,
            manifest_dir: &manifest_dir,
        },
    )
    .inspect_err(|e| tracing::warn!("test key: {e:#}"))
    .ok()?;
    let settings = Settings::load();
    let store = Store::open(&plan.store, settings.cow_mode).ok()?;
    let name = binary
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let path = store.test_result_path(&key);
    if let Some(result) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<TestResult>(&b).ok())
        .filter(|r| r.version == TEST_RESULT_VERSION && r.key == key)
    {
        let _ = std::io::stdout().write_all(result.stdout.as_bytes());
        let _ = std::io::stdout().flush();
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(result.stderr.as_bytes());
        let _ = writeln!(
            err,
            "naspeed: (cached) {name}: unchanged since a passing run ({}), not re-run",
            crate::commands::stats::human_ms(result.run_ms)
        );
        // mtime is the result's last use for gc.
        let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
        // gc renames the log away under the exclusive lock.
        let _lock = store.shared_lock();
        store.append_usage(&Record::TestHit {
            key,
            saved_ms: result.run_ms,
        });
        return Some(0);
    }

    let started = Instant::now();
    let output = run_teed(binary, args)
        .inspect_err(|e| eprintln!("naspeed: failed to run {}: {e:#}", binary.display()))
        .ok()?;
    let run_ms = started.elapsed().as_millis() as u64;
    let code = exit_code(output.status);
    if code == 0
        && let (Some(stdout), Some(stderr)) = (output.stdout, output.stderr)
    {
        let result = TestResult {
            version: TEST_RESULT_VERSION,
            key: key.clone(),
            binary: name,
            stdout,
            stderr,
            run_ms,
            created: now_secs(),
        };
        match serde_json::to_vec(&result)
            .map_err(anyhow::Error::from)
            .and_then(|b| store.write_file(&path, &b))
        {
            Ok(()) => {
                let _lock = store.shared_lock();
                store.append_usage(&Record::TestRun { key, run_ms })
            }
            Err(e) => tracing::warn!("test result not stored: {e:#}"),
        }
    }
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_targets_before_the_separator() {
        let args: Vec<OsString> = [
            "-p",
            "x",
            "--target",
            "a",
            "--target=b",
            "--",
            "--target",
            "c",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(targets(&args), vec!["a", "b"]);
    }

    #[test]
    fn runner_value_is_a_toml_array() {
        let v = runner_config("x86_64-unknown-linux-gnu", Path::new("/a b/naspeed")).unwrap();
        assert_eq!(
            v,
            r#"target.x86_64-unknown-linux-gnu.runner=["/a b/naspeed", "__test-run"]"#
        );
        let parsed: toml::Table = v.replacen('=', " = ", 1).parse().unwrap();
        assert!(parsed["target"]["x86_64-unknown-linux-gnu"]["runner"].is_array());
    }
}
