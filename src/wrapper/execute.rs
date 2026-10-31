//! Hit / miss flow for one cacheable rustc invocation.
//!
//! Hit: clone every stored output into `--out-dir`, rewrite the `.d` file,
//! replay rustc's stdout/stderr, exit 0. No rustc, no slot.
//! Miss: hold the key lock, run rustc with stdout/stderr teed line by line
//! (cargo must see the rmeta artifact notification immediately to keep
//! pipelining), then store the outputs.
//!
//! Every failure before rustc starts returns `None` so the caller passes
//! through; failures after rustc ran only cost the cache entry.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use filetime::FileTime;
use tracing::{debug, info, warn};

use super::build_script_run;
use super::cache_key::{self, FsContents, KeyContext};
use super::cacheability::{self, Cacheable, SourceKind, UnitKind};
use super::extern_identity;
use super::rustc_args::{self, RustcInvocation};
use super::toolchain_fingerprint;
use super::{Env, current_env, exit_code};
use crate::config::{CowMode, Settings, cargo_home};
use crate::jobserver;
use crate::remote::peer_client::PeerClient;
use crate::store::path_placeholders::Placeholders;
use crate::store::usage_log::Record;
use crate::store::{
    DepInfoFile, ENTRY_VERSION, Entry, Manifest, OutputFile, Store, cow_copy, now_secs, volume_root,
};

/// Prefixes local crates' artifacts carry instead of real paths.
pub const REMAP_WORKSPACE: &str = "/naspeed/workspace";
pub const REMAP_TARGET: &str = "/naspeed/target";

struct Job<'a> {
    settings: &'a Settings,
    program: &'a Path,
    /// Cargo's arguments, plus the remap flags naspeed adds for local crates.
    args: &'a [OsString],
    cwd: std::path::PathBuf,
    env: Env,
    cacheable: Cacheable,
    store: Store,
    placeholders: Placeholders,
    tier1: String,
    /// Expanded rustc args for an incremental warm-up after a hit
    /// (workspace crates compiled with `-C incremental` only).
    warm_args: Option<Vec<OsString>>,
}

pub fn run(settings: &Settings, program: &Path, args: &[OsString]) -> Option<i32> {
    let cwd = std::env::current_dir().ok()?;
    let env = current_env();
    let cargo_home = env
        .get("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(cargo_home);
    let expanded = rustc_args::expand_response_files(args, &cwd).ok()?;
    let inv = rustc_args::parse(&expanded);
    let cacheable = match cacheability::check(
        program,
        &inv,
        &cwd,
        &env,
        &cargo_home,
        settings.workspace_cache,
    ) {
        Ok(c) => c,
        Err(reason) => {
            debug!(crate_name = ?inv.crate_name, %reason, "passthrough");
            return None;
        }
    };
    // Local crates: keep the checkout path out of the artifacts so other
    // worktrees can share them (debuggers see the remapped prefixes).
    let local = cacheable.source_kind == SourceKind::Local;
    let mut run_args = args.to_vec();
    // A remap the user (or `ns release`, for CARGO_HOME) already set is kept;
    // ours is skipped only when one covers the crate's own sources.
    let inv = if local && !remaps_path(&inv, &cacheable.source_root) {
        for (from, to) in [
            (&cacheable.source_root, REMAP_WORKSPACE),
            (&cacheable.profile_dir, REMAP_TARGET),
        ] {
            run_args.push("--remap-path-prefix".into());
            run_args.push(format!("{}={to}", from.display()).into());
        }
        rustc_args::parse(&rustc_args::expand_response_files(&run_args, &cwd).ok()?)
    } else {
        inv
    };
    let Some(store_root) = volume_root::store_for(&cacheable.out_dir, &settings.stores) else {
        debug!(out_dir = %cacheable.out_dir.display(), "no store on this volume");
        return None;
    };
    let store = Store::open(&store_root, settings.cow_mode)
        .inspect_err(|e| warn!("open store: {e:#}"))
        .ok()?;
    let _shared = store.shared_lock().ok()?;

    let toolchain = toolchain_fingerprint::resolve(program, &cwd, &env, &store.memo_dir())
        .inspect_err(|e| warn!("toolchain: {e:#}"))
        .ok()?;
    let mut placeholders = Placeholders::new(&cacheable.profile_dir, &cargo_home);
    if local {
        placeholders = placeholders.with_workspace(&cacheable.source_root);
    }
    let memo = store.memo_dir();
    let tier1 = cache_key::tier1(
        &KeyContext {
            inv: &inv,
            cacheable: &cacheable,
            cwd: &cwd,
            env: &env,
            toolchain: &toolchain,
            placeholders: &placeholders,
        },
        &FsContents {
            memo_dir: Some(&memo),
            placeholders: Some(&placeholders),
        },
    )
    .inspect_err(|e| warn!("key: {e:#}"))
    .ok()?;

    store.append_usage(&Record::Target {
        profile_dir: cacheable.profile_dir.clone(),
    });
    let warm_args = (local && inv.incremental && settings.warm_incremental)
        .then(|| rustc_args::expand_response_files(&run_args, &cwd).ok())
        .flatten();
    let job = Job {
        settings,
        program,
        args: &run_args,
        cwd: cwd.clone(),
        env,
        cacheable,
        store,
        placeholders,
        tier1,
        warm_args,
    };

    if let Some(code) = job.try_hit() {
        return Some(code);
    }
    // Waiting here holds no rustc slot: if another build is compiling the
    // same key we will most likely hit once it finishes.
    let _key_lock = job.store.key_lock(&job.tier1).ok()?;
    if let Some(code) = job.try_hit() {
        return Some(code);
    }
    if let Some(code) = job.try_peer() {
        return Some(code);
    }
    Some(job.compile_and_store(&inv))
}

impl Job<'_> {
    /// Fetch the entry from a peer store into the local one, then restore it.
    fn try_peer(&self) -> Option<i32> {
        let client = PeerClient::new(&self.store, self.settings)?;
        client.fetch(&self.tier1, &|manifest| {
            cache_key::tier2(
                &self.tier1,
                &manifest.env_deps,
                &self.env,
                &self.placeholders,
                &self.cwd,
            )
        })?;
        self.try_hit()
    }

    fn try_hit(&self) -> Option<i32> {
        let manifest = self.store.read_manifest(&self.tier1)?;
        let key = cache_key::tier2(
            &self.tier1,
            &manifest.env_deps,
            &self.env,
            &self.placeholders,
            &self.cwd,
        );
        let entry = self.store.read_entry(&key)?;
        let restore_started = Instant::now();
        match self.restore(&entry) {
            Ok(()) => {}
            Err(e) => {
                warn!(crate_name = %entry.crate_name, "restore failed, recompiling: {e:#}");
                if e.downcast_ref::<CorruptObject>().is_some() {
                    self.store.remove_entry(&key);
                }
                return None;
            }
        }
        self.install_build_script_launcher(&entry);
        replay(&entry, &self.placeholders);
        if let Some(args) = &self.warm_args {
            super::incremental_warm::spawn(self.program, args);
        }
        let restore_ms = restore_started.elapsed().as_millis() as u64;
        self.store.append_usage(&Record::Hit {
            key,
            saved_ms: entry.compile_ms.saturating_sub(restore_ms),
        });
        info!(crate_name = %entry.crate_name, "hit");
        Some(0)
    }

    fn restore(&self, entry: &Entry) -> Result<()> {
        let out_dir = &self.cacheable.out_dir;
        for o in &entry.outputs {
            let object = self.store.object_path(&o.object);
            let ok = std::fs::metadata(&object).is_ok_and(|m| m.len() == o.size);
            if !ok {
                return Err(CorruptObject(o.object.clone()).into());
            }
        }
        let pid = std::process::id();
        let mut staged = Vec::with_capacity(entry.outputs.len() + 1);
        let result = (|| -> Result<()> {
            for o in &entry.outputs {
                let tmp = out_dir.join(format!(".naspeed-{pid}-{}", o.name));
                let _ = std::fs::remove_file(&tmp);
                let method = cow_copy::clone_or_copy(
                    &self.store.object_path(&o.object),
                    &tmp,
                    self.settings.cow_mode,
                )
                .with_context(|| format!("restore {}", o.name))?;
                staged.push((tmp.clone(), out_dir.join(&o.name)));
                if method != cow_copy::Method::Hardlink {
                    // Clones keep the object's read-only mode; rustc must be
                    // able to overwrite these files on a later rebuild.
                    std::fs::set_permissions(
                        &tmp,
                        std::fs::Permissions::from_mode(o.mode | 0o200),
                    )?;
                    filetime::set_file_mtime(&tmp, FileTime::now())?;
                    let _ = extern_identity::stamp(&tmp, &o.object);
                    let _ = extern_identity::mark_placed(&tmp);
                }
            }
            if let Some(dep) = &entry.dep_info {
                let tmp = out_dir.join(format!(".naspeed-{pid}-{}", dep.name));
                std::fs::write(&tmp, self.placeholders.decode(&dep.text))?;
                let _ = extern_identity::mark_placed(&tmp);
                staged.push((tmp, out_dir.join(&dep.name)));
            }
            for (tmp, dst) in &staged {
                std::fs::rename(tmp, dst)
                    .with_context(|| format!("rename into {}", dst.display()))?;
            }
            Ok(())
        })();
        if result.is_err() {
            for (tmp, _) in &staged {
                let _ = std::fs::remove_file(tmp);
            }
        }
        result
    }

    fn compile_and_store(&self, inv: &RustcInvocation) -> i32 {
        let started = SystemTime::now();
        let permit = jobserver::acquire(self.settings);
        debug!("rustc start");
        let rustc_started = Instant::now();
        let output = run_teed(self.program, self.args);
        let compile_ms = rustc_started.elapsed().as_millis() as u64;
        debug!("rustc end");
        drop(permit);
        let output = match output {
            Ok(output) => output,
            Err(e) => {
                eprintln!("naspeed: failed to run {}: {e:#}", self.program.display());
                return 1;
            }
        };
        let code = exit_code(output.status);
        if code != 0 {
            return code;
        }
        match self.store_outputs(inv, &output, started, compile_ms) {
            Ok(key) => {
                info!(crate_name = ?inv.crate_name, %key, "miss stored");
                if let Some(entry) = self.store.read_entry(&key) {
                    self.install_build_script_launcher(&entry);
                }
                crate::gc::auto::maybe_spawn(&self.store.root);
            }
            Err(e) => warn!(crate_name = ?inv.crate_name, "not stored: {e:#}"),
        }
        code
    }

    /// For a build script, put naspeed's launcher where cargo expects the
    /// binary so the script's run can be cached too (see `build_script_run`).
    fn install_build_script_launcher(&self, entry: &Entry) {
        if self.cacheable.kind != UnitKind::BuildScript || !self.settings.build_script_cache {
            return;
        }
        let stem = &self.cacheable.output_stem;
        let Some(binary) = entry.outputs.iter().find(|o| o.name == *stem) else {
            return;
        };
        if let Err(e) =
            build_script_run::install_launcher(&self.cacheable.out_dir.join(stem), &binary.object)
        {
            warn!(%stem, "launcher not installed: {e:#}");
        }
    }

    fn store_outputs(
        &self,
        inv: &RustcInvocation,
        output: &TeedOutput,
        started: SystemTime,
        compile_ms: u64,
    ) -> Result<String> {
        let (Some(stdout), Some(stderr)) = (&output.stdout, &output.stderr) else {
            bail!("rustc output is not UTF-8");
        };
        let out_dir = &self.cacheable.out_dir;
        let stem = &self.cacheable.output_stem;
        let (plain, lib) = (format!("{stem}."), format!("lib{stem}."));
        let dep_name = format!("{stem}.d");
        // Files written by this rustc only; older same-stem files are stale.
        let since = started - Duration::from_secs(1);

        // Outputs are ingested only after every check below passed, so a
        // refused store never leaves unreferenced objects behind.
        let mut pending = Vec::new();
        let mut dep_info = None;
        let mut env_deps = Vec::new();
        let mut stamped = Vec::new();
        let mut names: Vec<_> = std::fs::read_dir(out_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            // A build script's binary is exactly `<stem>`, without extension.
            .filter(|n| n.starts_with(&plain) || n.starts_with(&lib) || n == stem)
            .collect();
        names.sort();
        for name in names {
            let path = out_dir.join(&name);
            let meta = std::fs::symlink_metadata(&path)?;
            if !meta.file_type().is_file() {
                bail!("unexpected non-file output {name}");
            }
            if meta.modified()? < since {
                continue;
            }
            if name == dep_name {
                let raw = std::fs::read_to_string(&path)?;
                env_deps = cache_key::env_dep_names(&raw);
                if self.cacheable.source_kind == SourceKind::Local {
                    env_deps.extend(cache_key::dep_info_files(&raw).into_iter().map(|f| {
                        format!("{}{}", cache_key::FILE_DEP, self.placeholders.encode(&f))
                    }));
                }
                dep_info = Some(DepInfoFile {
                    name,
                    text: self.placeholders.encode(&raw),
                });
            } else {
                pending.push((name, path, meta.permissions().mode() & 0o777));
            }
        }
        let has_main_output = match self.cacheable.kind {
            UnitKind::Library => pending.iter().any(|(name, _, _)| {
                name.ends_with(".rlib")
                    || name.ends_with(".rmeta")
                    || name.ends_with(".dylib")
                    || name.ends_with(".so")
            }),
            UnitKind::BuildScript => pending.iter().any(|(name, _, _)| name == stem),
        };
        if !has_main_output {
            bail!("no main output found for {stem}");
        }
        if inv.emit_kinds().any(|k| k == "dep-info") && dep_info.is_none() {
            bail!("missing {dep_name}");
        }

        if self.cacheable.source_kind == SourceKind::Local
            && !env_deps.iter().any(|d| d.starts_with(cache_key::FILE_DEP))
        {
            // Without the list of files rustc read, the key would not cover
            // the sources at all.
            bail!("no source files in dep-info");
        }
        // A source edited while rustc ran: the output matches the old text
        // but the key would hash the new one, so the result is not stored.
        // Sub-second mtimes are compared with the start exactly, so a file
        // saved just before the build (`ns dev` builds 150 ms after a save)
        // is still stored; whole-second mtimes keep the 1 s slack.
        // Files under the profile dir (a build script's OUT_DIR) are always
        // fresh and nobody edits them by hand; their content is in the key.
        for dep in &env_deps {
            if let Some(path) = dep.strip_prefix(cache_key::FILE_DEP) {
                if path.starts_with(crate::store::path_placeholders::PROFILE_TOKEN) {
                    continue;
                }
                let path = self.cwd.join(self.placeholders.decode(path));
                if std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| edited_during(t, started))
                {
                    bail!("{} changed during the compile", path.display());
                }
            }
        }
        let mut outputs = Vec::new();
        for (name, path, mode) in pending {
            let (object, size) = self.store.ingest_object(&path)?;
            stamped.push((path, object.clone()));
            outputs.push(OutputFile {
                name,
                object,
                size,
                mode,
            });
        }
        let key = cache_key::tier2(
            &self.tier1,
            &env_deps,
            &self.env,
            &self.placeholders,
            &self.cwd,
        );
        let entry = Entry {
            version: ENTRY_VERSION,
            key: key.clone(),
            tier1: self.tier1.clone(),
            crate_name: inv.crate_name.clone().unwrap_or_default(),
            outputs,
            dep_info,
            stdout: self.placeholders.encode(stdout),
            stderr: self.placeholders.encode(stderr),
            created: now_secs(),
            texts: Vec::new(),
            symlinks: Vec::new(),
            dirs: Vec::new(),
            compile_ms,
        };
        // Objects are already in place; the manifest goes before the entry
        // so a reader that finds the manifest can compute the same key.
        self.store
            .write_manifest(&self.tier1, &Manifest { env_deps })?;
        self.store.write_entry(&entry)?;
        if self.settings.cow_mode != CowMode::Hardlink {
            for (path, object) in &stamped {
                let _ = extern_identity::stamp(path, object);
            }
        }
        self.store.append_usage(&Record::Miss {
            key: key.clone(),
            crate_name: entry.crate_name.clone(),
            size: entry.size(),
            compile_ms,
        });
        Ok(key)
    }
}

/// True when one of the invocation's `--remap-path-prefix` rules rewrites
/// paths under (or above) `dir`.
fn remaps_path(inv: &RustcInvocation, dir: &Path) -> bool {
    inv.tokens
        .iter()
        .filter(|t| t.flag == "--remap-path-prefix")
        .filter_map(|t| {
            t.value
                .as_ref()?
                .to_str()?
                .split_once('=')
                .map(|(from, _)| from.to_owned())
        })
        .any(|from| {
            let from = Path::new(&from);
            dir.starts_with(from) || from.starts_with(dir)
        })
}

/// True when an input with mtime `t` may have changed after a compile that
/// started at `started` began.
fn edited_during(t: SystemTime, started: SystemTime) -> bool {
    let whole_seconds = t
        .duration_since(std::time::UNIX_EPOCH)
        .is_ok_and(|d| d.subsec_nanos() == 0);
    if whole_seconds {
        t >= started - Duration::from_secs(1)
    } else {
        t >= started
    }
}

#[derive(Debug)]
struct CorruptObject(String);

impl std::fmt::Display for CorruptObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "object {} missing or truncated", self.0)
    }
}

impl std::error::Error for CorruptObject {}

fn replay(entry: &Entry, ph: &Placeholders) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(ph.decode(&entry.stdout).as_bytes());
    let _ = out.flush();
    let mut err = std::io::stderr().lock();
    for line in entry.stderr.split_inclusive('\n') {
        let decoded = if line.starts_with('{') {
            ph.decode_json(line)
        } else {
            ph.decode(line)
        };
        let _ = err.write_all(decoded.as_bytes());
    }
    let _ = err.flush();
}

pub(crate) struct TeedOutput {
    pub status: std::process::ExitStatus,
    /// `None` when the stream was not valid UTF-8 (then nothing is stored).
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

fn tee<R: Read + Send + 'static, W: Write>(src: R, mut dst: W) -> Vec<u8> {
    let mut reader = BufReader::new(src);
    let mut all = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let _ = dst.write_all(&line);
                let _ = dst.flush();
                all.extend_from_slice(&line);
            }
        }
    }
    all
}

/// Run a program, forwarding each stdout/stderr line as soon as it is written.
pub(crate) fn run_teed(program: &Path, args: &[OsString]) -> Result<TeedOutput> {
    run_teed_env(program, args, &[])
}

/// `run_teed` with extra environment variables for the child.
pub(crate) fn run_teed_env(
    program: &Path,
    args: &[OsString],
    envs: &[(OsString, OsString)],
) -> Result<TeedOutput> {
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", program.display()))?;
    let stdout = child.stdout.take().context("stdout")?;
    let stderr = child.stderr.take().context("stderr")?;
    let out_thread = std::thread::spawn(move || tee(stdout, std::io::stdout()));
    let err_thread = std::thread::spawn(move || tee(stderr, std::io::stderr()));
    let status = child.wait()?;
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(TeedOutput {
        status,
        stdout: String::from_utf8(stdout).ok(),
        stderr: String::from_utf8(stderr).ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_before_the_compile_are_not_too_new() {
        let started = SystemTime::UNIX_EPOCH + Duration::new(1_000, 500_000_000);
        // Saved 150 ms before the build started (ns dev): storable.
        assert!(!edited_during(
            started - Duration::from_millis(150),
            started
        ));
        assert!(edited_during(started + Duration::from_millis(1), started));
        // Whole-second mtimes (coarse filesystems) keep the 1 s slack.
        assert!(edited_during(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            started
        ));
        assert!(!edited_during(
            SystemTime::UNIX_EPOCH + Duration::from_secs(998),
            started
        ));
    }
}
