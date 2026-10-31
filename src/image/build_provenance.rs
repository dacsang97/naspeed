//! Image provenance: OCI labels (title, version, revision, source from the
//! `origin` remote, created) and `SOURCE_DATE_EPOCH`, which fixes `created`
//! and layer mtimes.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::project::Project;

use super::manifest::rfc3339;
use super::release_metadata::{ChosenBinary, ReleaseMetadata};

/// `git <args>` in `dir`, trimmed stdout, `None` on any failure.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// `SOURCE_DATE_EPOCH` from metadata.json, the environment, the last commit, or 0.
pub fn source_date_epoch(root: &Path, metadata: Option<&ReleaseMetadata>) -> Result<u64> {
    if let Some(epoch) = metadata.and_then(|m| m.source_date_epoch) {
        return Ok(epoch);
    }
    if let Ok(text) = std::env::var("SOURCE_DATE_EPOCH") {
        return text
            .trim()
            .parse()
            .with_context(|| format!("SOURCE_DATE_EPOCH={text:?} is not a number"));
    }
    Ok(git(root, &["log", "-1", "--format=%ct"])
        .and_then(|t| t.parse().ok())
        .unwrap_or(0))
}

/// What the image is a release of: version, tag, commit and channel.
pub struct ReleaseInfo {
    pub title: String,
    pub version: String,
    pub tag: Option<String>,
    pub commit: Option<String>,
    pub snapshot: bool,
    pub prerelease: bool,
}

impl ReleaseInfo {
    pub fn new(
        project: &Project,
        meta: Option<&ReleaseMetadata>,
        chosen: &ChosenBinary,
    ) -> Result<Self> {
        let auto = project.config.release.prerelease;
        if let Some(meta) = meta {
            let snapshot = meta.is_snapshot();
            let prerelease = meta.prerelease.unwrap_or_else(|| {
                !snapshot && auto.resolve(meta.tag.as_deref().unwrap_or(&meta.version))
            });
            return Ok(ReleaseInfo {
                title: meta.project.clone().unwrap_or_else(|| chosen.bin.clone()),
                version: meta.version.clone(),
                tag: meta.tag.clone().filter(|t| !t.is_empty()),
                commit: meta
                    .commit
                    .clone()
                    .or_else(|| head_commit(&project.graph.root)),
                snapshot,
                prerelease,
            });
        }
        // A renamed build (`binary = "api-noui"`) is owned by its cargo bin's package.
        let cargo_bin = project
            .config
            .release
            .builds
            .iter()
            .find(|b| b.binary_name() == chosen.bin)
            .map_or(chosen.bin.as_str(), |b| b.bin.as_str());
        let member = project.graph.bin_owner(cargo_bin).with_context(|| {
            format!(
                "no workspace package owns binary {cargo_bin}; run `ns release` or set [image].bin"
            )
        })?;
        let snapshot = member.version.to_ascii_uppercase().contains("SNAPSHOT");
        Ok(ReleaseInfo {
            title: member.name.clone(),
            version: member.version.clone(),
            tag: None,
            commit: head_commit(&project.graph.root),
            snapshot,
            prerelease: !snapshot && auto.resolve(&member.version),
        })
    }
}

/// `git rev-parse HEAD` at the workspace root.
pub fn head_commit(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"])
}

pub struct LabelInputs<'a> {
    pub title: &'a str,
    pub version: &'a str,
    pub commit: Option<&'a str>,
    pub created_epoch: u64,
    /// `[image].labels`, applied last so they can override the defaults.
    pub extra: &'a BTreeMap<String, String>,
}

/// `org.opencontainers.image.*` labels plus `[image].labels`.
pub fn labels(root: &Path, inputs: &LabelInputs) -> BTreeMap<String, String> {
    let oci = |key: &str| format!("org.opencontainers.image.{key}");
    let mut labels = BTreeMap::new();
    labels.insert(oci("title"), inputs.title.to_string());
    labels.insert(oci("version"), inputs.version.to_string());
    labels.insert(oci("created"), rfc3339(inputs.created_epoch));
    if let Some(revision) = inputs.commit {
        labels.insert(oci("revision"), revision.to_string());
    }
    if let Some(source) =
        git(root, &["remote", "get-url", "origin"]).and_then(|url| source_url(&url))
    {
        labels.insert(oci("source"), source);
    }
    labels.extend(inputs.extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    labels
}

/// `git@github.com:o/r.git` / `https://user@github.com/o/r.git` → `https://github.com/o/r`.
fn source_url(remote: &str) -> Option<String> {
    let rest = match remote.strip_prefix("git@") {
        Some(rest) => rest.replacen(':', "/", 1),
        None => {
            let rest = remote
                .strip_prefix("https://")
                .or_else(|| remote.strip_prefix("ssh://"))?;
            let (authority, path) = rest.split_once('/')?;
            // Drop any credentials before the host.
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            format!("{host}/{path}")
        }
    };
    Some(format!(
        "https://{}",
        rest.trim_end_matches('/').trim_end_matches(".git")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_urls_drop_credentials_and_suffix() {
        assert_eq!(
            source_url("git@github.com:o/r.git").as_deref(),
            Some("https://github.com/o/r")
        );
        assert_eq!(
            source_url("https://x-access-token:secret@github.com/o/r.git").as_deref(),
            Some("https://github.com/o/r")
        );
        assert_eq!(source_url("/tmp/local").as_deref(), None);
    }

    #[test]
    fn labels_cover_title_created_and_extra_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let extra = BTreeMap::from([
            (
                "org.opencontainers.image.title".to_string(),
                "Custom".to_string(),
            ),
            ("com.example.team".to_string(), "core".to_string()),
        ]);
        let mut inputs = LabelInputs {
            title: "myapp",
            version: "1.2.0",
            commit: Some("abc"),
            created_epoch: 1_700_000_000,
            extra: &BTreeMap::new(),
        };
        let l = labels(dir.path(), &inputs);
        assert_eq!(l["org.opencontainers.image.title"], "myapp");
        assert_eq!(l["org.opencontainers.image.version"], "1.2.0");
        assert_eq!(l["org.opencontainers.image.revision"], "abc");
        assert_eq!(
            l["org.opencontainers.image.created"],
            "2023-11-14T22:13:20Z"
        );
        inputs.extra = &extra;
        let l = labels(dir.path(), &inputs);
        assert_eq!(l["org.opencontainers.image.title"], "Custom");
        assert_eq!(l["com.example.team"], "core");
    }
}
