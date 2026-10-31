//! OCI manifest, image index and config JSON.
//!
//! Base images may use OCI or Docker v2 media types; what `ns image` writes is
//! always OCI. Base layers are kept by digest (the same bytes under either
//! media type); the derived config lives in `image_config`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::reference::Platform;

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";

pub const ACCEPTED_MANIFEST_TYPES: &[&str] =
    &[OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformSpec {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl Descriptor {
    pub fn new(media_type: &str, digest: &str, size: u64) -> Descriptor {
        Descriptor {
            media_type: media_type.into(),
            digest: digest.into(),
            size,
            platform: None,
            annotations: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
}

/// A base manifest as fetched: either a single image or a multi-platform list.
pub enum BaseManifest {
    Index(ImageIndex),
    Image(ImageManifest),
}

pub fn parse_base(bytes: &[u8], content_type: &str) -> Result<BaseManifest> {
    let value: Value = serde_json::from_slice(bytes).context("parse base manifest JSON")?;
    let media_type = value["mediaType"].as_str().unwrap_or(content_type);
    match media_type {
        OCI_INDEX | DOCKER_LIST => Ok(BaseManifest::Index(serde_json::from_value(value)?)),
        OCI_MANIFEST | DOCKER_MANIFEST => Ok(BaseManifest::Image(serde_json::from_value(value)?)),
        // OCI allows omitting mediaType: tell them apart by shape.
        _ if value.get("manifests").is_some() => {
            Ok(BaseManifest::Index(serde_json::from_value(value)?))
        }
        _ if value.get("layers").is_some() => {
            Ok(BaseManifest::Image(serde_json::from_value(value)?))
        }
        other => bail!("unsupported base manifest media type {other:?}"),
    }
}

/// Digest of the entry for `platform` in a base index.
pub fn select_platform(index: &ImageIndex, platform: &Platform) -> Result<String> {
    index
        .manifests
        .iter()
        .find(|m| {
            m.platform
                .as_ref()
                .is_some_and(|p| p.os == platform.os && p.architecture == platform.architecture)
        })
        .map(|m| m.digest.clone())
        .with_context(|| format!("base image has no {platform} entry"))
}

/// Base layers re-labelled with OCI media types (same bytes, same digests).
pub fn oci_layers(base: &ImageManifest) -> Result<Vec<Descriptor>> {
    if !matches!(base.config.media_type.as_str(), OCI_CONFIG | DOCKER_CONFIG) {
        bail!(
            "unsupported base config media type {}",
            base.config.media_type
        );
    }
    base.layers
        .iter()
        .map(|layer| {
            let media_type = match layer.media_type.as_str() {
                DOCKER_LAYER_GZIP => OCI_LAYER_GZIP,
                other if other.starts_with("application/vnd.oci.image.layer.") => other,
                other => bail!("unsupported base layer media type {other}"),
            };
            Ok(Descriptor {
                media_type: media_type.into(),
                annotations: None,
                platform: None,
                ..layer.clone()
            })
        })
        .collect()
}

pub fn image_manifest(config: Descriptor, layers: Vec<Descriptor>) -> Result<Vec<u8>> {
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some(OCI_MANIFEST.into()),
        config,
        layers,
    };
    serde_json::to_vec(&manifest).context("serialize manifest")
}

pub fn image_index(manifests: Vec<Descriptor>) -> Result<Vec<u8>> {
    let index = ImageIndex {
        schema_version: 2,
        media_type: Some(OCI_INDEX.into()),
        manifests,
    };
    serde_json::to_vec(&index).context("serialize index")
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix timestamp (UTC).
pub fn rfc3339(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let secs = epoch % 86_400;
    // Civil-from-days (Howard Hinnant), valid for the whole u64 range we see.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
