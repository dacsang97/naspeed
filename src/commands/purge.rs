//! `naspeed purge --target <dir>`: delete files naspeed restored from the
//! store (marked by xattr) plus their cargo fingerprints, so the next build
//! recompiles those crates with plain rustc. `NASPEED_DISABLE=1` alone does
//! not do this: cargo does not fingerprint the wrapper, so restored
//! artifacts would stay `Fresh`.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, bail};

use crate::cli::PurgeArgs;
use crate::store::physical_usage::human;
use crate::wrapper::extern_identity::is_placed;

/// `-<16 hex>` metadata hash from an artifact name like `libserde-0123…cdef.rlib`.
fn unit_hash(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

fn walk_deps_dirs(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == "deps" {
            out.push(path);
        } else if entry.file_name() != "incremental" && entry.file_name() != "build" {
            walk_deps_dirs(&path, out);
        }
    }
}

pub fn run(args: PurgeArgs) -> Result<u8> {
    if !args.target.is_dir() {
        bail!("{} is not a directory", args.target.display());
    }
    let mut deps_dirs = Vec::new();
    walk_deps_dirs(&args.target, &mut deps_dirs);
    let (mut files, mut bytes, mut fingerprints) = (0u64, 0u64, 0u64);
    for deps in deps_dirs {
        let mut hashes = BTreeSet::new();
        for entry in std::fs::read_dir(&deps)?.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_file()) || !is_placed(&path) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(h) = unit_hash(&name) {
                hashes.insert(h.to_owned());
            }
            files += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if !args.dry_run {
                std::fs::remove_file(&path)?;
            }
        }
        // Without its fingerprint cargo would still consider the unit fresh
        // if a stale output of the same name came back.
        let Some(profile) = deps.parent() else {
            continue;
        };
        let fp_dir = profile.join(".fingerprint");
        if let Ok(entries) = std::fs::read_dir(&fp_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if unit_hash(&name).is_some_and(|h| hashes.contains(h)) {
                    fingerprints += 1;
                    if !args.dry_run {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
    println!(
        "{} {files} restored files ({}) and {fingerprints} fingerprints under {}",
        if args.dry_run {
            "would remove"
        } else {
            "removed"
        },
        human(bytes),
        args.target.display()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_unit_hash() {
        assert_eq!(
            unit_hash("libserde-0123456789abcdef.rlib"),
            Some("0123456789abcdef")
        );
        assert_eq!(
            unit_hash("serde-0123456789abcdef.d"),
            Some("0123456789abcdef")
        );
        assert_eq!(
            unit_hash("serde_json-279e86eba95db35d.serde_json.57878356eb7e20ae-cgu.0.rcgu.o"),
            Some("279e86eba95db35d")
        );
        assert_eq!(unit_hash("build_script_build"), None);
    }
}
