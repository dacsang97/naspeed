//! Mark the target-dir units a cargo run used, fresh or rebuilt.
//!
//! A no-op build touches nothing in `target/`, so file times cannot tell an
//! unchanged unit from an abandoned one. Cargo's JSON stream names every unit
//! of the build graph, fresh ones included: `compiler-artifact` lists its
//! files and `build-script-executed` its out dir. Setting the mtime of each
//! unit's fingerprint dir (`.fingerprint/<pkg>-<hash>/`, or
//! `build/<pkg>/<hash>/fingerprint/` in cargo 1.100's layout) records the
//! use; `target_sweep` reads it back. Cargo itself never looks at that mtime.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::target_layout::{self, UnitRef};

/// Paths a cargo JSON message names: artifact files, the uplifted
/// executable and build-script out dirs.
pub fn message_paths(msg: &Value) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match msg["reason"].as_str() {
        Some("compiler-artifact") => {
            if let Some(list) = msg["filenames"].as_array() {
                out.extend(list.iter().filter_map(Value::as_str).map(PathBuf::from));
            }
            if let Some(exe) = msg["executable"].as_str() {
                out.push(PathBuf::from(exe));
            }
        }
        Some("build-script-executed") => {
            if let Some(dir) = msg["out_dir"].as_str() {
                out.push(PathBuf::from(dir));
            }
        }
        _ => {}
    }
    out
}

/// Touches the fingerprint dir of every unit behind `paths`.
/// Best effort: anything unreadable is skipped.
pub fn mark_used(paths: &[PathBuf]) {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    let mut flat: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for path in paths {
        let unit = target_layout::unit_of_path(path).or_else(|| {
            // An uplifted `<profile>/<name>`: no hash in the path.
            let profile = path.parent()?;
            target_layout::is_profile_dir(profile)
                .then(|| target_layout::unit_of_uplifted(profile, path))
                .flatten()
                .map(UnitRef::Fingerprint)
        });
        match unit {
            Some(UnitRef::Fingerprint(dir)) => {
                dirs.insert(dir);
            }
            Some(UnitRef::Flat { profile, hash }) => {
                flat.entry(profile).or_default().insert(hash);
            }
            None => {}
        }
    }
    for (profile, hashes) in flat {
        let Ok(rd) = std::fs::read_dir(profile.join(".fingerprint")) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            if let Some((_, hash)) = name.to_str().and_then(target_layout::split_unit_name)
                && hashes.contains(hash)
            {
                dirs.insert(entry.path());
            }
        }
    }
    let now = filetime::FileTime::now();
    for dir in dirs {
        let _ = filetime::set_file_mtime(dir, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    #[test]
    fn marks_units_in_the_per_unit_layout() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        std::fs::write(
            {
                std::fs::create_dir_all(&profile).unwrap();
                profile.join(".cargo-lock")
            },
            "",
        )
        .unwrap();
        let unit = |pkg: &str, hash: &str| {
            let dir = profile.join("build").join(pkg).join(hash);
            std::fs::create_dir_all(dir.join("fingerprint")).unwrap();
            std::fs::create_dir_all(dir.join("out")).unwrap();
            backdate(&dir.join("fingerprint"));
            dir
        };
        let lib = unit("core", "aaaaaaaaaaaaaaaa");
        let script = unit("core", "bbbbbbbbbbbbbbbb");
        let bin = unit("app", "cccccccccccccccc");
        let idle = unit("app", "dddddddddddddddd");
        let built = filetime::FileTime::from_unix_time(1_790_000_000, 7);
        let exe = bin.join("out/app-cccccccccccccccc");
        std::fs::write(&exe, "bin").unwrap();
        filetime::set_file_mtime(&exe, built).unwrap();
        std::fs::write(profile.join("app"), "bin").unwrap();
        filetime::set_file_mtime(profile.join("app"), built).unwrap();

        mark_used(&[
            lib.join("out/libcore-aaaaaaaaaaaaaaaa.rlib"),
            script.join("out"),
            profile.join("app"),
        ]);

        let day_ago = SystemTime::now() - Duration::from_secs(86_400);
        for used in [&lib, &script, &bin] {
            assert!(
                mtime(&used.join("fingerprint")) > day_ago,
                "{}",
                used.display()
            );
        }
        assert!(mtime(&idle.join("fingerprint")) < day_ago);
    }

    fn mtime(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    fn backdate(path: &Path) {
        let old = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(30 * 86_400),
        );
        filetime::set_file_mtime(path, old).unwrap();
    }

    #[test]
    fn reads_paths_from_artifact_and_build_script_messages() {
        let artifact: Value = serde_json::from_str(
            r#"{"reason":"compiler-artifact","fresh":true,
                "filenames":["/t/debug/deps/libcore-aaaaaaaaaaaaaaaa.rlib","/t/debug/deps/libcore-aaaaaaaaaaaaaaaa.rmeta"],
                "executable":null}"#,
        )
        .unwrap();
        let script: Value = serde_json::from_str(
            r#"{"reason":"build-script-executed","out_dir":"/t/debug/build/core-bbbbbbbbbbbbbbbb/out"}"#,
        )
        .unwrap();
        assert_eq!(message_paths(&artifact).len(), 2);
        assert_eq!(
            message_paths(&script),
            vec![PathBuf::from("/t/debug/build/core-bbbbbbbbbbbbbbbb/out")]
        );
        let other: Value = serde_json::from_str(r#"{"reason":"build-finished"}"#).unwrap();
        assert!(message_paths(&other).is_empty());
    }

    #[test]
    fn marks_only_the_units_named() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        let fp = profile.join(".fingerprint");
        for unit in [
            "core-aaaaaaaaaaaaaaaa",
            "core-bbbbbbbbbbbbbbbb",
            "app-cccccccccccccccc",
            "app-dddddddddddddddd",
            "core-dddddddddddddddd",
        ] {
            std::fs::create_dir_all(fp.join(unit)).unwrap();
            backdate(&fp.join(unit));
        }
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::create_dir_all(profile.join("build/core-bbbbbbbbbbbbbbbb/out")).unwrap();
        // The uplifted `app` is a copy: same size and mtime as its original,
        // unlike `app-dddd…` (another build of it) and the dep-info file.
        let built = filetime::FileTime::from_unix_time(1_790_000_000, 123);
        for (file, body) in [
            ("app-cccccccccccccccc", "bin"),
            ("app-cccccccccccccccc.d", "dep"),
            ("app-dddddddddddddddd", "old bin"),
        ] {
            std::fs::write(deps.join(file), body).unwrap();
            filetime::set_file_mtime(deps.join(file), built).unwrap();
        }
        std::fs::write(profile.join("app"), "bin").unwrap();
        filetime::set_file_mtime(profile.join("app"), built).unwrap();

        mark_used(&[
            deps.join("libcore-aaaaaaaaaaaaaaaa.rlib"),
            profile.join("build/core-bbbbbbbbbbbbbbbb/out"),
            profile.join("app"),
        ]);

        let day_ago = SystemTime::now() - Duration::from_secs(86_400);
        assert!(mtime(&fp.join("core-aaaaaaaaaaaaaaaa")) > day_ago);
        assert!(mtime(&fp.join("core-bbbbbbbbbbbbbbbb")) > day_ago);
        assert!(mtime(&fp.join("app-cccccccccccccccc")) > day_ago);
        assert!(mtime(&fp.join("app-dddddddddddddddd")) < day_ago);
        assert!(mtime(&fp.join("core-dddddddddddddddd")) < day_ago);
    }
}
