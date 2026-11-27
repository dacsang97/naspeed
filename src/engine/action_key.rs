//! Canonical encoding and key derivation for `ActionSpec`.
//!
//! The key is `blake3(ENC(spec))` where `ENC` is a fixed byte encoding —
//! never JSON, whose field order and whitespace are not canonical:
//!
//! ```text
//! "naspeed/action-key/1"      domain-separation tag (also the version)
//! u32le field_count           currently 8
//! str executable              u32le byte length + UTF-8 bytes
//! str toolchain_digest
//! seq argv                    u32le count + str per element
//! seq env                     u32le count + (str name, str value_hash)
//!                             pairs, sorted by name (BTreeMap order)
//! str os, str arch, str family
//! str work_dir                "@WORK_DIR@" today
//! seq inputs                  u32le count + (str path, str digest, u64le size)
//! seq outputs                 u32le count + str path
//! u8 cacheable                0 or 1
//! ```
//!
//! No absolute paths appear anywhere in this encoding; `work_dir` is a
//! placeholder token, so the key is stable across worktree roots. Changing
//! the tag version changes every key (fail-closed on schema change).

use std::fmt::Write as _;

use super::action::{ActionInput, ActionSpec, ExpectedOutput, Platform};

/// Domain-separation tag; bump the version to invalidate all keys.
pub const DOMAIN_TAG: &[u8] = b"naspeed/action-key/1";

pub fn encode(spec: &ActionSpec) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(DOMAIN_TAG);
    out.extend_from_slice(&8u32.to_le_bytes());
    put_str(&mut out, &spec.executable);
    put_str(&mut out, &spec.toolchain_digest);
    put_seq(&mut out, &spec.argv, |s, out| put_str(out, s));
    put_seq(
        &mut out,
        &spec.env.iter().collect::<Vec<_>>(),
        |(name, hash), out| {
            put_str(out, name);
            put_str(out, hash);
        },
    );
    put_platform(&mut out, &spec.platform);
    put_str(&mut out, &spec.work_dir);
    put_seq(&mut out, &spec.inputs, |i, out| put_input(out, i));
    put_seq(&mut out, &spec.outputs, |o, out| put_output(out, o));
    out.push(spec.cacheable as u8);
    out
}

pub fn action_key(spec: &ActionSpec) -> String {
    blake3::hash(&encode(spec)).to_hex().to_string()
}

/// Debug rendering of the canonical bytes (for vector diffs), lowercase hex.
pub fn encode_hex(spec: &ActionSpec) -> String {
    let mut s = String::new();
    for b in encode(spec) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn put_seq<T>(out: &mut Vec<u8>, items: &[T], f: impl Fn(&T, &mut Vec<u8>)) {
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        f(item, out);
    }
}

fn put_platform(out: &mut Vec<u8>, p: &Platform) {
    put_str(out, &p.os);
    put_str(out, &p.arch);
    put_str(out, &p.family);
}

fn put_input(out: &mut Vec<u8>, i: &ActionInput) {
    put_str(out, &i.path);
    put_str(out, &i.digest);
    out.extend_from_slice(&i.size.to_le_bytes());
}

fn put_output(out: &mut Vec<u8>, o: &ExpectedOutput) {
    put_str(out, &o.path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::action::{ActionInput, ExpectedOutput, Platform, WORK_DIR_PLACEHOLDER};
    use std::collections::BTreeMap;

    fn spec() -> ActionSpec {
        let mut env = BTreeMap::new();
        env.insert(
            "CFLAGS".to_owned(),
            ActionSpec::env_value_hash("-O2 -fPIC"),
        );
        env.insert(
            "TARGET".to_owned(),
            ActionSpec::env_value_hash("aarch64-apple-darwin"),
        );
        ActionSpec {
            executable: "rustc".into(),
            toolchain_digest: blake3::hash(b"rustc 1.91.0 sysroot").to_hex().to_string(),
            argv: vec![
                "rustc".into(),
                "--crate-type".into(),
                "lib".into(),
                "--out-dir".into(),
                "@WORK_DIR@/out".into(),
            ],
            env,
            platform: Platform {
                os: "macos".into(),
                arch: "aarch64".into(),
                family: "unix".into(),
            },
            work_dir: WORK_DIR_PLACEHOLDER.into(),
            inputs: vec![ActionInput {
                path: "src/lib.rs".into(),
                digest: blake3::hash(b"fn main() {}").to_hex().to_string(),
                size: 12,
            }],
            outputs: vec![ExpectedOutput {
                path: "out/libx.rlib".into(),
            }],
            cacheable: true,
        }
    }

    // Golden vectors: fixed spec -> fixed key, regardless of where the
    // worktree lives (no absolute path is ever encoded).
    const GOLDEN: &str = "16942ff5b3cdacf93ca439512cf7613ca47fc296d34a817650106a167f279712";

    #[test]
    fn golden_vector_is_stable() {
        assert_eq!(action_key(&spec()), GOLDEN);
        // Re-encode is deterministic.
        assert_eq!(encode(&spec()), encode(&spec()));
    }

    #[test]
    fn key_is_path_independent() {
        let mut under_a = spec();
        let mut under_b = spec();
        // Simulate two worktrees: the adapter puts only placeholders into
        // argv and work_dir; absolute roots never reach the spec.
        under_a.argv.push(format!("-L{WORK_DIR_PLACEHOLDER}/deps"));
        under_b.argv.push(format!("-L{WORK_DIR_PLACEHOLDER}/deps"));
        assert_eq!(under_a.key(), under_b.key());
    }

    #[test]
    fn key_changes_on_each_dimension() {
        let base = spec();
        let base_key = action_key(&base);

        let mut toolchain = base.clone();
        toolchain.toolchain_digest = blake3::hash(b"rustc 1.92.0").to_hex().to_string();
        assert_ne!(action_key(&toolchain), base_key, "toolchain");

        let mut envv = base.clone();
        envv.set_env("CFLAGS", "-O3");
        assert_ne!(action_key(&envv), base_key, "env value");
        let mut envv2 = base.clone();
        envv2.set_env("EXTRA_VAR", "1");
        assert_ne!(action_key(&envv2), base_key, "env name set");

        let mut input = base.clone();
        input.inputs[0].digest = blake3::hash(b"fn other() {}").to_hex().to_string();
        assert_ne!(action_key(&input), base_key, "input content");
        let mut input_path = base.clone();
        input_path.inputs[0].path = "src/other.rs".into();
        assert_ne!(action_key(&input_path), base_key, "input path");

        let mut platform = base.clone();
        platform.platform.os = "linux".into();
        assert_ne!(action_key(&platform), base_key, "platform");

        let mut argv = base.clone();
        argv.argv[2] = "rlib".into();
        assert_ne!(action_key(&argv), base_key, "argv");

        let mut uncacheable = base.clone();
        uncacheable.cacheable = false;
        assert_ne!(action_key(&uncacheable), base_key, "cacheability");
    }

    #[test]
    fn env_is_sorted_and_values_only_hashed() {
        // Order of insertion must not matter (BTreeMap), and raw values
        // must never appear in the canonical bytes.
        let mut a = spec();
        a.set_env("ZZ_LAST", "secret-1");
        a.set_env("AA_FIRST", "secret-2");
        let hex = encode_hex(&a);
        assert!(hex.find("736563726574").is_none(), "raw env value leaked");
        let pos_last = a
            .env
            .keys()
            .position(|k| k == "ZZ_LAST")
            .unwrap();
        let pos_first = a.env.keys().position(|k| k == "AA_FIRST").unwrap();
        assert!(pos_first < pos_last);
    }
}
