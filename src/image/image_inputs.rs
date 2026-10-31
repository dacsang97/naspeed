//! What goes into the image: one binary per platform (from `ns release`'s
//! `dist/metadata.json` or `--binary platform=path`) and its base image.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::layer_builder::{Linkage, inspect_elf};
use super::reference::{Platform, Reference};
use super::registry_client::hex;
use super::release_metadata::{ChosenBinary, ReleaseMetadata};
use crate::project::Project;

const STATIC_BASE: &str = "gcr.io/distroless/static-debian12:nonroot";
const CC_BASE: &str = "gcr.io/distroless/cc-debian12:nonroot";

/// One platform's binary and base image.
#[derive(Debug)]
pub struct PlatformInput {
    pub platform: Platform,
    pub binary: Vec<u8>,
    pub path: PathBuf,
    pub base: Reference,
}

/// Parse `linux/amd64=path` from `--binary`.
pub fn parse_binary_flag(text: &str) -> Result<(Platform, PathBuf)> {
    let (platform, path) = text
        .split_once('=')
        .with_context(|| format!("--binary {text:?}: expected <platform>=<path>"))?;
    Ok((Platform::parse(platform)?, PathBuf::from(path)))
}

/// Read an artifact, checking it against its metadata.json sha256 when given.
pub fn read_verified(path: &Path, sha256: Option<&str>) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if let Some(expected) = sha256 {
        let got = hex(&Sha256::digest(&bytes));
        if !got.eq_ignore_ascii_case(expected.trim_start_matches("sha256:")) {
            bail!(
                "{} does not match its sha256 in metadata.json (rebuild with ns release)",
                path.display()
            );
        }
    }
    Ok(bytes)
}

/// Resolve every configured platform to a binary; platforms without one are
/// skipped with a warning, and at least one must remain.
pub fn resolve_platforms(
    project: &Project,
    dist: &Path,
    metadata: Option<&ReleaseMetadata>,
    flags: &[(Platform, PathBuf)],
    chosen: &ChosenBinary,
) -> Result<Vec<PlatformInput>> {
    let bin = &chosen.bin;
    let mut wanted = Vec::new();
    for text in &project.config.image.platforms {
        let platform = Platform::parse(text).context("[image].platforms in ns.toml")?;
        if !wanted.contains(&platform) {
            wanted.push(platform);
        }
    }
    let base_override = project.config.image.base.as_deref();
    let mut out = Vec::new();
    for platform in wanted {
        let found = if flags.is_empty() {
            metadata.and_then(|meta| binary_from_metadata(meta, dist, chosen, &platform))
        } else {
            flags
                .iter()
                .find(|(p, _)| *p == platform)
                .map(|(_, path)| (path.clone(), None, None))
        };
        let Some((path, target, sha256)) = found else {
            eprintln!("ns image: no {bin} binary for {platform}; skipping that platform");
            continue;
        };
        let binary = read_verified(&path, sha256.as_deref())?;
        let linkage = inspect_elf(&binary, &platform.architecture)
            .with_context(|| format!("{} is not a Linux {platform} executable", path.display()))?;
        let base = match base_override {
            Some(base) => base,
            None => default_base(target.as_deref(), linkage),
        };
        let base = Reference::parse(base).context("base image")?;
        out.push(PlatformInput {
            platform,
            binary,
            path,
            base,
        });
    }
    if out.is_empty() {
        bail!(
            "no binary found for any of [image].platforms; run `ns release` with Linux targets or pass --binary"
        );
    }
    Ok(out)
}

/// The raw binary for `platform`, preferring musl over gnu builds.
fn binary_from_metadata(
    meta: &ReleaseMetadata,
    dist: &Path,
    chosen: &ChosenBinary,
    platform: &Platform,
) -> Option<(PathBuf, Option<String>, Option<String>)> {
    meta.artifacts
        .iter()
        .filter(|a| a.kind == "binary" && chosen.matches(a))
        .filter_map(|a| {
            let target = a.target.as_deref()?;
            (Platform::for_target(target).as_ref() == Some(platform)).then_some((a, target))
        })
        .min_by_key(|(_, target)| !target.ends_with("-musl"))
        .map(|(a, target)| {
            (
                dist.join(&a.path),
                Some(target.to_string()),
                a.sha256.clone(),
            )
        })
}

/// musl (or any static binary) → distroless/static; gnu → distroless/cc.
fn default_base(target: Option<&str>, linkage: Linkage) -> &'static str {
    match target {
        Some(t) if t.ends_with("-musl") => STATIC_BASE,
        Some(t) if t.ends_with("-gnu") => CC_BASE,
        _ if linkage == Linkage::Dynamic => CC_BASE,
        _ => STATIC_BASE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_prefers_musl_and_parses_flags() {
        let meta: ReleaseMetadata = serde_json::from_str(
            r#"{"project":"p","version":"1.0.0","commit":"abc","source_date_epoch":5,"artifacts":[
                {"kind":"archive","target":"x86_64-unknown-linux-musl","bin":"api","path":"a.tar.gz"},
                {"kind":"binary","target":"x86_64-unknown-linux-gnu","bin":"api","path":"x86_64-unknown-linux-gnu/api"},
                {"kind":"binary","target":"x86_64-unknown-linux-musl","bin":"api","path":"x86_64-unknown-linux-musl/api"},
                {"kind":"binary","target":"aarch64-apple-darwin","bin":"api","path":"aarch64-apple-darwin/api"}]}"#,
        )
        .unwrap();
        let api = ChosenBinary {
            bin: "api".into(),
            build: None,
        };
        let amd64 = Platform::parse("linux/amd64").unwrap();
        let (path, target, _) = binary_from_metadata(&meta, Path::new("/d"), &api, &amd64).unwrap();
        assert_eq!(path, Path::new("/d/x86_64-unknown-linux-musl/api"));
        assert_eq!(target.as_deref(), Some("x86_64-unknown-linux-musl"));
        let arm64 = Platform::parse("linux/arm64").unwrap();
        assert!(binary_from_metadata(&meta, Path::new("/d"), &api, &arm64).is_none());

        let (p, path) = parse_binary_flag("linux/arm64=target/x/hello").unwrap();
        assert_eq!(
            (p.to_string(), path),
            ("linux/arm64".into(), PathBuf::from("target/x/hello"))
        );
        assert!(parse_binary_flag("target/x/hello").is_err());
    }

    #[test]
    fn default_bases() {
        assert_eq!(
            default_base(Some("x86_64-unknown-linux-musl"), Linkage::Static),
            STATIC_BASE
        );
        assert_eq!(
            default_base(Some("aarch64-unknown-linux-gnu"), Linkage::Dynamic),
            CC_BASE
        );
        assert_eq!(default_base(None, Linkage::Dynamic), CC_BASE);
        assert_eq!(default_base(None, Linkage::Static), STATIC_BASE);
    }
}
