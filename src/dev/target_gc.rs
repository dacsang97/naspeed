//! Background sweep of the project's target dir while `ns dev` runs.
//!
//! Cargo never deletes old artifacts, so a long-lived dev session grows
//! `target/` without bound. Every `INTERVAL` this thread sweeps each profile
//! dir with `target_sweep` (units unused for the store's max age, default 14
//! days; `ns dev` marks every unit its own cargo runs use). A profile dir
//! cargo is using right now is skipped until the next round, so a sweep never
//! races a build. `NASPEED_NO_AUTO_GC=1` turns it off.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::log_bus::LogBus;
use super::process_supervisor::shutdown_requested;
use super::status_line;
use crate::gc::{cargo_build_lock, target_sweep};
use crate::store::physical_usage::human;

/// First sweep once the initial check and build have settled.
const FIRST_DELAY: Duration = Duration::from_secs(5 * 60);
const INTERVAL: Duration = Duration::from_secs(30 * 60);
const POLL: Duration = Duration::from_millis(500);

pub fn spawn(target_dir: PathBuf, bus: LogBus) -> Option<JoinHandle<()>> {
    if std::env::var_os("NASPEED_NO_AUTO_GC").is_some() {
        return None;
    }
    let age_days = crate::config::Settings::load().store_max_age_days;
    let max_age = (age_days > 0.0).then(|| Duration::from_secs_f64(age_days * 86_400.0));
    Some(std::thread::spawn(move || {
        let mut due = Instant::now() + FIRST_DELAY;
        while !shutdown_requested() {
            if Instant::now() < due {
                std::thread::sleep(POLL);
                continue;
            }
            due = Instant::now() + INTERVAL;
            sweep_once(&target_dir, max_age, &bus);
        }
    }))
}

fn sweep_once(target_dir: &Path, max_age: Option<Duration>, bus: &LogBus) {
    let mut total = target_sweep::SweepStats::default();
    for profile in profile_dirs(target_dir) {
        if shutdown_requested() {
            return;
        }
        let Some(_locks) = cargo_build_lock::lock_profile_dir(&profile) else {
            continue;
        };
        match target_sweep::sweep_profile(&profile, false, max_age) {
            Ok(stats) => total.add(&stats),
            Err(err) => {
                status_line::warn(
                    bus,
                    format!("target gc skipped {}: {err:#}", profile.display()),
                );
            }
        }
    }
    if total.bytes_freed > 0 {
        status_line::info(
            bus,
            format!(
                "target gc freed {} ({} stale units, {} incremental sessions)",
                human(total.bytes_freed),
                total.units_removed,
                total.incremental_sessions_removed
            ),
        );
    }
}

/// `target/<profile>` and `target/<triple>/<profile>` dirs cargo has built in.
fn profile_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(level1) = std::fs::read_dir(target_dir) else {
        return out;
    };
    for entry in level1.flatten() {
        let dir = entry.path();
        if dir.join(".fingerprint").is_dir() {
            out.push(dir);
        } else if let Ok(level2) = std::fs::read_dir(&dir) {
            out.extend(
                level2
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.join(".fingerprint").is_dir()),
            );
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_host_and_cross_profile_dirs_only() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path();
        for dir in [
            "debug/.fingerprint",
            "release/.fingerprint",
            "aarch64-unknown-linux-gnu/debug/.fingerprint",
            "tmp",
            "doc/core",
        ] {
            std::fs::create_dir_all(target.join(dir)).unwrap();
        }
        assert_eq!(
            profile_dirs(target),
            vec![
                target.join("aarch64-unknown-linux-gnu/debug"),
                target.join("debug"),
                target.join("release"),
            ]
        );
    }
}
