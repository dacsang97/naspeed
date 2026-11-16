//! What each rustc job needed last time. After every compile the wrapper
//! records the child's peak resident size, keyed by the job's shape (crate,
//! test harness or not, full build or metadata only, opt level). Admission
//! reserves that much before the next run of the same job starts.
//!
//! Kept machine-wide in `<config dir>/rustc-memory.sqlite`: memory is a
//! machine resource, unlike the per-volume artifact stores.

use std::ffi::OsString;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

/// Reserved for a job never seen before. Most crates need far less; the big
/// ones are learned after their first build.
pub const DEFAULT_ESTIMATE_BYTES: u64 = 512 * 1024 * 1024;

/// Identity of a rustc job for memory purposes. `None` when the arguments
/// carry no crate name (`rustc -vV` probes and the like).
pub fn job_key(args: &[OsString]) -> Option<String> {
    let mut crate_name = None;
    let mut test = false;
    let mut link = false;
    let mut opt = "0".to_owned();
    let mut it = args.iter().map(|a| a.to_string_lossy());
    while let Some(arg) = it.next() {
        match arg.as_ref() {
            "--crate-name" => crate_name = it.next().map(|v| v.into_owned()),
            "--test" => test = true,
            "-C" => {
                if let Some(v) = it.next()
                    && let Some(level) = v.strip_prefix("opt-level=")
                {
                    opt = level.to_owned();
                }
            }
            a => {
                if let Some(emit) = a.strip_prefix("--emit=") {
                    link = emit.split(',').any(|k| k == "link");
                } else if let Some(level) = a.strip_prefix("-Copt-level=") {
                    opt = level.to_owned();
                }
            }
        }
    }
    let kind = if test { "test" } else { "lib" };
    let emit = if link { "link" } else { "meta" };
    crate_name.map(|c| format!("{c}:{kind}:{emit}:o{opt}"))
}

fn open(config_dir: &Path) -> rusqlite::Result<Connection> {
    let _ = std::fs::create_dir_all(config_dir);
    let conn = Connection::open(config_dir.join("rustc-memory.sqlite"))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS peaks (
             key TEXT PRIMARY KEY,
             peak_bytes INTEGER NOT NULL,
             samples INTEGER NOT NULL DEFAULT 1,
             updated INTEGER NOT NULL
         );",
    )?;
    Ok(conn)
}

/// Learned peak for `key`, or the default when unknown or unreadable.
pub fn estimate(config_dir: &Path, key: Option<&str>) -> u64 {
    key.and_then(|k| {
        open(config_dir)
            .ok()?
            .query_row("SELECT peak_bytes FROM peaks WHERE key = ?1", [k], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .ok()
            .flatten()
    })
    .map(|b| b.max(0) as u64)
    .unwrap_or(DEFAULT_ESTIMATE_BYTES)
}

/// New stored peak: jump straight up to a bigger sample (under-reserving is
/// what causes swapping), decay slowly towards a smaller one (a crate that
/// shrank should eventually stop over-reserving).
pub fn blend(old: u64, sample: u64) -> u64 {
    if sample >= old {
        sample
    } else {
        old - (old - sample) / 8
    }
}

/// Remember `sample` for `key`. Failures are ignored: learning is an
/// optimisation, never a reason to fail a build.
pub fn record(config_dir: &Path, key: &str, sample: u64) {
    let result = (|| -> rusqlite::Result<()> {
        let conn = open(config_dir)?;
        let old: Option<i64> = conn
            .query_row("SELECT peak_bytes FROM peaks WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        let new = old.map_or(sample, |o| blend(o.max(0) as u64, sample));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO peaks (key, peak_bytes, samples, updated) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(key) DO UPDATE SET peak_bytes = ?2, samples = samples + 1, updated = ?3",
            params![key, new as i64, now],
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::debug!(key, "rustc peak not recorded: {e}");
    }
}

/// The `n` jobs with the largest learned peaks, for `naspeed status`.
pub fn heaviest(config_dir: &Path, n: usize) -> Vec<(String, u64)> {
    let Ok(conn) = open(config_dir) else {
        return Vec::new();
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT key, peak_bytes FROM peaks ORDER BY peak_bytes DESC LIMIT ?1")
    else {
        return Vec::new();
    };
    stmt.query_map([n as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// Peak resident size of a child that has been waited for.
///
/// Unix: `RUSAGE_CHILDREN` is the largest peak of all waited-for children
/// of this process (and their waited-for descendants, so the linker rustc
/// runs is included). The wrapper's only sizeable child is rustc.
#[cfg(unix)]
pub fn child_peak_bytes(_child: &Child) -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return None;
    }
    let max = usage.ru_maxrss.max(0) as u64;
    // Bytes on macOS, KiB elsewhere.
    Some(if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    })
}

/// Windows: the peak working set of the exited child, read from its handle.
#[cfg(windows)]
pub fn child_peak_bytes(child: &Child) -> Option<u64> {
    super::memory_probe::child_working_set(child).map(|(_, peak)| peak)
}

#[cfg(not(any(unix, windows)))]
pub fn child_peak_bytes(_child: &Child) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn key_separates_test_link_and_opt_level() {
        let lib = args(&[
            "--crate-name",
            "api",
            "--emit=dep-info,metadata,link",
            "-C",
            "opt-level=3",
        ]);
        assert_eq!(job_key(&lib).as_deref(), Some("api:lib:link:o3"));
        let test = args(&["--crate-name", "api", "--emit=dep-info,link", "--test"]);
        assert_eq!(job_key(&test).as_deref(), Some("api:test:link:o0"));
        let check = args(&[
            "--crate-name",
            "api",
            "--emit=dep-info,metadata",
            "-Copt-level=1",
        ]);
        assert_eq!(job_key(&check).as_deref(), Some("api:lib:meta:o1"));
        assert_eq!(job_key(&args(&["-vV"])), None);
    }

    #[test]
    fn blend_rises_at_once_and_decays_slowly() {
        assert_eq!(blend(100, 300), 300);
        assert_eq!(blend(800, 0), 700);
        assert_eq!(blend(800, 800), 800);
    }

    #[test]
    fn records_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            estimate(dir.path(), Some("x:lib:link:o0")),
            DEFAULT_ESTIMATE_BYTES
        );
        assert_eq!(estimate(dir.path(), None), DEFAULT_ESTIMATE_BYTES);
        record(dir.path(), "x:lib:link:o0", 1000);
        record(dir.path(), "x:lib:link:o0", 200);
        assert_eq!(estimate(dir.path(), Some("x:lib:link:o0")), 900);
        record(dir.path(), "y:lib:link:o0", 5000);
        let top = heaviest(dir.path(), 1);
        assert_eq!(top, vec![("y:lib:link:o0".to_owned(), 5000)]);
    }

    #[cfg(unix)]
    #[test]
    fn child_peak_is_measured() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "true"])
            .spawn()
            .unwrap();
        child.wait().unwrap();
        assert!(child_peak_bytes(&child).is_some_and(|b| b > 0));
    }
}
