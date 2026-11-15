//! Where cargo puts a unit's files inside a profile dir (`target/<profile>`
//! or `target/<triple>/<profile>`). Cargo has used two layouts:
//!
//! - flat (cargo 1.99 and earlier): artifacts in `<profile>/deps/`,
//!   fingerprints in `<profile>/.fingerprint/<pkg>-<hash>/`, and each build
//!   script in `<profile>/build/<pkg>-<hash>/` (its binary there, a run's
//!   `OUT_DIR` in `out/` and its stdout in `output`);
//! - per-unit (cargo 1.100 on, always): every unit in
//!   `<profile>/build/<pkg>/<hash>/` with its artifacts (or a run's
//!   `OUT_DIR`) in `out/`, its fingerprint in `fingerprint/` and a
//!   build-script run's stdout in `run/stdout`.
//!
//! `<hash>` is cargo's 16-hex-digit unit hash.

use std::path::{Path, PathBuf};

pub fn is_unit_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `(pkg, hash)` from a flat-layout unit name `<pkg>-<hash>`.
pub fn split_unit_name(name: &str) -> Option<(&str, &str)> {
    let (pkg, hash) = name.rsplit_once('-')?;
    is_unit_hash(hash).then_some((pkg, hash))
}

fn name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// `<profile>` of a per-unit `<profile>/build/<pkg>/<hash>/out`.
fn per_unit_profile(out_dir: &Path) -> Option<&Path> {
    let unit = out_dir.parent()?;
    let build = unit.parent()?.parent()?;
    (name(out_dir)? == "out" && is_unit_hash(name(unit)?) && name(build)? == "build")
        .then(|| build.parent())
        .flatten()
}

/// `<profile>` of a library's `--out-dir`: `<profile>/deps` or
/// `<profile>/build/<pkg>/<hash>/out`.
pub fn library_profile(out_dir: &Path) -> Option<&Path> {
    if name(out_dir)? == "deps" {
        return out_dir.parent();
    }
    per_unit_profile(out_dir)
}

/// `<profile>` of a build script's `--out-dir`: `<profile>/build/<pkg>-<hash>`
/// or `<profile>/build/<pkg>/<hash>/out`.
pub fn build_script_profile(out_dir: &Path) -> Option<&Path> {
    if let Some(profile) = per_unit_profile(out_dir) {
        return Some(profile);
    }
    let build = out_dir.parent()?;
    (name(build)? == "build").then(|| build.parent()).flatten()
}

/// `<profile>` of a build-script run's `OUT_DIR`:
/// `<profile>/build/<pkg>-<hash>/out` or `<profile>/build/<pkg>/<hash>/out`.
pub fn build_script_run_profile(out_dir: &Path) -> Option<&Path> {
    if let Some(profile) = per_unit_profile(out_dir) {
        return Some(profile);
    }
    let build = out_dir.parent()?.parent()?;
    (name(out_dir)? == "out" && name(build)? == "build")
        .then(|| build.parent())
        .flatten()
}

/// Where cargo keeps the stdout of the build-script run whose `OUT_DIR` is
/// `out_dir`.
pub fn build_script_stdout(out_dir: &Path) -> PathBuf {
    let unit = out_dir.parent().unwrap_or(out_dir);
    if per_unit_profile(out_dir).is_some() {
        unit.join("run").join("stdout")
    } else {
        unit.join("output")
    }
}

/// A dir cargo builds one profile into, in either layout.
pub fn is_profile_dir(dir: &Path) -> bool {
    dir.join(".fingerprint").is_dir()
        || (dir.join("build").is_dir() && dir.join(".cargo-lock").exists())
}

/// `target/<profile>` and `target/<triple>/<profile>` dirs cargo has built in.
pub fn profile_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(level1) = std::fs::read_dir(target_dir) else {
        return out;
    };
    for entry in level1.flatten() {
        let dir = entry.path();
        if is_profile_dir(&dir) {
            out.push(dir);
        } else if let Ok(level2) = std::fs::read_dir(&dir) {
            out.extend(
                level2
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| is_profile_dir(p)),
            );
        }
    }
    out.sort();
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitFiles {
    /// Flat layout: the unit's files are spread over `deps/`, `examples/`
    /// and `build/<pkg>-<hash>/`, found by hash.
    Flat,
    /// Per-unit layout: everything is in this `build/<pkg>/<hash>` dir.
    Dir(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitDir {
    pub pkg: String,
    pub hash: String,
    /// The dir holding the unit's fingerprint files.
    pub fingerprint: PathBuf,
    pub files: UnitFiles,
}

/// Every unit with a fingerprint in `profile`, in either layout.
pub fn units(profile: &Path) -> Vec<UnitDir> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(profile.join(".fingerprint")) {
        for entry in rd.flatten() {
            let file_name = entry.file_name();
            if let Some((pkg, hash)) = file_name.to_str().and_then(split_unit_name) {
                out.push(UnitDir {
                    pkg: pkg.to_owned(),
                    hash: hash.to_owned(),
                    fingerprint: entry.path(),
                    files: UnitFiles::Flat,
                });
            }
        }
    }
    let Ok(pkgs) = std::fs::read_dir(profile.join("build")) else {
        return out;
    };
    for pkg in pkgs.flatten() {
        let pkg_name = pkg.file_name().to_string_lossy().into_owned();
        let Ok(hashes) = std::fs::read_dir(pkg.path()) else {
            continue;
        };
        for unit in hashes.flatten() {
            let hash = unit.file_name().to_string_lossy().into_owned();
            let fingerprint = unit.path().join("fingerprint");
            if is_unit_hash(&hash) && fingerprint.is_dir() {
                out.push(UnitDir {
                    pkg: pkg_name.clone(),
                    hash,
                    fingerprint,
                    files: UnitFiles::Dir(unit.path()),
                });
            }
        }
    }
    out
}

/// The unit an artifact path belongs to, without listing any dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitRef {
    /// Flat layout: `.fingerprint/<pkg>-<hash>` in `profile`, found by hash.
    Flat { profile: PathBuf, hash: String },
    /// Per-unit layout: the unit's fingerprint dir.
    Fingerprint(PathBuf),
}

/// The unit behind an artifact path: `deps/libcore-<hash>.rlib` or
/// `build/core-<hash>/out` (flat), or anything under `build/<pkg>/<hash>/`
/// (per-unit). An uplifted `<profile>/<name>` carries no hash: see
/// `unit_of_uplifted`.
pub fn unit_of_path(path: &Path) -> Option<UnitRef> {
    // Per-unit: `<profile>/build/<pkg>/<hash>/…`.
    for unit in path.ancestors() {
        let (Some(hash), Some(build)) = (name(unit), unit.parent().and_then(Path::parent)) else {
            continue;
        };
        if is_unit_hash(hash) && name(build) == Some("build") {
            let fingerprint = unit.join("fingerprint");
            return fingerprint
                .is_dir()
                .then_some(UnitRef::Fingerprint(fingerprint));
        }
    }
    // Flat: the nearest `<pkg>-<hash>` name below the profile dir.
    let mut hash = None;
    for dir in path.ancestors() {
        if dir.join(".fingerprint").is_dir() {
            return Some(UnitRef::Flat {
                profile: dir.to_path_buf(),
                hash: hash?,
            });
        }
        if hash.is_none() {
            hash = name(dir)
                .and_then(|n| n.split('.').next())
                .and_then(split_unit_name)
                .map(|(_, h)| h.to_owned());
        }
    }
    None
}

/// The unit an uplifted `<profile>/<stem>[.ext]` was copied or linked from,
/// with the same size and mtime: `deps/<stem>-<hash>[.ext]` (flat), or in
/// the per-unit layout `build/<pkg>/<hash>/out/<stem>[.ext]` (a bin, which
/// cargo 1.100 builds without the hash suffix) or `out/<stem>-<hash>[.ext]`.
pub fn unit_of_uplifted(profile: &Path, path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let (len, modified) = (meta.len(), meta.modified().ok()?);
    let file = name(path)?;
    let (stem, ext) = match file.split_once('.') {
        Some((stem, ext)) => (stem, format!(".{ext}")),
        None => (file, String::new()),
    };
    let same = |candidate: &Path| {
        std::fs::metadata(candidate)
            .is_ok_and(|m| m.len() == len && m.modified().ok() == Some(modified))
    };
    for unit in units(profile) {
        let hashed = format!("{stem}-{}{ext}", unit.hash);
        let found = match &unit.files {
            UnitFiles::Flat => same(&profile.join("deps").join(&hashed)),
            UnitFiles::Dir(dir) => {
                let out = dir.join("out");
                same(&out.join(file)) || same(&out.join(&hashed))
            }
        };
        if found {
            return Some(unit.fingerprint);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef";

    #[test]
    fn finds_the_profile_of_out_dirs_in_both_layouts() {
        let p = Path::new("/t/debug");
        assert_eq!(library_profile(&p.join("deps")), Some(p));
        assert_eq!(
            library_profile(&p.join(format!("build/serde/{H}/out"))),
            Some(p)
        );
        assert_eq!(library_profile(&p.join("build")), None);
        assert_eq!(
            library_profile(&p.join(format!("build/serde-{H}/out"))),
            None
        );

        assert_eq!(
            build_script_profile(&p.join(format!("build/serde-{H}"))),
            Some(p)
        );
        assert_eq!(
            build_script_profile(&p.join(format!("build/serde/{H}/out"))),
            Some(p)
        );
        assert_eq!(build_script_profile(&p.join("deps")), None);

        assert_eq!(
            build_script_run_profile(&p.join(format!("build/serde-{H}/out"))),
            Some(p)
        );
        assert_eq!(
            build_script_run_profile(&p.join(format!("build/serde/{H}/out"))),
            Some(p)
        );
        // A package named `build` in the per-unit layout.
        assert_eq!(
            build_script_run_profile(&p.join(format!("build/build/{H}/out"))),
            Some(p)
        );
        assert_eq!(build_script_run_profile(&p.join("deps")), None);
    }

    #[test]
    fn finds_host_and_cross_profile_dirs_only() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path();
        for dir in [
            "debug/.fingerprint",
            "release/.fingerprint",
            "aarch64-unknown-linux-gnu/debug/.fingerprint",
            // cargo 1.100's per-unit layout: no `.fingerprint`.
            "x86_64-unknown-linux-gnu/release/build/core",
            "tmp",
            "doc/core",
        ] {
            std::fs::create_dir_all(target.join(dir)).unwrap();
        }
        std::fs::write(
            target.join("x86_64-unknown-linux-gnu/release/.cargo-lock"),
            "",
        )
        .unwrap();
        assert_eq!(
            profile_dirs(target),
            vec![
                target.join("aarch64-unknown-linux-gnu/debug"),
                target.join("debug"),
                target.join("release"),
                target.join("x86_64-unknown-linux-gnu/release"),
            ]
        );
    }

    #[test]
    fn build_script_stdout_follows_the_layout() {
        let p = Path::new("/t/debug");
        assert_eq!(
            build_script_stdout(&p.join(format!("build/serde-{H}/out"))),
            p.join(format!("build/serde-{H}/output"))
        );
        assert_eq!(
            build_script_stdout(&p.join(format!("build/serde/{H}/out"))),
            p.join(format!("build/serde/{H}/run/stdout"))
        );
    }

    fn per_unit(profile: &Path, pkg: &str, hash: &str) -> PathBuf {
        let unit = profile.join("build").join(pkg).join(hash);
        std::fs::create_dir_all(unit.join("fingerprint")).unwrap();
        std::fs::create_dir_all(unit.join("out")).unwrap();
        unit
    }

    #[test]
    fn lists_units_and_maps_artifacts_in_both_layouts() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let flat_fp = profile.join(format!(".fingerprint/core-{H}"));
        std::fs::create_dir_all(&flat_fp).unwrap();
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        let other = "fedcba9876543210";
        let unit = per_unit(&profile, "serde", other);
        // A per-unit dir without a fingerprint is not a unit.
        std::fs::create_dir_all(profile.join("build/serde/1111111111111111/out")).unwrap();
        std::fs::write(profile.join(".cargo-lock"), "").unwrap();
        assert!(is_profile_dir(&profile));

        let mut found: Vec<_> = units(&profile).into_iter().map(|u| u.hash).collect();
        found.sort();
        assert_eq!(found, vec![H.to_owned(), other.to_owned()]);

        assert_eq!(
            unit_of_path(&profile.join(format!("deps/libcore-{H}.rlib"))),
            Some(UnitRef::Flat {
                profile: profile.clone(),
                hash: H.to_owned()
            })
        );
        assert_eq!(
            unit_of_path(&unit.join(format!("out/libserde-{other}.rlib"))),
            Some(UnitRef::Fingerprint(unit.join("fingerprint")))
        );
        assert_eq!(unit_of_path(&profile.join("app")), None);
        let _ = flat_fp;
    }

    #[test]
    fn maps_uplifted_binaries_by_size_and_mtime() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let built = filetime::FileTime::from_unix_time(1_790_000_000, 123);
        let write = |path: &Path, body: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
            filetime::set_file_mtime(path, built).unwrap();
        };
        let unit = per_unit(&profile, "app", H);
        write(&unit.join(format!("out/tool-{H}")), "tool");
        let stale = per_unit(&profile, "app", "fedcba9876543210");
        write(&stale.join("out/tool-fedcba9876543210"), "old tool");
        write(&profile.join("tool"), "tool");
        assert_eq!(
            unit_of_uplifted(&profile, &profile.join("tool")),
            Some(unit.join("fingerprint"))
        );

        // A plain bin: cargo 1.100 writes `out/<name>` without the hash.
        let bin = per_unit(&profile, "app", "1111111111111111");
        write(&bin.join("out/app"), "app");
        let old_bin = per_unit(&profile, "app", "2222222222222222");
        write(&old_bin.join("out/app"), "old app");
        write(&profile.join("app"), "app");
        assert_eq!(
            unit_of_uplifted(&profile, &profile.join("app")),
            Some(bin.join("fingerprint"))
        );
    }
}
