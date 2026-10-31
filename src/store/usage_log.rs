//! Hits, misses and seen target dirs are appended to `usage.log` (one short
//! `O_APPEND` write per record, no locking) and folded into `index.sqlite`
//! only by `status`/`gc` while they hold the exclusive store lock. This keeps
//! sqlite out of the hot path and avoids `SQLITE_BUSY` with many wrappers.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// rustc / build-script output restored; `saved_ms` is the entry's
    /// compile time minus the restore time.
    Hit {
        key: String,
        saved_ms: u64,
    },
    Miss {
        key: String,
        crate_name: String,
        size: u64,
        compile_ms: u64,
    },
    Target {
        profile_dir: PathBuf,
    },
    /// A test binary run replayed from the test cache.
    TestHit {
        key: String,
        saved_ms: u64,
    },
    /// A test binary actually ran and passed (now cached).
    TestRun {
        key: String,
        run_ms: u64,
    },
    /// Entry fetched from a peer store.
    RemoteHit {
        key: String,
        bytes: u64,
        fetch_ms: u64,
    },
    /// Entry served to a peer (keeps it warm for this store's LRU).
    Served {
        key: String,
    },
}

fn log_path(root: &Path) -> PathBuf {
    root.join("usage.log")
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n'], " ")
}

fn line(ts: u64, record: &Record) -> String {
    match record {
        Record::Hit { key, saved_ms } => format!("H\t{ts}\t{key}\t{saved_ms}\n"),
        Record::Miss {
            key,
            crate_name,
            size,
            compile_ms,
        } => format!(
            "M\t{ts}\t{key}\t{}\t{size}\t{compile_ms}\n",
            clean(crate_name)
        ),
        Record::Target { profile_dir } => {
            format!("T\t{ts}\t{}\n", clean(&profile_dir.to_string_lossy()))
        }
        Record::TestHit { key, saved_ms } => format!("TH\t{ts}\t{key}\t{saved_ms}\n"),
        Record::TestRun { key, run_ms } => format!("TR\t{ts}\t{key}\t{run_ms}\n"),
        Record::RemoteHit {
            key,
            bytes,
            fetch_ms,
        } => format!("RH\t{ts}\t{key}\t{bytes}\t{fetch_ms}\n"),
        Record::Served { key } => format!("S\t{ts}\t{key}\n"),
    }
}

pub fn append(root: &Path, record: &Record) {
    let text = line(super::now_secs(), record);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(root))
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Counters for one day (or a sum of days).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub hits: u64,
    pub misses: u64,
    /// Compile time avoided by hits.
    pub saved_ms: u64,
    /// Compile time spent on misses.
    pub compile_ms: u64,
    pub test_hits: u64,
    pub test_runs: u64,
    pub test_saved_ms: u64,
    pub remote_hits: u64,
    pub remote_bytes: u64,
}

impl Totals {
    fn add(&mut self, o: &Totals) {
        self.hits += o.hits;
        self.misses += o.misses;
        self.saved_ms += o.saved_ms;
        self.compile_ms += o.compile_ms;
        self.test_hits += o.test_hits;
        self.test_runs += o.test_runs;
        self.test_saved_ms += o.test_saved_ms;
        self.remote_hits += o.remote_hits;
        self.remote_bytes += o.remote_bytes;
    }
}

/// Daily counter columns, in `Totals` field order.
const DAILY_COUNTERS: [&str; 9] = [
    "hits",
    "misses",
    "saved_ms",
    "compile_ms",
    "test_hits",
    "test_runs",
    "test_saved_ms",
    "remote_hits",
    "remote_bytes",
];

fn as_row(t: &Totals) -> [i64; 9] {
    [
        t.hits,
        t.misses,
        t.saved_ms,
        t.compile_ms,
        t.test_hits,
        t.test_runs,
        t.test_saved_ms,
        t.remote_hits,
        t.remote_bytes,
    ]
    .map(|v| v.min(i64::MAX as u64) as i64)
}

/// One parsed log line. Fields added after the first release are optional
/// so logs written by an older wrapper still fold in.
#[derive(Debug, PartialEq, Eq)]
enum Parsed<'a> {
    Hit(&'a str),
    Miss {
        key: &'a str,
        crate_name: &'a str,
        size: i64,
    },
    Target(&'a str),
    Touch(&'a str),
    Other,
}

fn parse_line(text: &str) -> Option<(i64, Parsed<'_>, Totals)> {
    let f: Vec<&str> = text.split('\t').collect();
    let ts = f.get(1)?.parse::<i64>().ok()?;
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let mut t = Totals::default();
    let parsed = match (f[0], f.len()) {
        ("H", 3..) => {
            t.hits = 1;
            t.saved_ms = num(3);
            Parsed::Hit(f[2])
        }
        ("M", 5..) => {
            t.misses = 1;
            t.compile_ms = num(5);
            Parsed::Miss {
                key: f[2],
                crate_name: f[3],
                size: f[4].parse().unwrap_or(0),
            }
        }
        ("T", 3) => Parsed::Target(f[2]),
        ("TH", 4) => {
            t.test_hits = 1;
            t.test_saved_ms = num(3);
            Parsed::Other
        }
        ("TR", 4) => {
            t.test_runs = 1;
            Parsed::Other
        }
        ("RH", 5) => {
            t.remote_hits = 1;
            t.remote_bytes = num(3);
            Parsed::Touch(f[2])
        }
        ("S", 3) => Parsed::Touch(f[2]),
        _ => return None,
    };
    Some((ts, parsed, t))
}

/// Register an entry the index has never seen (fetched from a peer, pushed
/// to this store, imported) from its file, used at `ts`.
fn register_from_file(tx: &rusqlite::Transaction, root: &Path, key: &str, ts: i64) -> Result<()> {
    let shard = key.get(..2).unwrap_or("xx");
    let path = root.join("entries").join(shard).join(format!("{key}.json"));
    let Some(entry) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<super::Entry>(&b).ok())
    else {
        return Ok(());
    };
    tx.execute(
        "INSERT OR IGNORE INTO entries (key, crate_name, size, created, last_used)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            key,
            entry.crate_name,
            entry.size() as i64,
            entry.created as i64,
            ts
        ],
    )?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn open_index(root: &Path) -> Result<Connection> {
    let conn = Connection::open(root.join("index.sqlite"))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
             key TEXT PRIMARY KEY,
             crate_name TEXT NOT NULL,
             size INTEGER NOT NULL,
             created INTEGER NOT NULL,
             last_used INTEGER NOT NULL,
             hits INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS targets (
             profile_dir TEXT PRIMARY KEY,
             last_seen INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS daily (
             day INTEGER PRIMARY KEY,
             hits INTEGER NOT NULL DEFAULT 0,
             misses INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    // Columns added after the first release.
    // Two processes may race here right after an upgrade; losing is fine.
    let add_column = |table: &str, col: &str| -> Result<()> {
        if has_column(&conn, table, col)? {
            return Ok(());
        }
        match conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0;"
        )) {
            Err(e) if e.to_string().contains("duplicate column") => Ok(()),
            other => Ok(other?),
        }
    };
    add_column("entries", "saved_ms")?;
    for col in &DAILY_COUNTERS[2..] {
        add_column("daily", col)?;
    }
    Ok(conn)
}

/// Fold `usage.log` into the index. Caller holds the exclusive store lock,
/// so no wrapper is appending while the file is renamed away.
pub fn merge(root: &Path, conn: &mut Connection) -> Result<usize> {
    let path = log_path(root);
    let merging = root.join(format!("usage.{}.merging", std::process::id()));
    match std::fs::rename(&path, &merging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    }
    let text = std::fs::read_to_string(&merging)?;
    let tx = conn.transaction()?;
    let mut n = 0;
    let mut days: std::collections::BTreeMap<i64, Totals> = Default::default();
    for text_line in text.lines() {
        let Some((ts, parsed, totals)) = parse_line(text_line) else {
            continue;
        };
        days.entry(ts / 86_400).or_default().add(&totals);
        match parsed {
            Parsed::Hit(key) => {
                register_from_file(&tx, root, key, ts)?;
                tx.execute(
                    "UPDATE entries SET hits = hits + 1, saved_ms = saved_ms + ?3,
                     last_used = max(last_used, ?2) WHERE key = ?1",
                    params![key, ts, totals.saved_ms as i64],
                )?;
            }
            Parsed::Miss {
                key,
                crate_name,
                size,
            } => {
                tx.execute(
                    "INSERT INTO entries (key, crate_name, size, created, last_used)
                     VALUES (?1, ?2, ?3, ?4, ?4)
                     ON CONFLICT(key) DO UPDATE SET last_used = max(last_used, ?4)",
                    params![key, crate_name, size, ts],
                )?;
            }
            Parsed::Target(dir) => {
                tx.execute(
                    "INSERT INTO targets (profile_dir, last_seen) VALUES (?1, ?2)
                     ON CONFLICT(profile_dir) DO UPDATE SET last_seen = max(last_seen, ?2)",
                    params![dir, ts],
                )?;
            }
            Parsed::Touch(key) => {
                register_from_file(&tx, root, key, ts)?;
                tx.execute(
                    "UPDATE entries SET last_used = max(last_used, ?2) WHERE key = ?1",
                    params![key, ts],
                )?;
            }
            Parsed::Other => {}
        }
        n += 1;
    }
    let cols = DAILY_COUNTERS.join(", ");
    let marks = (2..=DAILY_COUNTERS.len() + 1)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sets = DAILY_COUNTERS
        .iter()
        .map(|c| format!("{c} = {c} + excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO daily (day, {cols}) VALUES (?1, {marks}) ON CONFLICT(day) DO UPDATE SET {sets}"
    );
    for (day, t) in &days {
        let row = as_row(t);
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![day];
        values.extend(row.iter().map(|v| v as &dyn rusqlite::ToSql));
        tx.execute(&sql, values.as_slice())?;
    }
    tx.commit()?;
    std::fs::remove_file(&merging)?;
    Ok(n)
}

/// Counters over the last `days` days: merged rows plus the not-yet-merged
/// log (so `status`/`stats` are right while builds hold the store lock).
pub fn totals(root: &Path, conn: &Connection, days: i64) -> Result<Totals> {
    let since = (super::now_secs() as i64) / 86_400 - days + 1;
    let sums = DAILY_COUNTERS
        .iter()
        .map(|c| format!("coalesce(sum({c}), 0)"))
        .collect::<Vec<_>>()
        .join(", ");
    let row: [i64; 9] = conn.query_row(
        &format!("SELECT {sums} FROM daily WHERE day >= ?1"),
        params![since],
        |r| {
            let mut out = [0i64; 9];
            for (i, v) in out.iter_mut().enumerate() {
                *v = r.get(i)?;
            }
            Ok(out)
        },
    )?;
    let [
        hits,
        misses,
        saved_ms,
        compile_ms,
        test_hits,
        test_runs,
        test_saved_ms,
        remote_hits,
        remote_bytes,
    ] = row.map(|v| v.max(0) as u64);
    let mut total = Totals {
        hits,
        misses,
        saved_ms,
        compile_ms,
        test_hits,
        test_runs,
        test_saved_ms,
        remote_hits,
        remote_bytes,
    };
    if let Ok(text) = std::fs::read_to_string(log_path(root)) {
        for text_line in text.lines() {
            if let Some((ts, _, t)) = parse_line(text_line)
                && ts / 86_400 >= since
            {
                total.add(&t);
            }
        }
    }
    Ok(total)
}

/// Hits and misses over the last `days` days.
pub fn hit_miss(root: &Path, conn: &Connection, days: i64) -> Result<(u64, u64)> {
    let t = totals(root, conn, days)?;
    Ok((t.hits, t.misses))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_counts_and_registers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        append(
            root,
            &Record::Miss {
                key: "k1".into(),
                crate_name: "serde".into(),
                size: 10,
                compile_ms: 800,
            },
        );
        append(
            root,
            &Record::Hit {
                key: "k1".into(),
                saved_ms: 700,
            },
        );
        append(
            root,
            &Record::Hit {
                key: "k1".into(),
                saved_ms: 700,
            },
        );
        append(
            root,
            &Record::Target {
                profile_dir: "/w/t/debug".into(),
            },
        );
        let mut conn = open_index(root).unwrap();
        assert_eq!(hit_miss(root, &conn, 7).unwrap(), (2, 1));
        assert_eq!(merge(root, &mut conn).unwrap(), 4);
        assert_eq!(hit_miss(root, &conn, 7).unwrap(), (2, 1));
        let hits: i64 = conn
            .query_row("SELECT hits FROM entries WHERE key='k1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 2);
        let targets: i64 = conn
            .query_row("SELECT count(*) FROM targets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(targets, 1);
        assert_eq!(merge(root, &mut conn).unwrap(), 0);
    }

    #[test]
    fn totals_cover_new_records_and_old_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let now = crate::store::now_secs();
        // Lines from a wrapper that predates the time fields.
        std::fs::write(
            root.join("usage.log"),
            format!("M\t{now}\tk0\told\t5\nH\t{now}\tk0\n"),
        )
        .unwrap();
        append(
            root,
            &Record::Hit {
                key: "k0".into(),
                saved_ms: 1500,
            },
        );
        append(
            root,
            &Record::TestHit {
                key: "t1".into(),
                saved_ms: 2000,
            },
        );
        append(
            root,
            &Record::TestRun {
                key: "t2".into(),
                run_ms: 300,
            },
        );
        append(
            root,
            &Record::RemoteHit {
                key: "k0".into(),
                bytes: 4096,
                fetch_ms: 12,
            },
        );
        append(root, &Record::Served { key: "k0".into() });
        let mut conn = open_index(root).unwrap();
        let before = totals(root, &conn, 1).unwrap();
        assert_eq!(merge(root, &mut conn).unwrap(), 7);
        let after = totals(root, &conn, 1).unwrap();
        assert_eq!(before, after);
        assert_eq!(after.hits, 2);
        assert_eq!(after.misses, 1);
        assert_eq!(after.saved_ms, 1500);
        assert_eq!(after.test_hits, 1);
        assert_eq!(after.test_saved_ms, 2000);
        assert_eq!(after.test_runs, 1);
        assert_eq!(after.remote_hits, 1);
        assert_eq!(after.remote_bytes, 4096);
        let saved: i64 = conn
            .query_row("SELECT saved_ms FROM entries WHERE key='k0'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(saved, 1500);
    }

    #[test]
    fn old_index_gains_new_columns() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("index.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE entries (key TEXT PRIMARY KEY, crate_name TEXT NOT NULL, size INTEGER NOT NULL,
               created INTEGER NOT NULL, last_used INTEGER NOT NULL, hits INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE daily (day INTEGER PRIMARY KEY, hits INTEGER NOT NULL DEFAULT 0,
               misses INTEGER NOT NULL DEFAULT 0);
             INSERT INTO daily (day, hits, misses) VALUES (1, 3, 4);",
        )
        .unwrap();
        drop(conn);
        let conn = open_index(dir.path()).unwrap();
        assert!(has_column(&conn, "daily", "test_saved_ms").unwrap());
        assert!(has_column(&conn, "entries", "saved_ms").unwrap());
        // Reopening is a no-op.
        drop(conn);
        open_index(dir.path()).unwrap();
    }

    #[test]
    fn hits_register_entries_the_index_never_saw() {
        // An entry fetched from a peer: on disk, built long ago, no M line.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path(), crate::config::CowMode::Copy).unwrap();
        let key = "ab".repeat(32);
        store
            .write_entry(&crate::store::Entry {
                version: crate::store::ENTRY_VERSION,
                key: key.clone(),
                tier1: "cd".repeat(32),
                crate_name: "serde".into(),
                outputs: Vec::new(),
                dep_info: None,
                stdout: String::new(),
                stderr: String::new(),
                created: 1000,
                texts: Vec::new(),
                symlinks: Vec::new(),
                dirs: Vec::new(),
                compile_ms: 900,
            })
            .unwrap();
        append(
            dir.path(),
            &Record::Hit {
                key: key.clone(),
                saved_ms: 800,
            },
        );
        let mut conn = open_index(dir.path()).unwrap();
        merge(dir.path(), &mut conn).unwrap();
        let (last_used, hits): (i64, i64) = conn
            .query_row(
                "SELECT last_used, hits FROM entries WHERE key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(hits, 1);
        assert!(
            last_used > 1000,
            "last use is the hit, not the remote build"
        );
    }
}
