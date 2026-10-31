//! `naspeed gc [--targets] [--dedup] [--dry-run]`.
//!
//! Without flags: store LRU. `--targets`: sweep registered target dirs.
//! `--dedup`: clone-dedup identical files across registered target dirs.

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::params;

use super::status::registered_targets;
use crate::cli::GcArgs;
use crate::config::Settings;
use crate::gc::{cargo_build_lock, store_lru, target_dedup, target_sweep};
use crate::store::physical_usage::human;
use crate::store::{Store, cow_copy, usage_log};

pub fn run(args: GcArgs) -> Result<u8> {
    let settings = Settings::load();
    let only_store = !args.targets && !args.dedup;
    let verb = if args.dry_run { "would free" } else { "freed" };
    for root in &settings.stores {
        if !root.is_dir() {
            crate::ui_warn!("store missing, skipped"; path = root.display());
            continue;
        }
        let store = Store::open(root, settings.cow_mode)?;
        crate::ui_info!("store"; path = root.display());
        let _store = crate::ui::indent();
        let lock = store.exclusive_lock()?;
        let mut conn = usage_log::open_index(root)?;
        usage_log::merge(root, &mut conn)?;

        if only_store || args.auto {
            let cap_gb = args.max_size_gb.unwrap_or(settings.store_cap_gb);
            let cap = (cap_gb * 1024.0 * 1024.0 * 1024.0) as u64;
            let age_days = args.max_age_days.unwrap_or(settings.store_max_age_days);
            let max_age =
                (age_days > 0.0).then(|| std::time::Duration::from_secs_f64(age_days * 86_400.0));
            let r = store_lru::run(&store, cap, max_age, args.dry_run, store_lru::GRACE)?;
            crate::ui_info!(format!("trimmed the store, {verb} {} logical", human(r.bytes_freed));
                entries_before = r.entries_before, size_before = human(r.bytes_before),
                entries_removed = r.entries_removed, objects_removed = r.objects_removed,
                cap = format!("{cap_gb}GB"), max_age = format!("{age_days}d"));
            let _note = crate::ui::indent();
            crate::ui_info!(
                "blocks still cloned into target/ dirs stay allocated until those files go too"
            );
        }
        // Target-dir work does not need the store lock; release it so builds
        // continue while we walk target dirs.
        drop(lock);
        if args.auto {
            continue;
        }

        let targets: Vec<PathBuf> = registered_targets(&store)
            .into_iter()
            .filter(|t| !args.exclude.iter().any(|e| t.starts_with(e)))
            .collect();
        let (live, gone): (Vec<PathBuf>, Vec<PathBuf>) =
            targets.into_iter().partition(|t| t.is_dir());
        if !gone.is_empty() && !args.dry_run {
            for t in &gone {
                conn.execute(
                    "DELETE FROM targets WHERE profile_dir = ?1",
                    params![t.to_string_lossy()],
                )?;
            }
            crate::ui_info!(format!(
                "forgot {} target dirs that no longer exist",
                gone.len()
            ));
        }

        let mut locked: Vec<(PathBuf, Vec<_>)> = Vec::new();
        let mut busy = 0;
        if args.targets || args.dedup {
            for profile in &live {
                match cargo_build_lock::lock_profile_dir(profile) {
                    Some(guards) => locked.push((profile.clone(), guards)),
                    None => busy += 1,
                }
            }
            if busy > 0 {
                crate::ui_warn!(format!(
                    "skipped {busy} profile dirs that cargo is using right now"
                ));
            }
        }

        if args.targets {
            let mut total = target_sweep::SweepStats::default();
            for (profile, _) in &locked {
                total.add(&target_sweep::sweep_profile(profile, args.dry_run)?);
            }
            crate::ui_info!(format!("swept target dirs, {verb} {}", human(total.bytes_freed));
                stale_units = format!("{}/{}", total.units_removed, total.units_total),
                incremental_sessions = total.incremental_sessions_removed);
        }
        if args.dedup {
            let dirs: Vec<PathBuf> = locked.iter().map(|(p, _)| p.clone()).collect();
            if dirs.first().is_some_and(|d| !cow_copy::supports_clone(d)) {
                crate::ui_warn!("dedup skipped: the volume does not support clones");
            } else {
                let s = target_dedup::dedup(&dirs, args.dry_run)?;
                crate::ui_info!(format!("deduplicated target dirs, {verb} {}", human(s.bytes_freed));
                    duplicates_64k_plus = s.candidates, replaced = s.replaced);
            }
        }
    }
    Ok(0)
}
