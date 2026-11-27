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
use crate::gc::{cargo_build_lock, target_dedup, target_sweep};
use crate::store::physical_usage::human;
use crate::store::cow_copy;
use crate::target_layout;

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
    let mut profiles = Vec::new();
    for profile in target_layout::profile_dirs(target_dir) {
        if shutdown_requested() {
            return;
        }
        let Some(_locks) = cargo_build_lock::lock_profile_dir(&profile) else {
            continue;
        };
        profiles.push(profile.clone());
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
    // Byte-identical files in a profile dir (incremental sessions and rustc
    // scratch outputs mostly duplicate each other) become clones of one
    // copy. Skipped silently on volumes without clone support.
    if !profiles.is_empty() && cow_copy::supports_clone(&profiles[0]) {
        match target_dedup::dedup(&profiles, false) {
            Ok(stats) if stats.bytes_freed > 0 => status_line::info(
                bus,
                format!(
                    "target gc deduplicated {} across {} files",
                    human(stats.bytes_freed),
                    stats.replaced
                ),
            ),
            Ok(_) => {}
            Err(err) => status_line::warn(bus, format!("target dedup skipped: {err:#}")),
        }
    }
}
