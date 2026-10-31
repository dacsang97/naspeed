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
            println!("store {} missing, skipped", root.display());
            continue;
        }
        let store = Store::open(root, settings.cow_mode)?;
        println!("store {}", root.display());
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
            println!(
                "  store: {} entries ({}) before; {} entries and {} objects removed, {verb} {} logical (cap {cap_gb} GB, unused > {age_days} days)",
                r.entries_before,
                human(r.bytes_before),
                r.entries_removed,
                r.objects_removed,
                human(r.bytes_freed)
            );
            println!(
                "  note : blocks still cloned into target/ dirs stay allocated until those files go too"
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
            println!("  forgot {} target dirs that no longer exist", gone.len());
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
                println!("  skipped {busy} profile dirs that cargo is using right now");
            }
        }

        if args.targets {
            let mut total = target_sweep::SweepStats::default();
            for (profile, _) in &locked {
                total.add(&target_sweep::sweep_profile(profile, args.dry_run)?);
            }
            println!(
                "  targets: {} of {} units stale, {} incremental sessions; {verb} {}",
                total.units_removed,
                total.units_total,
                total.incremental_sessions_removed,
                human(total.bytes_freed)
            );
        }
        if args.dedup {
            let dirs: Vec<PathBuf> = locked.iter().map(|(p, _)| p.clone()).collect();
            if dirs.first().is_some_and(|d| !cow_copy::supports_clone(d)) {
                println!("  dedup: volume does not support clones, skipped");
            } else {
                let s = target_dedup::dedup(&dirs, args.dry_run)?;
                println!(
                    "  dedup: {} duplicate files ≥ 64 KiB, {} replaced; {verb} {}",
                    s.candidates,
                    s.replaced,
                    human(s.bytes_freed)
                );
            }
        }
    }
    Ok(0)
}
