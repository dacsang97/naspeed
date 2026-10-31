//! Build one platform's image: resolve the base manifest for the platform,
//! fetch its config (layers stay remote), add the binary layer and derive the
//! new config and manifest.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};

use super::blob_push::{BuiltPlatform, Clients};
use super::image_config::{ConfigChanges, RuntimeConfig, derive_config};
use super::image_inputs::PlatformInput;
use super::layer_builder::build_layer;
use super::manifest::{
    self, BaseManifest, Descriptor, ImageManifest, OCI_CONFIG, OCI_LAYER_GZIP, OCI_MANIFEST,
    PlatformSpec,
};
use super::registry_client::sha256_digest;

/// Settings shared by every platform of one image.
pub struct ImageSpec<'a> {
    pub runtime: &'a RuntimeConfig,
    pub labels: &'a BTreeMap<String, String>,
    pub epoch: u64,
}

/// Base manifests already fetched, keyed by full reference.
#[derive(Default)]
pub struct BaseCache {
    manifests: HashMap<String, BaseManifest>,
}

pub fn build_platform(
    clients: &mut Clients,
    bases: &mut BaseCache,
    input: &PlatformInput,
    spec: &ImageSpec,
) -> Result<(BuiltPlatform, Descriptor)> {
    let base = &input.base;
    let client = clients.get(&base.registry)?;
    let key = base.to_string();
    if !bases.manifests.contains_key(&key) {
        let fetched = client
            .get_manifest(&base.repository, &base.reference)
            .with_context(|| format!("base image {base}"))?;
        let parsed = manifest::parse_base(&fetched.bytes, &fetched.media_type)
            .with_context(|| format!("base image {base}"))?;
        bases.manifests.insert(key.clone(), parsed);
    }
    let base_image: ImageManifest = match &bases.manifests[&key] {
        BaseManifest::Image(image) => image.clone(),
        BaseManifest::Index(index) => {
            let digest = manifest::select_platform(index, &input.platform)
                .with_context(|| format!("base image {base}"))?;
            let fetched = client.get_manifest(&base.repository, &digest)?;
            match manifest::parse_base(&fetched.bytes, &fetched.media_type)? {
                BaseManifest::Image(image) => image,
                BaseManifest::Index(_) => {
                    bail!("base image {base}: nested index for {}", input.platform)
                }
            }
        }
    };
    let base_config = client
        .get_blob_bytes(&base.repository, &base_image.config.digest)
        .with_context(|| format!("config of base image {base}"))?;
    let base_layers =
        manifest::oci_layers(&base_image).with_context(|| format!("base image {base}"))?;

    let layer = build_layer(&spec.runtime.path, &input.binary, spec.epoch)?;
    let config = derive_config(
        &base_config,
        &input.platform,
        &ConfigChanges {
            diff_id: &layer.diff_id,
            runtime: spec.runtime,
            labels: spec.labels,
            created_epoch: spec.epoch,
        },
    )
    .with_context(|| format!("config for {} on {base}", input.platform))?;
    let config_digest = sha256_digest(&config);
    let mut layers = base_layers.clone();
    layers.push(Descriptor::new(
        OCI_LAYER_GZIP,
        &layer.digest,
        layer.blob.len() as u64,
    ));
    let image_manifest = manifest::image_manifest(
        Descriptor::new(OCI_CONFIG, &config_digest, config.len() as u64),
        layers,
    )?;
    let manifest_digest = sha256_digest(&image_manifest);
    let mut entry = Descriptor::new(OCI_MANIFEST, &manifest_digest, image_manifest.len() as u64);
    entry.platform = Some(PlatformSpec {
        architecture: input.platform.architecture.clone(),
        os: input.platform.os.clone(),
        variant: None,
    });
    crate::ui_info!("built platform image";
        platform = input.platform, binary = input.path.display(), base = base,
        layer = layer.digest, bytes = layer.blob.len(), manifest = manifest_digest);
    let built = BuiltPlatform {
        base: base.clone(),
        base_layers,
        layer_blob: layer.blob,
        layer_digest: layer.digest,
        config,
        config_digest,
        manifest: image_manifest,
        manifest_digest,
    };
    Ok((built, entry))
}
