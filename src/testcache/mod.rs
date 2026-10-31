//! `ns test`: `cargo test` with a test result cache.
//!
//! cargo builds as usual; naspeed is installed as the target runner
//! (`--config target.<triple>.runner`), so cargo starts every test binary as
//! `naspeed __test-run <binary> <args…>` with the package's env and cwd. The
//! runner keys the run (see `test_key`) and either replays the stored output
//! of an identical passing run or runs the binary and stores a pass.
//!
//! The key covers what naspeed can see up front (binary, args, declared and
//! source-named env vars, package trees, workspace binaries). What a test
//! reads at run time is recorded by the tracer (`trace`) and stored with
//! the pass; the pass replays only while every recorded input is unchanged.
//! A run the tracer could not follow falls back to hashing every workspace
//! file outside the packages.
//! Doctests run under rustdoc, not the runner, and always run.

pub mod test_key;
pub mod test_plan;
pub mod trace;

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
use crate::store::usage_log::{self, Record};
use crate::store::{Store, now_secs, volume_root};
use crate::wrapper::execute::run_teed_env;
use crate::wrapper::{current_env, exit_code};
use test_key::RunContext;
use test_plan::{PLAN_ENV, TestPlan};

pub const TEST_RESULT_VERSION: u32 = 2;

/// Set by `ns test --explain` for the runners cargo starts.
pub const EXPLAIN_ENV: &str = "NASPEED_TEST_EXPLAIN";

#[derive(Debug, Args)]
pub struct TestArgs {
    /// Run every test binary even when an identical run already passed.
    #[arg(long)]
    pub no_cache: bool,
    /// Say for every test binary why it replays or runs (which input changed).
    #[arg(long)]
    pub explain: bool,
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
    /// What the passing run read, as the tracer recorded it.
    #[serde(default)]
    pub inputs: Option<Vec<trace::Input>>,
    /// Untraced runs: hash of the workspace files outside the packages.
    #[serde(default)]
    pub workspace_files: Option<String>,
    /// Labelled parts of the key (hashed), for `--explain`.
    #[serde(default)]
    pub components: Vec<(String, String)>,
}

/// The last key a test target ran under, so `--explain` can name the part
/// of the key that changed when there is no stored pass for the new one.
#[derive(Debug, Serialize, Deserialize)]
struct LastRun {
    key: String,
    components: Vec<(String, String)>,
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
            crate::ui_warn!(format!(
                "{t} has its own target runner; its tests run uncached"
            ));
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

/// Build and test counters of the store serving the project's target dir.
fn store_totals(project: &Project) -> Option<usage_log::Totals> {
    let settings = Settings::load();
    let root = volume_root::existing_ancestor(&project.graph.target_dir)
        .and_then(|dir| volume_root::store_for(dir, &settings.stores))?;
    let conn = usage_log::open_index(&root).ok()?;
    // Two days, so a run across midnight still diffs the same window.
    usage_log::totals(&root, &conn, 2).ok()
}

pub fn run(args: TestArgs) -> Result<u8> {
    let project = Project::load_cwd()?;
    let (mut cmd, plan) = cargo_test_command(&project, args.no_cache, &args.cargo_args)?;
    let shown: Vec<String> = args
        .cargo_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    crate::ui_info!("running cargo test"; args = shown.join(" "));
    let start = Instant::now();
    let before = store_totals(&project);
    // The test runners cargo starts print `(cached)` lines one level deeper.
    if args.explain {
        cmd.env(EXPLAIN_ENV, "1");
    }
    let status = cmd
        .args(&args.cargo_args)
        .env(crate::ui::INDENT_ENV, (crate::ui::depth() + 1).to_string())
        .status()
        .context("run cargo test")?;
    drop(plan);
    let code = exit_code(status);
    if code == 0 {
        crate::ui_info!("cargo test passed");
    } else {
        crate::ui_error!("cargo test failed"; exit_code = code);
    }
    // What this run did, from the store's counters before and after.
    if let (Some(b), Some(a)) = (before, store_totals(&project)) {
        let _run = crate::ui::indent();
        crate::ui_info!("rustc"; hits = a.hits.saturating_sub(b.hits),
            misses = a.misses.saturating_sub(b.misses));
        crate::ui_info!("tests"; replayed = a.test_hits.saturating_sub(b.test_hits),
            ran = a.test_runs.saturating_sub(b.test_runs));
        crate::ui::took(start);
    }
    Ok(code.clamp(0, 255) as u8)
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

/// A line under the cargo output, at the runner's depth.
fn note(level: crate::ui::Level, msg: &str, fields: &[(&str, &str)]) {
    let fields: Vec<(&str, &dyn std::fmt::Display)> = fields
        .iter()
        .map(|(k, v)| (*k, v as &dyn std::fmt::Display))
        .collect();
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        crate::ui::format_line(
            crate::ui::depth(),
            level,
            msg,
            &fields,
            crate::ui::color_enabled(),
        )
    );
}

/// Why a stored pass no longer applies; `None` when it does.
fn stale_reason(result: &TestResult, plan: &TestPlan, env: &crate::wrapper::Env) -> Option<String> {
    if let Some(inputs) = &result.inputs
        && let Some(i) = trace::changed(inputs, env)
    {
        return Some(format!("{} changed", i.describe()));
    }
    // Untraced runs, and traced runs that started a program the tracer
    // could not follow, also depend on the workspace files.
    let recorded = result.workspace_files.as_deref()?;
    match test_key::workspace_files_hash(plan) {
        Ok(now) if now == recorded => None,
        Ok(_) => Some("workspace files outside the packages changed (run was not traced)".into()),
        Err(e) => Some(format!("workspace files unreadable: {e:#}")),
    }
}

/// Key parts that differ between the last run of this target and now.
fn changed_components(last: &LastRun, now: &[(String, String)]) -> Vec<String> {
    let before: std::collections::BTreeMap<&str, &str> = last
        .components
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let after: std::collections::BTreeMap<&str, &str> =
        now.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut out: Vec<String> = after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(*v))
        .map(|(k, _)| (*k).to_owned())
        .collect();
    out.extend(
        before
            .keys()
            .filter(|k| !after.contains_key(*k))
            .map(|k| format!("{k} (gone)")),
    );
    out
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
    let k = test_key::key(
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
    let explain = std::env::var_os(EXPLAIN_ENV).is_some();
    let name = binary
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `deps/<target>-<hash>`: the target without cargo's hash suffix.
    let stem = name.rsplit_once('-').map_or(name.as_str(), |(s, _)| s);
    let last_id = blake3::hash(
        format!(
            "{}\0{stem}\0{}",
            manifest_dir.display(),
            str_args.join("\0")
        )
        .as_bytes(),
    )
    .to_hex()[..32]
        .to_string();
    let last_path = store
        .tests_dir()
        .join("last")
        .join(format!("{last_id}.json"));

    let path = store.test_result_path(&k.key);
    let stored = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<TestResult>(&b).ok())
        .filter(|r| r.version == TEST_RESULT_VERSION && r.key == k.key);
    let reason = match &stored {
        Some(result) => match stale_reason(result, &plan, &env) {
            None => {
                let _ = std::io::stdout().write_all(result.stdout.as_bytes());
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().write_all(result.stderr.as_bytes());
                let checked = match &result.inputs {
                    Some(i) => format!("{} recorded inputs", i.len()),
                    None => "workspace files".into(),
                };
                let took = crate::commands::stats::human_ms(result.run_ms);
                let mut fields = vec![("took", took.as_str())];
                if explain {
                    fields.push(("checked", checked.as_str()));
                }
                note(
                    crate::ui::Level::Info,
                    &format!("(cached) {name}: unchanged since a passing run, not re-run"),
                    &fields,
                );
                // mtime is the result's last use for gc.
                let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
                // gc renames the log away under the exclusive lock.
                let _lock = store.shared_lock();
                store.append_usage(&Record::TestHit {
                    key: k.key,
                    saved_ms: result.run_ms,
                });
                return Some(0);
            }
            Some(why) => why,
        },
        None => match std::fs::read(&last_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<LastRun>(&b).ok())
        {
            Some(last) if last.key != k.key => {
                let parts = changed_components(&last, &k.components);
                format!("key changed: {}", parts.join(", "))
            }
            Some(_) => "last run did not pass".into(),
            None => "no earlier run".into(),
        },
    };
    if explain {
        note(
            crate::ui::Level::Info,
            &format!("running {name}"),
            &[("why", &reason)],
        );
    }

    let lib = if plan.trace {
        trace::library(&plan.store)
    } else {
        None
    };
    let log = store.tmp_path("trace");
    let child_env = lib
        .as_deref()
        .map(|l| trace::child_env(l, &log, &env))
        .unwrap_or_default();
    let started = Instant::now();
    let output = run_teed_env(binary, args, &child_env)
        .inspect_err(|e| crate::ui_error!(format!("failed to run {}: {e:#}", binary.display())))
        .ok()?;
    let run_ms = started.elapsed().as_millis() as u64;
    let code = exit_code(output.status);
    if code == 0
        && let (Some(stdout), Some(stderr)) = (output.stdout, output.stderr)
    {
        let traced = lib.as_ref().and_then(|_| {
            let excl = trace::Exclusions::new(&plan.target_dir, &plan.store, &env);
            trace::collect(&log, binary, &env, &excl)
        });
        let complete = traced.as_ref().is_some_and(|t| !t.partial);
        let workspace_files = if complete {
            None
        } else {
            test_key::workspace_files_hash(&plan).ok()
        };
        if explain && lib.is_some() && !complete {
            note(
                crate::ui::Level::Warn,
                &format!("{name}: run not fully traced; also keyed on workspace files"),
                &[],
            );
        }
        let inputs = traced.map(|t| t.inputs);
        let result = TestResult {
            version: TEST_RESULT_VERSION,
            key: k.key.clone(),
            binary: name,
            stdout,
            stderr,
            run_ms,
            created: now_secs(),
            inputs,
            workspace_files,
            components: k.components.clone(),
        };
        let last = LastRun {
            key: k.key.clone(),
            components: k.components,
        };
        match serde_json::to_vec(&result)
            .map_err(anyhow::Error::from)
            .and_then(|b| store.write_file(&path, &b))
            .and_then(|()| store.write_file(&last_path, &serde_json::to_vec(&last)?))
        {
            Ok(()) => {
                let _lock = store.shared_lock();
                store.append_usage(&Record::TestRun { key: k.key, run_ms })
            }
            Err(e) => tracing::warn!("test result not stored: {e:#}"),
        }
    }
    let _ = std::fs::remove_file(&log);
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
