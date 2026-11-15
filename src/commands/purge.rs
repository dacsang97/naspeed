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
use crate::target_layout::{self, UnitFiles};
use crate::wrapper::extern_identity::is_placed;

/// `-<16 hex>` metadata hash from an artifact name like `libserde-0123…cdef.rlib`.
fn unit_hash(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

#[derive(Default)]
struct Removed {
    files: u64,
    bytes: u64,
    fingerprints: u64,
}

/// Deletes the restored files directly in `dir`; returns the unit hashes of
/// what it deleted.
fn remove_placed(dir: &Path, dry_run: bool, removed: &mut Removed) -> Result<BTreeSet<String>> {
    let mut hashes = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(hashes);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_file()) || !is_placed(&path) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(h) = unit_hash(&name) {
            hashes.insert(h.to_owned());
        }
        removed.files += 1;
        removed.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        if !dry_run {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(hashes)
}

// Without its fingerprint cargo would still consider the unit fresh if a
// stale output of the same name came back.
fn remove_fingerprint(dir: &Path, dry_run: bool, removed: &mut Removed) {
    removed.fingerprints += 1;
    if !dry_run {
        let _ = std::fs::remove_dir_all(dir);
    }
}

pub fn run(args: PurgeArgs) -> Result<u8> {
    if !args.target.is_dir() {
        bail!("{} is not a directory", args.target.display());
    }
    let profiles = if target_layout::is_profile_dir(&args.target) {
        vec![args.target.clone()]
    } else {
        target_layout::profile_dirs(&args.target)
    };
    let mut removed = Removed::default();
    for profile in profiles {
        // Flat layout: every unit's artifacts share `deps/`.
        let hashes = remove_placed(&profile.join("deps"), args.dry_run, &mut removed)?;
        for unit in target_layout::units(&profile) {
            match &unit.files {
                UnitFiles::Flat if hashes.contains(&unit.hash) => {
                    remove_fingerprint(&unit.fingerprint, args.dry_run, &mut removed);
                }
                UnitFiles::Flat => {}
                // Per-unit layout (cargo 1.100 on): `build/<pkg>/<hash>/out`.
                UnitFiles::Dir(dir) => {
                    if !remove_placed(&dir.join("out"), args.dry_run, &mut removed)?.is_empty() {
                        remove_fingerprint(&unit.fingerprint, args.dry_run, &mut removed);
                    }
                }
            }
        }
    }
    crate::ui_info!(
        if args.dry_run { "would remove restored files" } else { "removed restored files" };
        files = removed.files, size = human(removed.bytes), fingerprints = removed.fingerprints,
        target = args.target.display());
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
