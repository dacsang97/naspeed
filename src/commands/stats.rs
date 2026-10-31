//! `naspeed stats`: what the caches actually saved — compile time avoided by
//! rustc / build-script hits, test runs replayed, entries fetched from peers.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::cli::StatsArgs;
use crate::config::Settings;
use crate::store::Store;
use crate::store::physical_usage::human;
use crate::store::usage_log::{self, Totals};

#[derive(Debug, Serialize)]
struct CrateSaving {
    crate_name: String,
    hits: u64,
    saved_ms: u64,
}

#[derive(Debug, Serialize)]
struct StoreStats {
    store: String,
    days: i64,
    hits: u64,
    misses: u64,
    hit_rate: Option<f64>,
    saved_ms: u64,
    compile_ms: u64,
    test_hits: u64,
    test_runs: u64,
    test_saved_ms: u64,
    remote_hits: u64,
    remote_bytes: u64,
    /// All time, from the merged index.
    top_crates: Vec<CrateSaving>,
}

/// `1h 02m`, `3m 07s`, `850ms`.
pub fn human_ms(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 => format!("{ms}ms"),
        1..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn top_crates(conn: &Connection, limit: usize) -> Result<Vec<CrateSaving>> {
    let mut stmt = conn.prepare(
        "SELECT crate_name, sum(hits), sum(saved_ms) FROM entries
         WHERE saved_ms > 0 GROUP BY crate_name ORDER BY sum(saved_ms) DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(CrateSaving {
            crate_name: r.get(0)?,
            hits: r.get::<_, i64>(1)?.max(0) as u64,
            saved_ms: r.get::<_, i64>(2)?.max(0) as u64,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn collect(store: &Store, days: i64) -> Result<StoreStats> {
    // Fold the log when no build holds the store; otherwise `totals` reads
    // the unmerged lines too (only the per-crate table lags behind).
    if let Ok(Some(_lock)) = store.try_exclusive_lock()
        && let Ok(mut conn) = usage_log::open_index(&store.root)
    {
        let _ = usage_log::merge(&store.root, &mut conn);
    }
    let conn = usage_log::open_index(&store.root)?;
    let t: Totals = usage_log::totals(&store.root, &conn, days)?;
    let lookups = t.hits + t.misses;
    Ok(StoreStats {
        store: store.root.display().to_string(),
        days,
        hits: t.hits,
        misses: t.misses,
        hit_rate: (lookups > 0).then(|| t.hits as f64 / lookups as f64),
        saved_ms: t.saved_ms,
        compile_ms: t.compile_ms,
        test_hits: t.test_hits,
        test_runs: t.test_runs,
        test_saved_ms: t.test_saved_ms,
        remote_hits: t.remote_hits,
        remote_bytes: t.remote_bytes,
        top_crates: top_crates(&conn, 10)?,
    })
}

fn print(s: &StoreStats) {
    println!("store {} — last {} day(s)", s.store, s.days);
    let rate = s
        .hit_rate
        .map(|r| format!("{:.1}%", r * 100.0))
        .unwrap_or_else(|| "n/a".into());
    println!(
        "  rustc   : {} hits, {} misses ({rate}); saved {} of compile time, spent {} compiling misses",
        s.hits,
        s.misses,
        human_ms(s.saved_ms),
        human_ms(s.compile_ms)
    );
    println!(
        "  tests   : {} runs replayed from cache, {} ran; saved {}",
        s.test_hits,
        s.test_runs,
        human_ms(s.test_saved_ms)
    );
    println!(
        "  peers   : {} entries fetched ({})",
        s.remote_hits,
        human(s.remote_bytes)
    );
    println!(
        "  total   : {} saved",
        human_ms(s.saved_ms + s.test_saved_ms)
    );
    if !s.top_crates.is_empty() {
        println!("  top crates by time saved (all time):");
        for c in &s.top_crates {
            println!(
                "    {:<40} {:>6} hits  {}",
                c.crate_name,
                c.hits,
                human_ms(c.saved_ms)
            );
        }
    }
    println!(
        "  Saved time is each hit's original compile time minus its restore time;\n  hits on entries stored before naspeed recorded compile times count as 0."
    );
}

pub fn run(args: StatsArgs) -> Result<u8> {
    let settings = Settings::load();
    let days = args.days.max(1);
    let mut all = Vec::new();
    for root in &settings.stores {
        if !root.is_dir() {
            continue;
        }
        let store = Store::open(root, settings.cow_mode)?;
        all.push(collect(&store, days)?);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&all)?);
    } else if all.is_empty() {
        println!("no store found: run `naspeed setup` first");
    } else {
        for (i, s) in all.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print(s);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_ms(850), "850ms");
        assert_eq!(human_ms(1500), "1.5s");
        assert_eq!(human_ms(187_000), "3m 07s");
        assert_eq!(human_ms(3_720_000), "1h 02m");
    }

    #[test]
    fn collects_saved_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), crate::config::CowMode::Copy).unwrap();
        store.append_usage(&usage_log::Record::Miss {
            key: "k".into(),
            crate_name: "serde".into(),
            size: 1,
            compile_ms: 4000,
        });
        store.append_usage(&usage_log::Record::Hit {
            key: "k".into(),
            saved_ms: 3900,
        });
        let s = collect(&store, 7).unwrap();
        assert_eq!((s.hits, s.misses, s.saved_ms), (1, 1, 3900));
        assert_eq!(s.top_crates.len(), 1);
        assert_eq!(s.top_crates[0].crate_name, "serde");
        assert_eq!(s.top_crates[0].saved_ms, 3900);
    }
}
