//! `naspeed __sweep-target <profile_dir>` — detached opportunistic cleanup
//! the rustc wrapper spawns while cargo keeps working. Sweeps stale
//! `-<hash>` unit variants and old incremental sessions, then replaces
//! byte-identical files with clones, under cargo's build lock so an active
//! build is left for the next caller.
//!
//! Spawned only after `NASPEED_SWEEP_STAMP` showed the dir due again;
//! `NASPEED_NO_AUTO_GC` skips it entirely. Always quiet: the wrapper's
//! stdout belongs to cargo.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::info;

use crate::config::Settings;
use crate::gc::{cargo_build_lock, target_dedup, target_sweep};
use crate::store::cow_copy;

#[path = "../compiler/patch_gc.rs"]
mod patch_gc;

/// Minimum spacing between sweeps of one profile dir.
pub const INTERVAL: Duration = Duration::from_secs(4 * 3600);

/// Stamp the wrapper touches when it spawns this command. Sibling of the
/// profile dir's own files, under cargo's own marker area.
pub fn stamp_path(profile_dir: &Path) -> std::path::PathBuf {
    profile_dir.join(".naspeed-sweep-stamp")
}

/// True when the profile dir is due for a sweep. The wrapper stats the
/// stamp before spawning; a missing stamp means due.
pub fn due(profile_dir: &Path) -> bool {
    match std::fs::metadata(stamp_path(profile_dir)) {
        Ok(m) => m
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= INTERVAL),
        Err(_) => true,
    }
}

/// Mark the stamp without running the sweep (spawn path) or after it ran.
pub fn touch(profile_dir: &Path) {
    let stamp = stamp_path(profile_dir);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&stamp)
        .and_then(|f| filetime::set_file_mtime(&stamp, filetime::FileTime::now()).map(|_| f));
}

pub fn run(profile_dir: &Path) -> anyhow::Result<u8> {
    if std::env::var_os("NASPEED_NO_AUTO_GC").is_some() || !profile_dir.is_dir() {
        return Ok(0);
    }
    let settings = Settings::load();
    let age_days = settings.store_max_age_days;
    let max_age =
        (age_days > 0.0).then(|| Duration::from_secs_f64(age_days * 86_400.0));
    // Lazy app-patch generations and images accumulate the same way cargo
    // units do; the build-time collector only runs when a build publishes,
    // so the sweep covers the stale tail. It does not need cargo's lock:
    // patch_gc synchronizes on its own and only deletes unreferenced
    // entries, so it is safe while `ns dev` or cargo holds the dir.
    if let Err(error) = sweep_lazy_cache(profile_dir) {
        eprintln!("ns sweep: lazy cache sweep skipped: {error}");
    }
    // Cargo still building: leave everything for the next due check. The
    // stamp stays old so a later rustc re-triggers.
    let Some(_locks) = cargo_build_lock::lock_profile_dir(profile_dir) else {
        return Ok(0);
    };
    let stats = target_sweep::sweep_profile(profile_dir, false, max_age)?;
    info!(
        bytes = stats.bytes_freed,
        units = stats.units_removed,
        sessions = stats.incremental_sessions_removed,
        "swept {}",
        profile_dir.display()
    );
    if cow_copy::supports_clone(profile_dir) {
        let dedup = target_dedup::dedup(&[profile_dir.to_path_buf()], false)?;
        info!(bytes = dedup.bytes_freed, replaced = dedup.replaced, "dedup done");
    }
    touch(profile_dir);
    Ok(0)
}

/// Detached spawn used by the wrapper. Cheap when not due: one stat.
/// Returns without waiting; cargo's lock makes the sweep skip itself when
/// it would race this build.
pub fn maybe_spawn(profile_dir: &Path) {
    if std::env::var_os("NASPEED_NO_AUTO_GC").is_some() || !due(profile_dir) {
        return;
    }
    // Claim the interval up front; losing a race with another wrapper only
    // means one extra stat next time.
    touch(profile_dir);
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .arg("__sweep-target")
        .arg(profile_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Generations to retain per binary owner beyond live/leased ones.
const LAZY_KEEP: usize = 8;

/// For every binary owning a `patch-lease`, run the same mark-and-sweep a
/// publishing build would: keep the newest generations plus anything
/// leased or referenced from `live/`, remove the rest. Skips silently when
/// the global lock is held by a build or a running launcher.
fn sweep_lazy_cache(profile_dir: &Path) -> anyhow::Result<()> {
    use std::num::NonZeroUsize;
    // Plans record absolute image paths; marking needs the cache path in
    // the same absolute form for the prefix check to match.
    let deps = profile_dir
        .join("deps")
        .join(".naspeed-lazy")
        .canonicalize()
        .unwrap_or_else(|_| profile_dir.join("deps").join(".naspeed-lazy"));
    if !deps.is_dir() {
        return Ok(());
    }
    let Ok((_lock, exclusive)) = patch_gc::begin(&deps, true) else {
        return Ok(());
    };
    if !exclusive {
        return Ok(());
    }
    let Some(keep) = NonZeroUsize::new(LAZY_KEEP) else {
        return Ok(());
    };
    // Owner name per generation = its patch-lease content; a generation
    // without one is already orphaned and safe to drop under any owner.
    let mut owners: std::collections::BTreeMap<Vec<u8>, Option<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut newest_by_owner: std::collections::BTreeMap<Vec<u8>, u128> =
        std::collections::BTreeMap::new();
    let mut gens: Vec<(u128, PathBuf, Option<Vec<u8>>)> = Vec::new();
    for entry in std::fs::read_dir(&deps)?.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        let Some((pid, nonce)) = text.split_once('-') else { continue };
        if pid.parse::<u32>().is_err()
            || nonce.is_empty()
            || !nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !entry.file_type().is_ok_and(|t| t.is_dir())
        {
            continue;
        }
        let Ok(nonce) = u128::from_str_radix(nonce, 16) else { continue };
        let lease = std::fs::read(entry.path().join("patch-lease")).ok();
        gens.push((nonce, entry.path(), lease.clone()));
        if let Some(owner) = &lease {
            newest_by_owner
                .entry(owner.clone())
                .and_modify(|n| *n = (*n).max(nonce))
                .or_insert(nonce);
        }
    }
    for (nonce, path, lease) in gens {
        let Some(owner) = lease else { continue };
        if newest_by_owner.get(&owner) == Some(&nonce) {
            owners.insert(owner, Some(path));
        }
    }
    for (owner, current) in owners {
        patch_gc::sweep(
            &deps,
            current.as_deref().unwrap_or(&deps),
            std::ffi::OsStr::from_bytes(&owner),
            keep,
        )?;
    }
    Ok(())
}
