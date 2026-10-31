//! `ns image`: multi-arch OCI image from prebuilt binaries, pushed through the
//! registry HTTP API, with no Docker daemon.
//!
//! Per platform the base manifest and config are fetched (never its layers,
//! unless a cross-registry push has to copy them), one layer holding the
//! binary at `[image].path` is added, and the config gains its diff_id and
//! the Dockerfile-style runtime settings of `[image]` (user, entrypoint, cmd,
//! workdir, env, ports, volumes, labels). The result is always written as an
//! OCI layout under `<dist>/oci/`; `--push` also uploads it, plus the
//! release's SBOM as an artifact referring to the index. The last stdout line
//! is `<repo>@<index digest>` (for `cosign sign`).

mod blob_push;
mod build_provenance;
mod image_config;
mod image_inputs;
mod image_tags;
mod layer_builder;
mod manifest;
mod oci_layout;
mod platform_image;
mod reference;
mod registry_auth;
mod registry_client;
mod registry_transport;
mod release_metadata;
mod sbom_attach;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use self::blob_push::Clients;
use self::build_provenance::{LabelInputs, ReleaseInfo, labels, source_date_epoch};
use self::image_config::RuntimeConfig;
use self::image_inputs::{parse_binary_flag, resolve_platforms};
use self::image_tags::{TagContext, resolve_tags};
use self::manifest::{Descriptor, OCI_INDEX};
use self::platform_image::{BaseCache, ImageSpec, build_platform};
use self::reference::Reference;
use self::registry_client::sha256_digest;
use self::release_metadata::{ReleaseMetadata, choose_from_metadata, choose_without_metadata};
use crate::project::Project;

#[derive(Debug, Args)]
pub struct ImageArgs {
    /// Push to this registry instead of the one in `[image].name` (e.g. `localhost:5000`).
    #[arg(long)]
    pub registry: Option<String>,
    /// Tag to push (repeatable), replacing `[image].tags`
    /// (default `{version}` plus `latest` unless a snapshot or pre-release).
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Directory holding `ns release` output (`metadata.json`). Default: `<workspace>/dist`.
    #[arg(long)]
    pub dist_dir: Option<PathBuf>,
    /// Upload to the registry. Without it the image is only written to `<dist>/oci`.
    #[arg(long)]
    pub push: bool,
    /// Use plain http for the target registry (implied for localhost / 127.0.0.1).
    #[arg(long)]
    pub insecure: bool,
    /// Binary for a platform instead of metadata.json (repeatable),
    /// e.g. `linux/amd64=target/x86_64-unknown-linux-musl/release/api`.
    #[arg(long = "binary", value_name = "PLATFORM=PATH")]
    pub binaries: Vec<String>,
    /// SBOM (JSON) to attach instead of the one `ns release` wrote for the build.
    #[arg(long, value_name = "FILE", conflicts_with = "no_sbom")]
    pub sbom: Option<PathBuf>,
    /// Do not attach an SBOM.
    #[arg(long)]
    pub no_sbom: bool,
}

pub fn run(args: ImageArgs) -> Result<u8> {
    let project = Project::load_cwd()?;
    let root = project.graph.root.clone();
    let dist = args.dist_dir.clone().unwrap_or_else(|| root.join("dist"));
    let flags = args
        .binaries
        .iter()
        .map(|b| parse_binary_flag(b))
        .collect::<Result<Vec<_>>>()?;
    let metadata = if flags.is_empty() {
        ReleaseMetadata::load(&dist)?
    } else {
        None
    };
    let chosen = match &metadata {
        Some(meta) => choose_from_metadata(meta, &project.config)?,
        None => {
            let flag_paths: Vec<PathBuf> = flags.iter().map(|(_, p)| p.clone()).collect();
            let bins: Vec<String> = project
                .graph
                .bins()
                .into_iter()
                .map(|(_, bin)| bin.to_string())
                .collect();
            choose_without_metadata(&project.config, &flag_paths, &bins)?
        }
    };
    let inputs = resolve_platforms(&project, &dist, metadata.as_ref(), &flags, &chosen)?;
    let release = ReleaseInfo::new(&project, metadata.as_ref(), &chosen)?;
    let image = &project.config.image;
    let tags = resolve_tags(
        &args.tags,
        &image.tags,
        &TagContext {
            version: &release.version,
            tag: release.tag.as_deref(),
            commit: release.commit.as_deref(),
            skip_latest: release.snapshot || release.prerelease,
        },
    )?;
    let target = target_repository(image.name.as_deref(), args.registry.as_deref(), &chosen.bin)?;
    let epoch = source_date_epoch(&root, metadata.as_ref())?;
    let labels = labels(
        &root,
        &LabelInputs {
            title: &release.title,
            version: &release.version,
            commit: release.commit.as_deref(),
            created_epoch: epoch,
            extra: &image.labels,
        },
    );
    let runtime = RuntimeConfig::new(image, &chosen.bin)?;
    let sbom_document = sbom_attach::source_document(
        args.sbom.as_deref(),
        args.no_sbom,
        &dist,
        metadata.as_ref(),
        &chosen,
    )?;
    let spec = ImageSpec {
        runtime: &runtime,
        labels: &labels,
        epoch,
    };

    let mut clients = Clients::new(args.insecure.then(|| target.registry.clone()));
    let mut bases = BaseCache::default();
    let mut built = Vec::new();
    let mut entries = Vec::new();
    for input in &inputs {
        let (platform, entry) = build_platform(&mut clients, &mut bases, input, &spec)?;
        built.push(platform);
        entries.push(entry);
    }
    let index = manifest::image_index(entries)?;
    let index_digest = sha256_digest(&index);
    let sbom = sbom_document
        .map(|(name, document)| {
            let subject = Descriptor::new(OCI_INDEX, &index_digest, index.len() as u64);
            sbom_attach::build(document, &name, subject)
        })
        .transpose()?;
    let layout = dist.join("oci");
    oci_layout::write_layout(&layout, &built, &index, &index_digest, &tags, sbom.as_ref())?;
    crate::ui_info!("wrote OCI layout (base layers referenced by digest only)";
        path = layout.display());

    let repo_ref = format!("{}/{}", target.registry, target.repository);
    if args.push {
        let stats = blob_push::push(
            &mut clients,
            &target,
            &built,
            &index,
            &tags,
            &blob_cache_dir(&dist),
        )?;
        crate::ui_info!(stats.to_string());
        if let Some(sbom) = &sbom {
            sbom_attach::push(clients.get(&target.registry)?, &target.repository, sbom)?;
            crate::ui_info!("pushed SBOM";
                tag = format!("{repo_ref}:{}", sbom.tag),
                digest = format!("{repo_ref}@{}", sbom.manifest_digest));
        }
    } else {
        for tag in &tags {
            crate::ui_info!("would push (dry run; pass --push to upload)"; tag = format!("{repo_ref}:{tag}"));
        }
        if let Some(sbom) = &sbom {
            crate::ui_info!("would push SBOM"; tag = format!("{repo_ref}:{}", sbom.tag));
        }
    }
    println!("{repo_ref}@{index_digest}");
    Ok(0)
}

/// `[image].name` with its registry replaced by `--registry`; with no name, `<registry>/<bin>`.
fn target_repository(name: Option<&str>, registry: Option<&str>, bin: &str) -> Result<Reference> {
    if let Some(registry) = registry
        && (registry.is_empty() || registry.contains('/') || registry.contains("://"))
    {
        bail!("--registry takes host[:port], got {registry:?}");
    }
    let mut target = match (name, registry) {
        (Some(name), _) => Reference::parse(name).context("[image].name in ns.toml")?,
        (None, Some(registry)) => Reference::parse(&format!("{registry}/{bin}"))?,
        (None, None) => {
            bail!("set [image].name in ns.toml (e.g. ghcr.io/owner/{bin}) or pass --registry")
        }
    };
    if let Some(registry) = registry {
        target.registry = registry.to_string();
    }
    Ok(target)
}

/// Streamed base layers are cached in the naspeed store on the dist volume
/// (else the first store, else the config dir when no store is set up).
fn blob_cache_dir(dist: &Path) -> PathBuf {
    let settings = crate::config::Settings::load();
    crate::store::volume_root::store_for(dist, &settings.stores)
        .or_else(|| settings.stores.first().cloned())
        .unwrap_or_else(|| settings.config_dir.clone())
        .join("oci")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_override_replaces_only_the_host() {
        let t =
            target_repository(Some("ghcr.io/owner/api"), Some("localhost:5055"), "api").unwrap();
        assert_eq!(
            (t.registry.as_str(), t.repository.as_str()),
            ("localhost:5055", "owner/api")
        );
        let t = target_repository(None, Some("localhost:5055"), "api").unwrap();
        assert_eq!(t.repository, "api");
        assert!(target_repository(None, None, "api").is_err());
        assert!(target_repository(None, Some("http://x"), "api").is_err());
    }
}
