//! `ActionSpec`: everything that determines an action's key.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Working directory is keyed as a fixed placeholder, never an absolute
/// path, so the same logical action keys identically under any worktree.
pub const WORK_DIR_PLACEHOLDER: &str = "@WORK_DIR@";

/// One input the action reads, already content-identified by the adapter.
/// `path` is logical (relative, `/`-separated), never absolute.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionInput {
    pub path: String,
    /// blake3 hex of the input's content.
    pub digest: String,
    pub size: u64,
}

/// One output the action is expected to produce, relative to the working
/// directory. Checked after the run; the recorded tree is authoritative.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedOutput {
    pub path: String,
}

/// The platform the action ran (or will run) on. Part of the key: results
/// do not transfer across platforms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    /// Anything else that changes semantics (libc, ABI, kernel family);
    /// empty when nothing applies.
    pub family: String,
}

impl Platform {
    pub fn current() -> Platform {
        Platform {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            family: std::env::consts::FAMILY.to_owned(),
        }
    }
}

/// One cacheable execution. Every field feeds the canonical encoding; the
/// adapter is responsible for completeness (anything the executable could
/// observe must appear here or the action must be `cacheable = false`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// Program run: argv[0] as invoked (not resolved to an absolute path).
    pub executable: String,
    /// Content identity of the toolchain: blake3 hex over whatever the
    /// adapter considers the toolchain (binary, sysroot, version stamp).
    /// Two toolchains producing identical argv still get different keys.
    pub toolchain_digest: String,
    /// Canonical argv: relative paths, placeholders for work-dir paths;
    /// adapters must have already removed anything host-specific.
    pub argv: Vec<String>,
    /// Environment allowlist, sorted by name. Values are secret on many
    /// machines, so only their blake3 hex is stored or keyed: the record
    /// never contains the value.
    pub env: BTreeMap<String, String>,
    pub platform: Platform,
    /// Always `WORK_DIR_PLACEHOLDER`; kept as a field so a future schema
    /// can name further roots without ambiguity.
    pub work_dir: String,
    pub inputs: Vec<ActionInput>,
    pub outputs: Vec<ExpectedOutput>,
    /// When false the action may run but its result is never stored.
    pub cacheable: bool,
}

impl ActionSpec {
    /// Hash `value` the way `env` expects: callers keep the value local.
    pub fn env_value_hash(value: &str) -> String {
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    /// Set an allowlisted variable, hashing its value in place.
    pub fn set_env(&mut self, name: impl Into<String>, value: &str) {
        self.env.insert(name.into(), Self::env_value_hash(value));
    }

    /// blake3 key for this action (canonical encoding, domain-separated).
    pub fn key(&self) -> String {
        crate::engine::action_key::action_key(self)
    }
}
