//! The OCI image layout written to `<dist>/oci/` on every run (also dry
//! runs): our blobs (never the base layers), the image index named once per
//! tag in `index.json`, and the SBOM artifact under its `sha256-….sbom` tag.

use std::path::Path;

use anyhow::{Context, Result};

use super::blob_push::BuiltPlatform;
use super::manifest::{self, Descriptor, OCI_INDEX, OCI_MANIFEST};
use super::sbom_attach::{self, Sbom};

pub fn write_layout(
    dir: &Path,
    built: &[BuiltPlatform],
    index: &[u8],
    index_digest: &str,
    tags: &[String],
    sbom: Option<&Sbom>,
) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("clear {}", dir.display()))?;
    }
    let blobs = dir.join("blobs/sha256");
    std::fs::create_dir_all(&blobs).with_context(|| format!("create {}", blobs.display()))?;
    let write_blob = |digest: &str, bytes: &[u8]| {
        let path = blobs.join(digest.trim_start_matches("sha256:"));
        std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
    };
    for p in built {
        write_blob(&p.layer_digest, &p.layer_blob)?;
        write_blob(&p.config_digest, &p.config)?;
        write_blob(&p.manifest_digest, &p.manifest)?;
    }
    write_blob(index_digest, index)?;
    let named = |media_type: &str, digest: &str, size: usize, name: Option<&str>| {
        let mut d = Descriptor::new(media_type, digest, size as u64);
        d.annotations = name.map(|name| {
            [(
                "org.opencontainers.image.ref.name".to_string(),
                name.to_string(),
            )]
            .into()
        });
        d
    };
    let mut entries: Vec<Descriptor> = tags
        .iter()
        .map(|tag| named(OCI_INDEX, index_digest, index.len(), Some(tag)))
        .collect();
    if entries.is_empty() {
        entries.push(named(OCI_INDEX, index_digest, index.len(), None));
    }
    if let Some(sbom) = sbom {
        for (digest, bytes) in sbom_attach::blobs(sbom) {
            write_blob(&digest, bytes)?;
        }
        write_blob(&sbom.manifest_digest, &sbom.manifest)?;
        entries.push(named(
            OCI_MANIFEST,
            &sbom.manifest_digest,
            sbom.manifest.len(),
            Some(&sbom.tag),
        ));
    }
    std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)
        .with_context(|| format!("write {}", dir.join("oci-layout").display()))?;
    std::fs::write(dir.join("index.json"), manifest::image_index(entries)?)
        .with_context(|| format!("write {}", dir.join("index.json").display()))
}
