//! `dist/metadata.json` as written by `ns release`, and the choice of which
//! build goes into the image.
//!
//! Version 2 adds `tag`, `snapshot`, `prerelease` and a `build` id on every
//! artifact; version 1 files (without them) are still accepted: a missing
//! `build` falls back to the shipped binary name, which is also the default
//! build id.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::project::NsConfig;

#[derive(Debug, Deserialize)]
pub struct ReleaseMetadata {
    #[serde(default)]
    pub project: Option<String>,
    pub version: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub snapshot: Option<bool>,
    #[serde(default)]
    pub prerelease: Option<bool>,
    #[serde(default)]
    pub source_date_epoch: Option<u64>,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Deserialize)]
pub struct Artifact {
    pub kind: String,
    /// Release build id (v2).
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    /// Shipped binary name.
    #[serde(default)]
    pub bin: Option<String>,
    pub path: PathBuf,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Artifact {
    /// Build id, or the shipped name for v1 artifacts (the default id).
    fn build_id(&self) -> Option<&str> {
        self.build.as_deref().or(self.bin.as_deref())
    }
}

impl ReleaseMetadata {
    /// `Ok(None)` when the file does not exist.
    pub fn load(dist: &Path) -> Result<Option<ReleaseMetadata>> {
        let path = dist.join("metadata.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))
                .map(Some),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    /// `snapshot` (v2), else a `SNAPSHOT` marker in the version (v1).
    pub fn is_snapshot(&self) -> bool {
        self.snapshot
            .unwrap_or_else(|| self.version.to_ascii_uppercase().contains("SNAPSHOT"))
    }

    /// The SBOM artifact written for `chosen`'s build, if any.
    pub fn sbom_for(&self, chosen: &ChosenBinary) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|a| a.kind == "sbom" && chosen.matches(a))
    }
}

/// The binary placed in the image: its shipped name (for `{binary}` in
/// `[image].path`) and, when known from metadata, its release build id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChosenBinary {
    pub bin: String,
    pub build: Option<String>,
}

impl ChosenBinary {
    /// Whether a metadata artifact belongs to this build.
    pub fn matches(&self, artifact: &Artifact) -> bool {
        match &self.build {
            Some(id) => artifact.build_id() == Some(id),
            None => artifact.bin.as_deref() == Some(&self.bin),
        }
    }
}

/// `[image].build`, else `[image].bin`, else the only build in metadata.json.
pub fn choose_from_metadata(meta: &ReleaseMetadata, config: &NsConfig) -> Result<ChosenBinary> {
    let mut candidates: Vec<ChosenBinary> = meta
        .artifacts
        .iter()
        .filter(|a| a.kind == "binary")
        .filter_map(|a| {
            let bin = a.bin.clone()?;
            Some(ChosenBinary {
                build: a.build_id().map(str::to_string),
                bin,
            })
        })
        .collect();
    candidates.sort_by(|a, b| (&a.build, &a.bin).cmp(&(&b.build, &b.bin)));
    candidates.dedup();
    let describe = |list: &[ChosenBinary]| {
        list.iter()
            .map(|c| c.build.clone().unwrap_or_else(|| c.bin.clone()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let image = &config.image;
    let selected: Vec<ChosenBinary> = if let Some(id) = &image.build {
        candidates
            .iter()
            .filter(|c| c.build.as_deref() == Some(id.as_str()))
            .cloned()
            .collect()
    } else if let Some(bin) = &image.bin {
        candidates
            .iter()
            .filter(|c| &c.bin == bin)
            .cloned()
            .collect()
    } else {
        candidates.clone()
    };
    match selected.as_slice() {
        [one] => Ok(one.clone()),
        [] if image.build.is_some() || image.bin.is_some() => bail!(
            "metadata.json has no binary for {}; available builds: {}",
            image
                .build
                .as_ref()
                .map(|id| format!("[image].build = {id:?}"))
                .unwrap_or_else(|| format!(
                    "[image].bin = {:?}",
                    image.bin.as_deref().unwrap_or("")
                )),
            if candidates.is_empty() {
                "none".to_string()
            } else {
                describe(&candidates)
            }
        ),
        [] => bail!("metadata.json lists no binaries; run `ns release` with Linux targets"),
        many => bail!(
            "several builds ({}); choose one with [image].build or [image].bin in ns.toml",
            describe(many)
        ),
    }
}

/// Without metadata.json: `[image].bin`, else the shipped name of
/// `[image].build`, else the `--binary` file name, else the only workspace bin.
pub fn choose_without_metadata(
    config: &NsConfig,
    flag_paths: &[PathBuf],
    workspace_bins: &[String],
) -> Result<ChosenBinary> {
    let chosen = |bin: String| Ok(ChosenBinary { bin, build: None });
    if let Some(bin) = &config.image.bin {
        return chosen(bin.clone());
    }
    if let Some(id) = &config.image.build {
        let builds = &config.release.builds;
        return match builds.iter().find(|b| b.id() == id) {
            Some(build) => chosen(build.binary_name().to_string()),
            // Without [[release.builds]] each binary's id is its own name.
            None if builds.is_empty() => chosen(id.clone()),
            None => bail!("[image].build = {id:?} matches no [[release.builds]] id"),
        };
    }
    let candidates: Vec<String> = if let Some(path) = flag_paths.first() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("--binary path has no file name")?;
        vec![name.to_string()]
    } else {
        workspace_bins.to_vec()
    };
    match candidates.as_slice() {
        [one] => chosen(one.clone()),
        [] => bail!("no binary to put in the image; run `ns release` first or pass --binary"),
        many => bail!(
            "several binaries ({}); choose one with [image].bin in ns.toml",
            many.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2: &str = r#"{"project":"p","version":"1.0.0","tag":"v1.0.0","commit":"abc","snapshot":false,
        "prerelease":false,"source_date_epoch":5,"artifacts":[
        {"kind":"binary","build":"api","bin":"api","target":"x86_64-unknown-linux-musl","path":"x86_64-unknown-linux-musl/api","sha256":"00"},
        {"kind":"binary","build":"noui","bin":"api-noui","target":"x86_64-unknown-linux-musl","path":"x86_64-unknown-linux-musl/api-noui","sha256":"00"},
        {"kind":"sbom","build":"api","bin":"api","target":null,"path":"api-1.0.0.spdx.json","sha256":"00"},
        {"kind":"sbom","build":"noui","bin":"api-noui","target":null,"path":"api-noui-1.0.0.spdx.json","sha256":"00"}]}"#;

    const V1: &str = r#"{"project":"p","version":"1.0.0-SNAPSHOT-abc","commit":"abc","artifacts":[
        {"kind":"binary","target":"x86_64-unknown-linux-musl","bin":"api","path":"x86_64-unknown-linux-musl/api"}]}"#;

    fn config(toml: &str) -> NsConfig {
        NsConfig::parse(toml).unwrap()
    }

    #[test]
    fn v2_selects_by_build_then_bin_and_finds_its_sbom() {
        let meta: ReleaseMetadata = serde_json::from_str(V2).unwrap();
        assert!(!meta.is_snapshot());
        let err = choose_from_metadata(&meta, &config(""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("api, noui"), "{err}");

        let noui = choose_from_metadata(&meta, &config("[image]\nbuild = \"noui\"\n")).unwrap();
        assert_eq!(
            noui,
            ChosenBinary {
                bin: "api-noui".into(),
                build: Some("noui".into())
            }
        );
        assert_eq!(
            meta.sbom_for(&noui).unwrap().path,
            PathBuf::from("api-noui-1.0.0.spdx.json")
        );
        let by_bin = choose_from_metadata(&meta, &config("[image]\nbin = \"api\"\n")).unwrap();
        assert_eq!(by_bin.build.as_deref(), Some("api"));
        assert!(choose_from_metadata(&meta, &config("[image]\nbuild = \"x\"\n")).is_err());
    }

    #[test]
    fn v1_metadata_still_works() {
        let meta: ReleaseMetadata = serde_json::from_str(V1).unwrap();
        assert!(meta.is_snapshot());
        assert_eq!(meta.prerelease, None);
        let chosen = choose_from_metadata(&meta, &config("")).unwrap();
        assert_eq!(chosen.bin, "api");
        // A v1 build id is the shipped name.
        assert_eq!(
            choose_from_metadata(&meta, &config("[image]\nbuild = \"api\"\n"))
                .unwrap()
                .bin,
            "api"
        );
        assert!(meta.sbom_for(&chosen).is_none());
    }

    #[test]
    fn without_metadata_uses_config_flags_or_workspace() {
        let builds = config(
            "[[release.builds]]\nid = \"noui\"\nbin = \"api\"\nbinary = \"api-noui\"\n[image]\nbuild = \"noui\"\n",
        );
        assert_eq!(
            choose_without_metadata(&builds, &[], &[]).unwrap().bin,
            "api-noui"
        );
        let flags = [PathBuf::from("target/x/hello")];
        assert_eq!(
            choose_without_metadata(&config(""), &flags, &[])
                .unwrap()
                .bin,
            "hello"
        );
        let bins = ["a".to_string(), "b".to_string()];
        assert!(choose_without_metadata(&config(""), &[], &bins).is_err());
        assert_eq!(
            choose_without_metadata(&config(""), &[], &bins[..1])
                .unwrap()
                .bin,
            "a"
        );
    }
}
