//! Machine-wide limits on concurrently running rustc processes, on top of
//! each cargo's own jobserver (which rustc keeps honouring): a fixed number
//! of slots, and memory admission that reserves each job's learned peak
//! before it starts (see `admission`).

pub mod admission;
pub mod memory_probe;
pub mod peak_memory;
pub mod slot_pool;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use crate::config::Settings;

/// Held while a real rustc runs (passthrough or miss); hits never take one.
pub struct RustcPermit {
    // Field order matters: the reservation file goes before the slot lock
    // is released, so nobody reads it as belonging to the next holder.
    reservation: Option<ReservationFile>,
    _slot: Option<slot_pool::Slot>,
    config_dir: PathBuf,
    job_key: Option<String>,
}

struct ReservationFile {
    dir: PathBuf,
    slot: usize,
    estimate: u64,
}

impl Drop for ReservationFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(admission::reservation_path(&self.dir, self.slot));
    }
}

impl RustcPermit {
    /// Tell the other wrappers which process holds this reservation, so they
    /// count only what it has not allocated yet.
    pub fn started(&self, pid: u32) {
        if let Some(r) = &self.reservation {
            let reservation = admission::Reservation {
                pid,
                estimate: r.estimate,
            };
            admission::write_reservation(&r.dir, r.slot, reservation);
        }
    }

    /// Learn this job's peak from its exited child for next time.
    pub fn finished(&self, child: &Child) {
        if let (Some(key), Some(peak)) = (&self.job_key, peak_memory::child_peak_bytes(child)) {
            peak_memory::record(&self.config_dir, key, peak);
        }
    }
}

/// Wait for a slot, then for memory admission. `args` are the rustc
/// arguments; they identify the job whose learned peak is reserved.
pub fn acquire(settings: &Settings, args: &[OsString]) -> RustcPermit {
    let job_key = peak_memory::job_key(args);
    let dir = settings.slots_dir();
    let slot = slot_pool::acquire(&dir, settings.slots);
    let mut reservation = None;
    // `min_free_mem_mb = 0` turns memory admission off.
    if let Some(slot) = &slot
        && settings.min_free_mem_mb > 0
    {
        // Our slot is ours now; whatever its previous holder left is stale.
        let _ = std::fs::remove_file(admission::reservation_path(&dir, slot.index));
        let estimate = peak_memory::estimate(&settings.config_dir, job_key.as_deref());
        let floor = settings.min_free_mem_mb.saturating_mul(1024 * 1024);
        let waited = admission::wait_admitted(&dir, settings.slots, slot.index, estimate, floor);
        if waited >= Duration::from_millis(100) {
            tracing::info!(waited_ms = waited.as_millis() as u64, job = ?job_key, "memory admission wait");
        }
        reservation = Some(ReservationFile {
            dir: dir.clone(),
            slot: slot.index,
            estimate,
        });
    }
    RustcPermit {
        reservation,
        _slot: slot,
        config_dir: settings.config_dir.clone(),
        job_key,
    }
}
