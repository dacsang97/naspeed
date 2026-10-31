//! Push order: base layers (already there → skip; same registry → mount;
//! other registry → stream through the `<store>/oci/` digest cache), the new
//! layer and config, every platform manifest by digest, then the index under
//! each tag (or by digest when no tag applies). Counters record what actually crossed the wire.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::manifest::{Descriptor, OCI_INDEX, OCI_MANIFEST};
use super::reference::{Reference, is_loopback};
use super::registry_client::{RegistryClient, sha256_digest};

/// Everything built for one platform, ready to push or to write out.
pub struct BuiltPlatform {
    pub base: Reference,
    pub base_layers: Vec<Descriptor>,
    pub layer_blob: Vec<u8>,
    pub layer_digest: String,
    pub config: Vec<u8>,
    pub config_digest: String,
    pub manifest: Vec<u8>,
    pub manifest_digest: String,
}

/// One client per registry, created on first use.
pub struct Clients {
    by_registry: HashMap<String, RegistryClient>,
    /// Registry that `--insecure` applies to (the push target).
    insecure_registry: Option<String>,
}

impl Clients {
    pub fn new(insecure_registry: Option<String>) -> Clients {
        Clients {
            by_registry: HashMap::new(),
            insecure_registry,
        }
    }

    pub fn get(&mut self, registry: &str) -> Result<&RegistryClient> {
        if !self.by_registry.contains_key(registry) {
            let insecure =
                is_loopback(registry) || self.insecure_registry.as_deref() == Some(registry);
            self.by_registry.insert(
                registry.to_string(),
                RegistryClient::new(registry, insecure)?,
            );
        }
        Ok(&self.by_registry[registry])
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PushStats {
    pub base_present: u32,
    pub base_mounted: u32,
    pub base_copied: u32,
    pub base_copied_bytes: u64,
    pub new_uploaded: u32,
    pub new_present: u32,
}

impl fmt::Display for PushStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "base layers: {} copied ({} bytes), {} mounted, {} already present; new blobs: {} uploaded, {} already present",
            self.base_copied,
            self.base_copied_bytes,
            self.base_mounted,
            self.base_present,
            self.new_uploaded,
            self.new_present
        )
    }
}

pub fn push(
    clients: &mut Clients,
    target: &Reference,
    built: &[BuiltPlatform],
    index: &[u8],
    tags: &[String],
    cache_dir: &Path,
) -> Result<PushStats> {
    let mut stats = PushStats::default();
    let repo = target.repository.as_str();
    let mut done = HashSet::new();
    for platform in built {
        for layer in &platform.base_layers {
            if done.insert(layer.digest.clone()) {
                copy_base_layer(
                    clients,
                    target,
                    &platform.base,
                    layer,
                    cache_dir,
                    &mut stats,
                )?;
            }
        }
        let client = clients.get(&target.registry)?;
        for (digest, bytes) in [
            (&platform.layer_digest, &platform.layer_blob),
            (&platform.config_digest, &platform.config),
        ] {
            if !done.insert(digest.clone()) {
                continue;
            }
            if client.blob_exists(repo, digest)? {
                stats.new_present += 1;
            } else {
                client.upload_blob_bytes(repo, digest, bytes)?;
                stats.new_uploaded += 1;
            }
        }
        client.put_manifest(
            repo,
            &platform.manifest_digest,
            OCI_MANIFEST,
            &platform.manifest,
        )?;
    }
    let client = clients.get(&target.registry)?;
    for tag in tags {
        client.put_manifest(repo, tag, OCI_INDEX, index)?;
        eprintln!("ns image: pushed {}/{repo}:{tag}", target.registry);
    }
    if tags.is_empty() {
        // e.g. a snapshot with tags = ["{tag}", "latest"]: reachable by digest only.
        let digest = client.put_manifest(repo, &sha256_digest(index), OCI_INDEX, index)?;
        eprintln!(
            "ns image: pushed {}/{repo}@{digest} (no tags)",
            target.registry
        );
    }
    Ok(stats)
}

fn copy_base_layer(
    clients: &mut Clients,
    target: &Reference,
    base: &Reference,
    layer: &Descriptor,
    cache_dir: &Path,
    stats: &mut PushStats,
) -> Result<()> {
    let repo = target.repository.as_str();
    let digest = layer.digest.as_str();
    let mut session = None;
    {
        let client = clients.get(&target.registry)?;
        if client.blob_exists(repo, digest)? {
            stats.base_present += 1;
            return Ok(());
        }
        if base.api_host() == target.api_host() {
            match client.mount_blob(repo, digest, &base.repository)? {
                None => {
                    stats.base_mounted += 1;
                    return Ok(());
                }
                Some(location) => session = Some(location),
            }
        }
    }
    let cached = cached_blob(
        clients.get(&base.registry)?,
        &base.repository,
        layer,
        cache_dir,
    )?;
    clients
        .get(&target.registry)?
        .upload_blob_file(repo, digest, &cached, session)?;
    stats.base_copied += 1;
    stats.base_copied_bytes += layer.size;
    Ok(())
}

/// `<cache>/sha256/<hex>`, downloaded (and digest-checked) on first use.
fn cached_blob(
    client: &RegistryClient,
    repo: &str,
    layer: &Descriptor,
    cache_dir: &Path,
) -> Result<PathBuf> {
    let hex = layer
        .digest
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported digest algorithm in {}", layer.digest))?;
    let dir = cache_dir.join("sha256");
    let path = dir.join(hex);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == layer.size) {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let (tmp_path, mut file) = tempfile_in(&dir)?;
    eprintln!(
        "ns image: fetching base layer {} ({} bytes) from {}",
        layer.digest,
        layer.size,
        client.registry()
    );
    let fetched = client
        .get_blob_into(repo, &layer.digest, &mut file, layer.size.saturating_add(1))
        .and_then(|_| file.sync_all().context("flush cached blob"))
        .and_then(|_| {
            std::fs::rename(&tmp_path, &path).with_context(|| format!("store {}", path.display()))
        });
    if let Err(err) = fetched {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(path)
}

/// A uniquely named temp file next to the cache entry (renamed atomically).
fn tempfile_in(dir: &Path) -> Result<(PathBuf, std::fs::File)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let path = dir.join(format!(".tmp-{}-{nanos}", std::process::id()));
    let file =
        std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    Ok((path, file))
}
