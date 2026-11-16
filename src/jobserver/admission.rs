//! Memory admission: a rustc starts only when the memory it is expected to
//! need fits next to what already-running rustc are still expected to grab.
//!
//! A one-shot "is there enough free memory?" check admits every wrapper that
//! starts in the same instant, because none of them has allocated yet. So
//! each admitted rustc leaves a reservation next to its slot lock
//! (`<slots>/<i>.res`: child pid and estimate), and decisions are taken one
//! at a time under `<slots>/admit.lock`. A reservation counts only while its
//! slot lock is held, so a killed wrapper never leaves one behind.
//!
//! Progress is guaranteed: with no other admitted rustc running, the next
//! one starts whatever the memory state (there is nothing to wait for).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{memory_probe, slot_pool};
use crate::store::key_lock::{self, Mode};

/// One admitted rustc, as seen by the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// The rustc child, 0 until it is spawned.
    pub pid: u32,
    pub estimate: u64,
}

pub fn reservation_path(dir: &Path, slot: usize) -> PathBuf {
    dir.join(format!("{slot}.res"))
}

fn parse(text: &str) -> Option<Reservation> {
    let mut it = text.split_whitespace();
    Some(Reservation {
        pid: it.next()?.parse().ok()?,
        estimate: it.next()?.parse().ok()?,
    })
}

pub fn write_reservation(dir: &Path, slot: usize, r: Reservation) {
    let _ = std::fs::write(
        reservation_path(dir, slot),
        format!("{} {}\n", r.pid, r.estimate),
    );
}

/// Reservations of slots other than `mine` whose holder is alive. Stale files
/// (slot no longer held) are removed.
pub fn live_reservations(dir: &Path, slots: usize, mine: Option<usize>) -> Vec<Reservation> {
    let mut out = Vec::new();
    for i in (0..slots).filter(|i| Some(*i) != mine) {
        let path = reservation_path(dir, i);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match key_lock::try_lock(&slot_pool::slot_path(dir, i), Mode::Exclusive) {
            Ok(None) => {
                // Held: a default-sized reservation if the file is mid-write.
                out.push(parse(&text).unwrap_or(Reservation {
                    pid: 0,
                    estimate: super::peak_memory::DEFAULT_ESTIMATE_BYTES,
                }));
            }
            Ok(Some(_free)) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
    out
}

/// Memory the admitted rustc are still expected to take: each one's
/// estimate minus what it already holds (already out of "available").
pub fn outstanding(others: &[Reservation], rss: impl Fn(u32) -> Option<u64>) -> u64 {
    others
        .iter()
        .map(|r| {
            let used = if r.pid == 0 {
                0
            } else {
                rss(r.pid).unwrap_or(0)
            };
            r.estimate.saturating_sub(used)
        })
        .sum()
}

/// The admission rule, free of OS calls so it can be tested.
pub fn fits(
    others_running: bool,
    pressure: bool,
    available: Option<u64>,
    outstanding: u64,
    estimate: u64,
    floor: u64,
) -> bool {
    if !others_running {
        return true;
    }
    if pressure {
        return false;
    }
    match available {
        Some(avail) => avail >= outstanding.saturating_add(estimate).saturating_add(floor),
        // No reading on this platform: the slot count is the only limit.
        None => true,
    }
}

/// Block until this rustc may start, then leave its reservation. Returns
/// how long it waited.
pub fn wait_admitted(dir: &Path, slots: usize, slot: usize, estimate: u64, floor: u64) -> Duration {
    let start = Instant::now();
    let mut delay = Duration::from_millis(100);
    let mut next_log = Duration::from_secs(30);
    loop {
        let decision = {
            let _admit = key_lock::lock(&dir.join("admit.lock"), Mode::Exclusive).ok();
            let others = live_reservations(dir, slots, Some(slot));
            let pending = outstanding(&others, memory_probe::process_rss_bytes);
            let pressure = !others.is_empty() && memory_probe::under_pressure();
            let available = memory_probe::available_bytes();
            let ok = fits(
                !others.is_empty(),
                pressure,
                available,
                pending,
                estimate,
                floor,
            );
            if ok {
                write_reservation(dir, slot, Reservation { pid: 0, estimate });
            }
            (ok, others.len(), pressure, available, pending)
        };
        let (ok, running, pressure, available, pending) = decision;
        if ok {
            return start.elapsed();
        }
        let waited = start.elapsed();
        if waited >= next_log {
            tracing::info!(
                waited_s = waited.as_secs(),
                running,
                pressure,
                available_mb = available.map(|a| a / (1024 * 1024)),
                outstanding_mb = pending / (1024 * 1024),
                estimate_mb = estimate / (1024 * 1024),
                "waiting for memory"
            );
            next_log += Duration::from_secs(30);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn alone_always_runs() {
        assert!(fits(false, true, Some(0), 0, 4 * GB, GB));
    }

    #[test]
    fn pressure_blocks_when_others_run() {
        assert!(!fits(true, true, Some(64 * GB), 0, GB, GB));
    }

    #[test]
    fn reservations_of_running_jobs_count() {
        // 3 GiB available, one 1.2 GiB job that has not allocated yet.
        let others = [Reservation {
            pid: 0,
            estimate: 1200 * 1024 * 1024,
        }];
        let pending = outstanding(&others, |_| None);
        assert!(!fits(
            true,
            false,
            Some(3 * GB),
            pending,
            1200 * 1024 * 1024,
            GB
        ));
        // Once it holds 1 GiB, only 200 MiB of it is still to come.
        let others = [Reservation {
            pid: 7,
            estimate: 1200 * 1024 * 1024,
        }];
        let pending = outstanding(&others, |_| Some(GB));
        assert_eq!(pending, 176 * 1024 * 1024);
        assert!(fits(true, false, Some(3 * GB), pending, GB, GB));
    }

    #[test]
    fn unknown_platform_falls_back_to_slots() {
        assert!(fits(true, false, None, 10 * GB, GB, GB));
    }

    #[test]
    fn reservations_live_only_while_their_slot_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let held = slot_pool::acquire(dir.path(), 3).unwrap();
        let r = Reservation {
            pid: 42,
            estimate: 7,
        };
        write_reservation(dir.path(), held.index, r);
        // A file whose slot nobody holds is stale.
        let free = (0..3).find(|i| *i != held.index).unwrap();
        write_reservation(dir.path(), free, r);
        assert_eq!(live_reservations(dir.path(), 3, None), vec![r]);
        assert!(!reservation_path(dir.path(), free).exists());
        // Our own slot is never counted against ourselves.
        assert!(live_reservations(dir.path(), 3, Some(held.index)).is_empty());
    }

    #[test]
    fn second_job_waits_until_first_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let a = slot_pool::acquire(dir.path(), 2).unwrap();
        // Nothing else running: admitted at once despite an absurd floor.
        wait_admitted(dir.path(), 2, a.index, GB, u64::MAX / 4);
        let path = dir.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            let b = slot_pool::acquire(&path, 2).unwrap();
            wait_admitted(&path, 2, b.index, GB, u64::MAX / 4)
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(!waiter.is_finished());
        drop(a);
        let waited = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(300));
    }
}
