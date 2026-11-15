//! Mark the target-dir units a cargo run used, fresh or rebuilt.
//!
//! A no-op build touches nothing in `target/`, so file times cannot tell an
//! unchanged unit from an abandoned one. Cargo's JSON stream names every unit
//! of the build graph, fresh ones included: `compiler-artifact` lists its
//! files and `build-script-executed` its out dir. Setting the mtime of each
//! unit's `.fingerprint/<pkg>-<hash>/` dir records the use; `target_sweep`
//! reads it back. Cargo itself never looks at that mtime.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::target_sweep::{artifact_hash, split_unit_name};

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

/// Touches `.fingerprint/<pkg>-<hash>/` of every unit behind `paths`.
/// Best effort: anything unreadable is skipped.
pub fn mark_used(paths: &[PathBuf]) {
    let mut by_profile: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for path in paths {
        if let Some((profile, hash)) = unit_of(path) {
            by_profile.entry(profile).or_default().insert(hash);
        }
    }
    let now = filetime::FileTime::now();
    for (profile, hashes) in by_profile {
        let Ok(rd) = std::fs::read_dir(profile.join(".fingerprint")) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            if let Some((_, hash)) = name.to_str().and_then(split_unit_name)
                && hashes.contains(hash)
            {
                let _ = filetime::set_file_mtime(entry.path(), now);
            }
        }
    }
}

/// (profile dir, unit hash) of an artifact path inside a profile dir, e.g.
/// `debug/deps/libcore-<hash>.rlib`, `debug/build/core-<hash>/out` or the
/// uplifted `debug/app`.
fn unit_of(path: &Path) -> Option<(PathBuf, String)> {
    let mut hash = None;
    for dir in path.ancestors() {
        if dir.join(".fingerprint").is_dir() {
            let hash = match hash {
                Some(hash) => hash,
                None => uplifted_hash(dir, path)?,
            };
            return Some((dir.to_path_buf(), hash));
        }
        if hash.is_none() {
            hash = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(artifact_hash)
                .map(str::to_owned);
        }
    }
    None
}

/// Cargo uplifts `deps/<name>-<hash>[.exe]` to `<profile>/<name>[.exe]`
/// as a hard link, or a copy or clone where it cannot link; either way size
/// and mtime match the original.
fn uplifted_hash(profile: &Path, path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let (len, modified) = (meta.len(), meta.modified().ok()?);
    let name = path.file_name()?.to_str()?;
    let (stem, ext) = match name.split_once('.') {
        Some((stem, ext)) => (stem, format!(".{ext}")),
        None => (name, String::new()),
    };
    std::fs::read_dir(profile.join("deps"))
        .ok()?
        .flatten()
        .find_map(|entry| {
            let candidate = entry.file_name().into_string().ok()?;
            let hash = artifact_hash(&candidate)?;
            if candidate != format!("{stem}-{hash}{ext}") {
                return None;
            }
            let other = entry.metadata().ok()?;
            (other.len() == len && other.modified().ok()? == modified).then(|| hash.to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

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
