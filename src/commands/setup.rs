//! `naspeed setup`: install the binary, create the store, point cargo at
//! naspeed. Idempotent; the first pre-naspeed backup is never overwritten.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::SetupArgs;
use crate::config::{self, Settings};
use crate::cross;
use crate::setup::cargo_config::{self, Edit, SetupState, key};
use crate::setup::{cargo_config_path, installed_binary, shell_env_check};
use crate::store::{Store, cow_copy, now_secs, volume_root};

pub const FIRST_BACKUP_NAME: &str = "config.toml.before-naspeed";

pub const INCREMENTAL_KEPT_OFF: &str = "build.incremental = false kept: incremental sessions of workspace crates grow target dirs by several GB per project, so it stays off to save disk (pass --enable-incremental to remove it)";

pub fn backups_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("backups")
}

fn default_store() -> Result<PathBuf> {
    default_store_for(&std::env::current_dir()?, &config::home_dir())
}

/// `~/.naspeed` when `cwd` is on the home or system volume (a fresh install
/// runs `setup` from `~`), else `<volume of cwd>/.naspeed` so projects on an
/// external volume get a store they can clone from.
fn default_store_for(cwd: &Path, home: &Path) -> Result<PathBuf> {
    let mount = volume_root::mount_point(cwd).context("find volume of current dir")?;
    let on_home_volume = volume_root::device_of(cwd) == volume_root::device_of(home);
    if on_home_volume || mount == Path::new("/") {
        return Ok(home.join(".naspeed"));
    }
    Ok(mount.join(".naspeed"))
}

fn install_binary(dst: &Path) -> Result<bool> {
    let src = std::env::current_exe().context("locate naspeed binary")?;
    if std::fs::canonicalize(&src).ok() == std::fs::canonicalize(dst).ok() {
        return Ok(false);
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Copy then rename: replacing the file in place would corrupt a wrapper
    // process that is executing it right now.
    let tmp = dst.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::copy(&src, &tmp)
        .with_context(|| format!("copy {} → {}", src.display(), tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, dst)?;
    Ok(true)
}

pub fn run(args: SetupArgs) -> Result<u8> {
    let settings = Settings::load();
    let config_dir = settings.config_dir.clone();

    // Store on the projects' volume.
    let store_root = match &args.store {
        Some(p) => p.clone(),
        None => default_store()?,
    };
    std::fs::create_dir_all(&store_root)
        .with_context(|| format!("create store {}", store_root.display()))?;
    let store_root = std::fs::canonicalize(&store_root)?;
    let store = Store::open(&store_root, settings.cow_mode)?;
    let clones = cow_copy::supports_clone(&store.tmp_dir());
    std::fs::write(
        store_root.join("clone-support"),
        if clones { "yes\n" } else { "no\n" },
    )?;
    let mut stores = settings.stores.clone();
    if !stores.contains(&store_root) {
        stores.push(store_root.clone());
    }
    config::save_stores(&config_dir, &stores)?;
    if let Some(slots) = args.slots {
        let path = config_dir.join("config.toml");
        let mut doc = config::read_doc(&path).unwrap_or_default();
        doc["slots"] = toml_edit::value(slots as i64);
        std::fs::write(&path, doc.to_string())?;
    }

    // Binary on the internal disk.
    let bin = installed_binary();
    let installed = install_binary(&bin)?;
    let alias = crate::setup::ns_alias();
    if !crate::setup::install_ns_alias().with_context(|| format!("create {}", alias.display()))? {
        crate::ui_warn!("ns exists and is not naspeed's; run the front-end as `naspeed dev|release|…`";
            path = alias.display());
    }

    // Cargo config.
    let cfg_path = cargo_config_path();
    let (_, mut doc) = cargo_config::read_config(&cfg_path)?;
    let mut state = cargo_config::load_state(&config_dir).unwrap_or_else(|| SetupState {
        cargo_config: cfg_path.clone(),
        config_existed: cfg_path.exists(),
        ..SetupState::default()
    });

    let mut edits = vec![
        Edit::Set(
            key(&["build", "rustc-wrapper"]),
            toml_edit::Value::from(bin.to_string_lossy().as_ref()).to_string(),
        ),
        Edit::Set(key(&["build", "jobs"]), args.jobs.to_string()),
    ];
    if args.enable_incremental {
        edits.push(Edit::Remove(key(&["build", "incremental"])));
        state.incremental_note = None;
    } else if cargo_config::get(&doc, &key(&["build", "incremental"])).as_deref() == Some("false") {
        state.incremental_note = Some(INCREMENTAL_KEPT_OFF.to_owned());
    }
    if let Some(level) = &args.deps_debug {
        let value = match level.as_str() {
            "true" | "false" => level.clone(),
            n if n.parse::<u8>().is_ok() => n.to_owned(),
            other => toml_edit::Value::from(other).to_string(),
        };
        edits.push(Edit::Set(
            key(&["profile", "dev", "package", "*", "debug"]),
            value,
        ));
    }
    // Settings left by the wrapper naspeed replaces.
    let old_wrapper_env: Vec<String> = doc
        .get("env")
        .and_then(|e| e.as_table_like())
        .map(|t| {
            t.iter()
                .map(|(k, _)| k.to_owned())
                .filter(|k| k.starts_with("SCCACHE_"))
                .collect()
        })
        .unwrap_or_default();
    for name in old_wrapper_env {
        edits.push(Edit::Remove(key(&["env", &name])));
    }
    let mut cross_notes = Vec::new();
    if args.cross {
        let plan = cross::shim_install::prepare(&config_dir, &store_root, &bin, &args)?;
        edits.extend(plan.edits);
        state.shims = plan.shims;
        cross_notes = plan.notes;
    }

    let changed = cargo_config::apply(&mut doc, &edits, &mut state)?;
    if changed {
        let backups = backups_dir(&config_dir);
        std::fs::create_dir_all(&backups)?;
        if cfg_path.exists() {
            let first = backups.join(FIRST_BACKUP_NAME);
            if !first.exists() {
                std::fs::copy(&cfg_path, &first)?;
            }
            if state.first_backup.is_none() {
                state.first_backup = Some(first);
            }
            std::fs::copy(
                &cfg_path,
                backups.join(format!("config.toml.{}", now_secs())),
            )?;
        }
        if let Some(parent) = cfg_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = doc.to_string();
        std::fs::write(&cfg_path, &text)?;
        state.written_hash = Some(blake3::hash(text.as_bytes()).to_hex().to_string());
    }
    cargo_config::save_state(&config_dir, &state)?;

    crate::ui_info!("setting up naspeed");
    let _setup = crate::ui::indent();
    crate::ui_info!(if installed { "binary installed" } else { "binary already installed" };
        path = bin.display());
    if clones {
        crate::ui_info!("store ready"; path = store_root.display(), clones = "yes");
    } else {
        crate::ui_warn!("store has no copy-on-write clones: builds are faster but save no disk";
            path = store_root.display());
    }
    if changed {
        crate::ui_info!("cargo config updated";
            path = cfg_path.display(), backup = "~/.config/naspeed/backups");
    } else {
        crate::ui_info!("cargo config already set"; path = cfg_path.display());
    }
    crate::ui_info!("rustc slots"; machine_wide = Settings::load().slots, build_jobs = args.jobs);
    if let Some(note) = &state.incremental_note {
        crate::ui_info!(note);
    }
    for note in cross_notes {
        crate::ui_info!("cross"; note = note);
    }
    let warnings = shell_env_check::report(&config::home_dir(), &bin);
    if !warnings.is_empty() {
        crate::ui_warn!("action needed (naspeed does not edit shell rc files)");
        let _list = crate::ui::indent();
        for w in warnings {
            crate::ui_warn!(w);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_store_is_in_home_when_cwd_shares_its_volume() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("code/app");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(
            default_store_for(&project, home.path()).unwrap(),
            home.path().join(".naspeed")
        );
    }
}
