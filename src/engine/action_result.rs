//! `ActionResult`: the versioned record stored under `actions/<2>/<key>.json`.
//! Untrusted bytes on read — every referenced object is verified before the
//! record is visible as a hit.

use serde::{Deserialize, Serialize};

/// Bump on any schema change; unknown versions are a typed cache miss.
pub const ACTION_RESULT_VERSION: u32 = 1;

/// Captured stream, stored as a store object so large output stays off the
/// record's critical path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamRef {
    /// blake3 hex of the object's content; empty for a missing stream.
    pub digest: String,
    pub size: u64,
}

/// Who produced the record and how much to trust it. Mirrors the existing
/// main-write/PR-read trust policy; provenance is descriptive, never a
/// substitute for object verification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// "local", "ci:<pipeline id>", "peer:<peer id>", …
    pub producer: String,
    /// "trusted" (written on a trusted ref/branch) or "untrusted".
    pub trust: String,
    /// Hostname/user string for debugging; may be empty.
    pub origin: String,
}

/// Observed resource use, informational only (never part of key or hit).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub peak_rss_bytes: u64,
    pub user_cpu_ms: u64,
    pub system_cpu_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionResult {
    pub version: u32,
    /// The `ActionSpec` key this record answers; must match the store path.
    pub action_key: String,
    /// Process exit code; negative when killed by a signal on Unix.
    pub exit_status: i32,
    pub stdout: Option<StreamRef>,
    pub stderr: Option<StreamRef>,
    /// blake3 hex of the output tree record under `trees/`; empty when the
    /// action produced no outputs.
    pub output_tree: String,
    /// Wall-clock duration of the execution.
    pub duration_ms: u64,
    #[serde(default)]
    pub resources: Resources,
    pub provenance: Provenance,
    /// Unix seconds when the record was committed.
    pub created: u64,
}

impl ActionResult {
    /// A hit requires a record that validated *and* a successful exit.
    /// Failed actions stay observable (the record is kept) but never hit.
    pub fn is_hit(&self) -> bool {
        self.exit_status == 0
    }
}
