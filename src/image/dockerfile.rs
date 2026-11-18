use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::image_tags::{TagContext, resolve_tags};
fn clean_relative(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("{field} must be a non-empty relative workspace path");
    }
    if path.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        bail!(
            "{field} must not contain parent traversal: {}",
            path.display()
        );
    }
    Ok(())
}

use super::reference::Reference;
use super::release_metadata::{ChosenBinary, ReleaseMetadata};
use crate::project::Project;

pub fn run(
    project: &Project,
    dist: &Path,
    metadata: &ReleaseMetadata,
    chosen: &ChosenBinary,
    cli_tags: &[String],
    registry: Option<&str>,
    push: bool,
) -> Result<()> {
    let image = &project.config.image;
    let dockerfile = image.dockerfile.as_ref().context("[image].dockerfile")?;
    clean_relative(dockerfile, "[image].dockerfile")?;
    for path in &image.extra_files {
        clean_relative(path, "[image].extra_files")?;
    }
    if image.platforms.len() != 1 {
        bail!("Dockerfile images currently require exactly one [image].platforms entry");
    }
    let platform = &image.platforms[0];
    let artifact = metadata
        .artifacts
        .iter()
        .find(|a| {
            a.kind == "binary"
                && chosen.matches(a)
                && a.target.as_deref().is_some_and(|t| {
                    super::reference::Platform::for_target(t)
                        .is_some_and(|p| p.to_string() == *platform)
                })
        })
        .with_context(|| format!("no {} binary for {platform}", chosen.bin))?;

    let context = dist.join("docker-context");
    if context.exists() {
        std::fs::remove_dir_all(&context)?;
    }
    std::fs::create_dir_all(&context)?;
    copy_path(
        &project.graph.root.join(dockerfile),
        &context.join(dockerfile),
    )?;
    for rel in &image.extra_files {
        copy_path(&project.graph.root.join(rel), &context.join(rel))?;
    }
    std::fs::copy(dist.join(&artifact.path), context.join(&chosen.bin))
        .with_context(|| format!("stage {}", chosen.bin))?;

    let tags = resolve_tags(
        cli_tags,
        &image.tags,
        &TagContext {
            version: &metadata.version,
            tag: metadata.tag.as_deref(),
            commit: metadata.commit.as_deref(),
            skip_latest: metadata.is_snapshot() || metadata.prerelease.unwrap_or(false),
        },
    )?;
    let mut target = Reference::parse(
        image
            .name
            .as_deref()
            .context("set [image].name for Dockerfile builds")?,
    )?;
    if let Some(host) = registry {
        target.registry = host.to_string();
    }
    let repository = format!("{}/{}", target.registry, target.repository);

    let cache = super::blob_cache_dir(dist).join("buildkit");
    let next = cache.with_extension("next");
    if next.exists() {
        std::fs::remove_dir_all(&next)?;
    }
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "build", "--platform", platform, "--file"])
        .arg(context.join(dockerfile));
    if cache.is_dir() {
        cmd.arg("--cache-from")
            .arg(format!("type=local,src={}", cache.display()));
    }
    cmd.arg("--cache-to")
        .arg(format!("type=local,dest={},mode=max", next.display()));
    for tag in &tags {
        cmd.arg("--tag").arg(format!("{repository}:{tag}"));
    }
    cmd.arg(if push { "--push" } else { "--load" })
        .arg(&context);
    let status = cmd.status().context("run docker buildx build")?;
    if !status.success() {
        bail!("docker buildx build failed ({status})");
    }
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
    }
    if next.exists() {
        std::fs::rename(&next, &cache)?;
    }
    Ok(())
}

fn copy_path(source: &Path, dest: &Path) -> Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else if source.is_file() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, dest)?;
    } else {
        bail!("Docker context input {} does not exist", source.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_context_paths_stay_inside_workspace() {
        clean_relative(Path::new("build/Dockerfile"), "dockerfile").unwrap();
        clean_relative(Path::new("assets"), "extra").unwrap();
        assert!(clean_relative(Path::new("../Dockerfile"), "dockerfile").is_err());
        assert!(clean_relative(Path::new("assets/../../secret"), "extra").is_err());
        assert!(clean_relative(Path::new("/tmp/Dockerfile"), "dockerfile").is_err());
        assert!(clean_relative(Path::new(""), "extra").is_err());
    }
}
