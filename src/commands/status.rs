//! `naspeed status`: store size, logical vs physical, hit rate, slots.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::cli::StatusArgs;
use crate::config::Settings;
use crate::jobserver::slot_pool;
use crate::setup::{cargo_config, cargo_config_path, installed_binary};
use crate::store::Store;
use crate::store::physical_usage::{Usage, human, target_tree_usage, tree_usage};
use crate::store::usage_log;

fn count_files(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_files(&e.path()),
            Ok(t) if t.is_file() => 1,
            _ => 0,
        })
        .sum()
}

pub fn registered_targets(store: &Store) -> Vec<PathBuf> {
    let Ok(conn) = usage_log::open_index(&store.root) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT profile_dir FROM targets ORDER BY profile_dir") else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| rows.flatten().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// One log line for a tree's size: logical bytes, and when the filesystem
/// reports it, how much of that is unshared vs shared through clones.
fn usage_line(what: &str, u: &Usage) {
    match u.private {
        Some(p) => crate::ui_info!(what; files = u.files, logical = human(u.logical),
            unshared = human(p), shared = human(u.logical.saturating_sub(p))),
        None => crate::ui_info!(what; files = u.files, logical = human(u.logical),
            sharing = "not reported by the filesystem"),
    }
}

pub fn run(args: StatusArgs) -> Result<u8> {
    let settings = Settings::load();
    crate::ui_info!("naspeed"; version = env!("CARGO_PKG_VERSION"));
    let _top = crate::ui::indent();

    let (_, doc) = cargo_config::read_config(&cargo_config_path())?;
    let wrapper = cargo_config::get(&doc, &cargo_config::key(&["build", "rustc-wrapper"]));
    let bin = installed_binary();
    let active =
        wrapper.as_deref().map(|w| w.trim_matches('"')) == Some(bin.to_string_lossy().as_ref());
    let wrapper_value = wrapper.as_deref().unwrap_or("unset");
    if active {
        crate::ui_info!("cargo wraps rustc with naspeed"; rustc_wrapper = wrapper_value);
    } else {
        crate::ui_warn!("cargo does NOT wrap rustc with naspeed"; rustc_wrapper = wrapper_value);
    }
    if let Ok(env_wrapper) = std::env::var("RUSTC_WRAPPER")
        && !env_wrapper.is_empty()
    {
        crate::ui_warn!("RUSTC_WRAPPER in this shell overrides the config"; value = env_wrapper);
    }
    if settings.disabled {
        crate::ui_warn!("NASPEED_DISABLE is set: every call passes through");
    }
    if let Some(note) =
        cargo_config::load_state(&settings.config_dir).and_then(|s| s.incremental_note)
    {
        crate::ui_info!(note);
    }
    crate::ui_info!("rustc slots";
        in_use = format!("{}/{}", slot_pool::in_use(&settings.slots_dir(), settings.slots), settings.slots),
        min_free_memory = format!("{} MiB", settings.min_free_mem_mb));

    for root in &settings.stores {
        if !root.is_dir() {
            crate::ui_warn!("store missing (volume unplugged?): builds pass through uncached";
                path = root.display());
            continue;
        }
        crate::ui_info!("store"; path = root.display());
        let _store = crate::ui::indent();
        let store = Store::open(root, settings.cow_mode)?;
        // Fold the usage log only when no gc/wrapper conflicts; otherwise
        // counts are still read from both index and log.
        if let Ok(Some(_lock)) = store.try_exclusive_lock()
            && let Ok(mut conn) = usage_log::open_index(root)
        {
            let _ = usage_log::merge(root, &mut conn);
        }
        let entries = count_files(&store.entries_dir());
        let objects = tree_usage(&store.objects_dir());
        crate::ui_info!("entries"; count = entries);
        usage_line("objects", &objects);
        if let Ok(conn) = usage_log::open_index(root) {
            let t = usage_log::totals(root, &conn, 7)?;
            let (hits, misses) = (t.hits, t.misses);
            let rate = if hits + misses > 0 {
                format!("{:.1}%", 100.0 * hits as f64 / (hits + misses) as f64)
            } else {
                "n/a".into()
            };
            crate::ui_info!("last 7 days (`naspeed stats`)"; hits = hits, misses = misses,
                hit_rate = rate, saved_compiling = super::stats::human_ms(t.saved_ms),
                saved_testing = super::stats::human_ms(t.test_saved_ms));
        }
        let targets = registered_targets(&store);
        let live: Vec<_> = targets.iter().filter(|t| t.is_dir()).collect();
        crate::ui_info!("target profile dirs"; registered = targets.len(), present = live.len());
        let mut total = Usage {
            private: Some(0),
            ..Usage::default()
        };
        for t in &live {
            let u = target_tree_usage(t);
            total.files += u.files;
            total.logical += u.logical;
            total.private = total.private.zip(u.private).map(|(a, b)| a + b);
        }
        if !live.is_empty() {
            usage_line("target files", &total);
            if let Some(p) = total.private {
                crate::ui_info!("physical estimate (store objects + unshared target bytes)";
                    size = human(objects.logical + p));
            }
        }
    }
    crate::ui_info!(
        "`du` counts every clone at full size; use the unshared/physical numbers above or `df`"
    );

    if args.check {
        return doctor();
    }
    Ok(0)
}

/// Build a tiny crate with `cargo build -v` and look for naspeed in the
/// rustc command lines cargo prints.
fn doctor() -> Result<u8> {
    let dir = std::env::temp_dir().join(format!("naspeed-doctor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"naspeed-doctor\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
    )?;
    std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}\n")?;
    let out = Command::new("cargo")
        .args(["build", "-v", "--offline"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        // Colored output puts escape codes between `Running` and the command.
        .env("CARGO_TERM_COLOR", "never")
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;
    let text = String::from_utf8_lossy(&out.stderr);
    let wrapped = text
        .lines()
        .any(|l| l.contains("Running `") && l.contains("naspeed"));
    if wrapped {
        crate::ui_info!("check: cargo build -v shows naspeed around rustc");
    } else {
        crate::ui_error!("check: cargo build -v does NOT show naspeed around rustc");
    }
    Ok(if wrapped && out.status.success() {
        0
    } else {
        1
    })
}
