//! Pack one reproducible archive per build and target from the binaries the
//! matrix build left in `<dist>/<target>/`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::build_plan::ReleaseBuild;
use super::dist::{Artifact, Kind};
use super::name_template::{self, NameVars};
use super::{archive, checksums, target_matrix};
use crate::project::ns_config::ArchiveConfig;

/// What to pack and where.
pub struct PackageInput<'a> {
    pub root: &'a Path,
    pub archive: &'a ArchiveConfig,
    pub builds: &'a [ReleaseBuild],
    /// Matrix order of every target.
    pub targets: &'a [String],
    pub project: &'a str,
    pub version: &'a str,
    pub tag: Option<&'a str>,
    pub dist_dir: &'a Path,
    pub source_date_epoch: u64,
}

/// Binary and archive artifacts (per target, per build), and the
/// `(archive name, sha256)` pairs that go into the checksum file.
pub struct Packaged {
    pub artifacts: Vec<Artifact>,
    pub sums: Vec<(String, String)>,
}

/// `(build index, target, base name)` for every build × target in matrix
/// order. The base name is also the archive's top dir. Errors on unknown
/// placeholders and on two archives with one name.
pub fn archive_names(input: &PackageInput) -> Result<Vec<(usize, String, String)>> {
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for target in input.targets {
        for (i, build) in input.builds.iter().enumerate() {
            if !build.targets.contains(target) {
                continue;
            }
            let base = name_template::render(
                &input.archive.name,
                &NameVars {
                    project: input.project,
                    id: build.id(),
                    binary: build.binary(),
                    version: input.version,
                    tag: input.tag.unwrap_or_default(),
                    target,
                },
            )?;
            if !seen.insert(base.clone()) {
                bail!(
                    "archive.name {:?} gives {base:?} twice; add {{binary}} or {{id}}, and {{target}} or {{os}}_{{arch}}",
                    input.archive.name
                );
            }
            names.push((i, target.clone(), base));
        }
    }
    Ok(names)
}

pub fn package_all(input: &PackageInput) -> Result<Packaged> {
    let out = input.dist_dir;
    let extras = archive_extras(input.root, &input.archive.files)?;
    let mut artifacts = Vec::new();
    let mut sums = Vec::new();
    for (i, target, top) in archive_names(input)? {
        let build = &input.builds[i];
        let (id, binary) = (Some(build.id()), Some(build.binary()));
        let exe = target_matrix::exe_name(build.binary(), &target);
        let rel = format!("{target}/{exe}");
        if !out.join(&rel).is_file() {
            bail!("build {} for {target} left no {rel}", build.id());
        }
        artifacts.push(artifact(
            Kind::Binary,
            id,
            binary,
            Some(&target),
            out,
            &rel,
        )?);

        let format = match target_matrix::is_windows(&target) {
            true => input.archive.windows,
            false => input.archive.format,
        };
        let name = format!("{top}.{}", archive::extension(format));
        let mut entries = vec![archive::Entry {
            name: exe,
            source: out.join(&rel),
            executable: true,
        }];
        entries.extend(extras.iter().cloned());
        archive::write_archive(
            format,
            &out.join(&name),
            &top,
            &entries,
            input.source_date_epoch,
        )?;
        let a = artifact(Kind::Archive, id, binary, Some(&target), out, &name)?;
        sums.push((name, a.sha256.clone()));
        artifacts.push(a);
    }
    Ok(Packaged { artifacts, sums })
}

/// `archive.files` as archive entries; each must be a file in the workspace.
fn archive_extras(root: &Path, files: &[PathBuf]) -> Result<Vec<archive::Entry>> {
    files
        .iter()
        .map(|rel| {
            let source = root.join(rel);
            if !source.is_file() {
                bail!("archive.files entry {} is not a file", source.display());
            }
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            Ok(archive::Entry {
                name,
                source,
                executable: false,
            })
        })
        .collect()
}

/// Metadata entry for `<out_dir>/rel`, hashing the file as it is now.
pub fn artifact(
    kind: Kind,
    build: Option<&str>,
    bin: Option<&str>,
    target: Option<&str>,
    out_dir: &Path,
    rel: &str,
) -> Result<Artifact> {
    Ok(Artifact {
        kind,
        build: build.map(str::to_string),
        bin: bin.map(str::to_string),
        target: target.map(str::to_string),
        path: rel.to_string(),
        sha256: checksums::sha256_file(&out_dir.join(rel))
            .with_context(|| format!("hash {rel}"))?,
    })
}
