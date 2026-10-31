//! SBOM attached to the image as an OCI artifact manifest
//! (`artifactType: text/spdx+json`, the empty config `{}`, one SPDX JSON
//! layer) whose `subject` is the image index. It is pushed by digest, which
//! registries with the referrers API index automatically, and also tagged
//! `sha256-<index hex>.sbom` for registries without it (cosign's scheme).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::image_inputs::read_verified;
use super::manifest::{Descriptor, OCI_MANIFEST};
use super::registry_client::{RegistryClient, sha256_digest};
use super::release_metadata::{ChosenBinary, ReleaseMetadata};

pub const SPDX_JSON: &str = "text/spdx+json";
pub const OCI_EMPTY: &str = "application/vnd.oci.empty.v1+json";
pub const EMPTY_BLOB: &[u8] = b"{}";

/// The SBOM document and the artifact manifest that refers to the image.
pub struct Sbom {
    pub document: Vec<u8>,
    pub document_digest: String,
    pub manifest: Vec<u8>,
    pub manifest_digest: String,
    /// `sha256-<index hex>.sbom`.
    pub tag: String,
}

/// The SBOM file read from `path` must be JSON (SPDX JSON or another JSON SBOM).
pub fn check_document(path: &Path, bytes: &[u8]) -> Result<()> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .with_context(|| format!("SBOM {} is not valid JSON", path.display()))?;
    Ok(())
}

/// `--sbom`, else the SBOM `ns release` wrote for the chosen build (checked
/// against its sha256), else none. Returns the file name and contents.
pub fn source_document(
    explicit: Option<&Path>,
    disabled: bool,
    dist: &Path,
    metadata: Option<&ReleaseMetadata>,
    chosen: &ChosenBinary,
) -> Result<Option<(String, Vec<u8>)>> {
    if disabled {
        return Ok(None);
    }
    let (path, sha256) = match (explicit, metadata.and_then(|m| m.sbom_for(chosen))) {
        (Some(path), _) => (path.to_path_buf(), None),
        (None, Some(artifact)) => (dist.join(&artifact.path), artifact.sha256.as_deref()),
        (None, None) => return Ok(None),
    };
    let document = read_verified(&path, sha256).context("SBOM")?;
    check_document(&path, &document)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sbom.spdx.json")
        .to_string();
    Ok(Some((name, document)))
}

/// `sha256:<hex>` → `sha256-<hex>.sbom`.
pub fn sbom_tag(subject_digest: &str) -> Result<String> {
    let Some(hex) = subject_digest.strip_prefix("sha256:") else {
        bail!("unsupported digest {subject_digest}");
    };
    Ok(format!("sha256-{hex}.sbom"))
}

/// Artifact manifest for `document` (named `file_name`) about `subject`.
pub fn build(document: Vec<u8>, file_name: &str, subject: Descriptor) -> Result<Sbom> {
    let document_digest = sha256_digest(&document);
    let mut layer = Descriptor::new(SPDX_JSON, &document_digest, document.len() as u64);
    layer.annotations = Some(BTreeMap::from([(
        "org.opencontainers.image.title".to_string(),
        file_name.to_string(),
    )]));
    let tag = sbom_tag(&subject.digest)?;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "artifactType": SPDX_JSON,
        "config": Descriptor::new(OCI_EMPTY, &sha256_digest(EMPTY_BLOB), EMPTY_BLOB.len() as u64),
        "layers": [layer],
        "subject": subject,
    });
    let manifest = serde_json::to_vec(&manifest).context("serialize SBOM manifest")?;
    Ok(Sbom {
        manifest_digest: sha256_digest(&manifest),
        manifest,
        document_digest,
        document,
        tag,
    })
}

/// Blobs (digest, bytes) the SBOM manifest references.
pub fn blobs(sbom: &Sbom) -> [(String, &[u8]); 2] {
    [
        (sha256_digest(EMPTY_BLOB), EMPTY_BLOB),
        (sbom.document_digest.clone(), &sbom.document),
    ]
}

/// Upload the blobs, then PUT the manifest by digest and under its tag.
pub fn push(client: &RegistryClient, repo: &str, sbom: &Sbom) -> Result<()> {
    for (digest, bytes) in blobs(sbom) {
        if !client.blob_exists(repo, &digest)? {
            client.upload_blob_bytes(repo, &digest, bytes)?;
        }
    }
    client
        .put_manifest(repo, &sbom.manifest_digest, OCI_MANIFEST, &sbom.manifest)
        .context("push SBOM manifest")?;
    client
        .put_manifest(repo, &sbom.tag, OCI_MANIFEST, &sbom.manifest)
        .context("tag SBOM manifest")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::super::manifest::OCI_INDEX;
    use super::*;

    #[test]
    fn manifest_refers_to_the_index_as_subject() {
        let index_digest = format!("sha256:{}", "ab".repeat(32));
        let subject = Descriptor::new(OCI_INDEX, &index_digest, 1234);
        let doc = br#"{"spdxVersion":"SPDX-2.3"}"#.to_vec();
        let sbom = build(doc.clone(), "api-1.0.0.spdx.json", subject).unwrap();
        assert_eq!(sbom.tag, format!("sha256-{}.sbom", "ab".repeat(32)));
        assert_eq!(sbom.manifest_digest, sha256_digest(&sbom.manifest));
        let manifest: Value = serde_json::from_slice(&sbom.manifest).unwrap();
        assert_eq!(
            manifest,
            json!({
                "schemaVersion": 2,
                "mediaType": OCI_MANIFEST,
                "artifactType": "text/spdx+json",
                "config": {
                    "mediaType": "application/vnd.oci.empty.v1+json",
                    "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                    "size": 2
                },
                "layers": [{
                    "mediaType": "text/spdx+json",
                    "digest": sha256_digest(&doc),
                    "size": doc.len(),
                    "annotations": {"org.opencontainers.image.title": "api-1.0.0.spdx.json"}
                }],
                "subject": {"mediaType": OCI_INDEX, "digest": index_digest, "size": 1234}
            })
        );
        let digests: Vec<String> = blobs(&sbom).into_iter().map(|(d, _)| d).collect();
        assert_eq!(digests[1], sha256_digest(&doc));
    }

    #[test]
    fn rejects_non_json_documents_and_odd_digests() {
        assert!(check_document(Path::new("x"), b"not json").is_err());
        assert!(check_document(Path::new("x"), b"{}").is_ok());
        assert!(sbom_tag("sha512:00").is_err());
    }
}
