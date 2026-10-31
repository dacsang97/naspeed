//! Machine-wide limit on concurrently running rustc processes. This is on
//! top of each cargo's own jobserver, which rustc keeps honouring.

pub mod memory_guard;
pub mod slot_pool;

use std::time::Duration;

use crate::config::Settings;

/// Held while a real rustc runs (passthrough or miss); hits never take one.
pub struct RustcPermit {
    _slot: Option<slot_pool::Slot>,
}

pub fn acquire(settings: &Settings) -> RustcPermit {
    let slot = slot_pool::acquire(&settings.slots_dir(), settings.slots);
    // Check memory after getting the slot: the slot holder is the one about
    // to allocate, and waiting without a slot would let others jump ahead.
    let waited = memory_guard::wait_for_memory(settings.min_free_mem_mb, Duration::from_secs(30));
    if waited >= Duration::from_millis(100) {
        tracing::info!(waited_ms = waited.as_millis() as u64, "memory guard wait");
    }
    RustcPermit { _slot: slot }
}
