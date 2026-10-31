//! The `dist/` directory: ownership marker, cleaning, and `metadata.json`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Marker proving a directory was produced by `ns release` and may be wiped.
pub const MARKER: &str = ".ns-dist";
pub const METADATA: &str = "metadata.json";

/// Empty `dir` (creating it if needed) and write the marker. A non-empty
/// directory without the marker is refused: it may hold someone's files.
pub fn prepare(dir: &Path) -> Result<()> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            let empty = entries.next().is_none();
            if !empty {
                if !dir.join(MARKER).is_file() {
                    bail!(
                        "{} is not empty and has no {MARKER} marker; refusing to delete files ns did not produce",
                        dir.display()
                    );
                }
                std::fs::remove_dir_all(dir).with_context(|| format!("clean {}", dir.display()))?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join(MARKER), "produced by ns release; safe to delete\n")
        .with_context(|| format!("write {}", dir.join(MARKER).display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Binary,
    Archive,
    Checksums,
    Signature,
    Sbom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: Kind,
    /// Release build id; `None` for checksums and signatures.
    pub build: Option<String>,
    /// Shipped binary name of that build.
    pub bin: Option<String>,
    pub target: Option<String>,
    /// Relative to the dist dir, `/`-separated.
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub project: String,
    pub version: String,
    /// Release tag; `None` for snapshots.
    pub tag: Option<String>,
    pub commit: Option<String>,
    pub snapshot: bool,
    pub prerelease: bool,
    pub source_date_epoch: u64,
    pub artifacts: Vec<Artifact>,
}

impl Metadata {
    pub fn write(&self, dist: &Path) -> Result<()> {
        let path = dist.join(METADATA);
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_rules() {
        let root = tempfile::tempdir().unwrap();
        let dist = root.path().join("dist");
        prepare(&dist).unwrap();
        std::fs::write(dist.join("old.tar.gz"), b"x").unwrap();
        prepare(&dist).unwrap();
        assert!(!dist.join("old.tar.gz").exists());
        assert!(dist.join(MARKER).is_file());

        let foreign = root.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("keep.txt"), b"x").unwrap();
        assert!(prepare(&foreign).is_err());
        assert!(foreign.join("keep.txt").exists());

        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        prepare(&empty).unwrap();
    }

    #[test]
    fn metadata_shape() {
        let m = Metadata {
            project: "p".into(),
            version: "1.0.0".into(),
            tag: Some("v1.0.0".into()),
            commit: None,
            snapshot: false,
            prerelease: false,
            source_date_epoch: 0,
            artifacts: vec![Artifact {
                kind: Kind::Checksums,
                build: None,
                bin: None,
                target: None,
                path: "SHA256SUMS".into(),
                sha256: "0".repeat(64),
            }],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["commit"], serde_json::Value::Null);
        assert_eq!(v["artifacts"][0]["kind"], "checksums");
        assert_eq!(v["artifacts"][0]["target"], serde_json::Value::Null);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 8, "{keys:?}");
        assert_eq!(v["tag"], "v1.0.0");
        assert_eq!(v["artifacts"][0].as_object().unwrap().len(), 6);
    }
}
