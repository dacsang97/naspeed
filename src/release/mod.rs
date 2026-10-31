//! `ns release`: run `before` hooks, build every build × target, archive,
//! write SBOMs and the checksum file, sign, publish.
//!
//! # `dist/` contract (shared with `ns image` and CI)
//!
//! * The dist dir defaults to `<workspace root>/dist`. It is wiped at the
//!   start of every run, but only when it is empty or carries the `.ns-dist`
//!   marker; any other non-empty directory is refused.
//! * Builds: `[[release.builds]]`, or one plain build per `release.bins`
//!   entry (default: every workspace binary). Each gets `NS_VERSION`,
//!   `NS_TAG`, `NS_COMMIT`, `SOURCE_DATE_EPOCH` and its own `env`.
//! * `dist/<target>/<binary>[.exe]`: raw binaries under their shipped name.
//! * `dist/<archive.name>.tar.gz` (`.zip` for Windows by default): one per
//!   build and target, holding a single top dir of the same base name with
//!   the binary and every `archive.files` entry. Archives are reproducible:
//!   mtime = `SOURCE_DATE_EPOCH`, uid/gid 0, sorted entries, mode 0755 for the
//!   binary and directories, 0644 otherwise.
//! * `dist/<binary>-<version>.spdx.json`: SPDX 2.3 SBOM per build (unless
//!   `sbom = "none"`).
//! * `dist/<release.checksum>` (default `SHA256SUMS`): `sha256sum -c` format,
//!   archives and SBOMs; `<name>.sig` + `<name>.pem` when cosign signed it.
//! * `dist/metadata.json` (v2):
//!   `{"project", "version", "tag": str|null, "commit": str|null, "snapshot",
//!     "prerelease", "source_date_epoch", "artifacts": [{"kind": "binary"|
//!     "archive"|"checksums"|"signature"|"sbom", "build": id|null, "bin":
//!     binary|null, "target": str|null, "path": "<relative to dist>",
//!     "sha256"}]}`. `project` is the root package of the workspace, else the
//!   first build's binary. Binaries and archives are listed per target (matrix
//!   order) and build, then SBOMs, checksums, signatures.
//!
//! Version: the `vX.Y.Z` tag on HEAD, which must equal the version of every
//! package owning a release binary; `--snapshot` gives
//! `<package version>-SNAPSHOT-<short sha|nogit>`. `SOURCE_DATE_EPOCH` is
//! taken from the environment, else the HEAD commit time, else 0.

pub mod archive;
pub mod build_plan;
pub mod changelog;
pub mod checksums;
pub mod cosign;
pub mod dist;
pub mod git_info;
pub mod github_release;
pub mod name_template;
mod package;
mod publish;
pub mod sbom;
pub mod target_matrix;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::project::Project;
use crate::project::ns_config::SbomMode;
use dist::{Kind, Metadata};
use git_info::GitState;

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    /// Local build with a `-SNAPSHOT-<sha>` version; nothing is published.
    #[arg(long)]
    pub snapshot: bool,
    /// Create a GitHub Release and upload the assets (needs GITHUB_TOKEN).
    #[arg(long)]
    pub publish: bool,
    /// With --publish: create the release as a draft (also `release.draft`).
    #[arg(long, requires = "publish")]
    pub draft: bool,
    /// Build only these targets (repeatable); default: each build's targets.
    #[arg(long = "target", value_name = "TRIPLE")]
    pub targets: Vec<String>,
    /// Output directory; default `<workspace root>/dist`.
    #[arg(long)]
    pub dist_dir: Option<PathBuf>,
    /// Targets built at the same time.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
    pub jobs: u16,
    /// Sign with cosign even without a CI OIDC identity (interactive flow).
    #[arg(long)]
    pub sign: bool,
    /// GitHub repository `owner/name`; default `GITHUB_REPOSITORY`, else the
    /// remote's URL.
    #[arg(long)]
    pub repo: Option<String>,
    /// Git remote the tag must already be pushed to.
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

pub fn run(args: ReleaseArgs) -> Result<u8> {
    if args.publish && args.snapshot {
        bail!("--publish cannot be combined with --snapshot");
    }
    let project = Project::load_cwd()?;
    let graph = &project.graph;
    let cfg = &project.config.release;
    let builds = build_plan::resolve(&project, &args.targets)?;
    let targets = build_plan::all_targets(&builds);
    target_matrix::check_installed(&targets, &crate::cross::installed_targets())?;
    checksums::check_file_name(&cfg.checksum)?;
    let exclude = changelog::compile(&cfg.changelog)?;

    let state = GitState::read(&graph.root);
    let version = git_info::resolve_version(&state, &builds[0].version, args.snapshot)?;
    let sde_env = std::env::var("SOURCE_DATE_EPOCH").ok();
    let sde = git_info::source_date_epoch(sde_env.as_deref(), &state)?;
    let project_name = build_plan::project_name(&project, &builds);
    let tag = version.tag.as_deref();
    let prerelease = cfg
        .prerelease
        .resolve(tag.unwrap_or(version.version.as_str()));
    let dist_dir = args
        .dist_dir
        .clone()
        .unwrap_or_else(|| graph.root.join("dist"));
    let pack = package::PackageInput {
        root: &graph.root,
        archive: &cfg.archive,
        builds: &builds,
        targets: &targets,
        project: &project_name,
        version: &version.version,
        tag,
        dist_dir: &dist_dir,
        source_date_epoch: sde,
    };
    // Template errors and name clashes surface before any build.
    package::archive_names(&pack)?;
    // Every publish precondition is checked before spending time on builds.
    let publish = match args.publish {
        true => Some(publish::plan(&args, &graph.root, &state, &version)?),
        false => None,
    };

    dist::prepare(&dist_dir)?;
    let release_start = std::time::Instant::now();
    crate::ui_info!("starting release";
        project = project_name, version = version.version, targets = targets.join(","));
    let _release = crate::ui::indent();
    let env = build_plan::base_env(&version.version, tag, state.commit.as_deref(), sde);
    build_plan::run_hooks(&cfg.before, &graph.root, &env)?;
    let manifest_path = graph.root.join("Cargo.toml");
    let build_start = std::time::Instant::now();
    crate::ui_info!("building binaries");
    let building = crate::ui::indent();
    target_matrix::build_all(
        &target_matrix::MatrixInput {
            manifest_path: manifest_path.clone(),
            target_dir: &graph.target_dir,
            profile: &cfg.profile,
            builds: &builds,
            base_env: &env,
            dist_dir: &dist_dir,
        },
        &targets,
        usize::from(args.jobs),
    )?;
    crate::ui::took(build_start);
    drop(building);

    let package::Packaged {
        mut artifacts,
        mut sums,
    } = package::package_all(&pack)?;
    let mut uploads: Vec<PathBuf> = sums.iter().map(|(n, _)| dist_dir.join(n)).collect();

    if cfg.sbom == SbomMode::Spdx {
        for build in &builds {
            let info = sbom::SbomInfo {
                binary: build.binary(),
                version: &version.version,
                commit: state.commit.as_deref(),
                source_date_epoch: sde,
            };
            let name = sbom::write(&manifest_path, build, &info, &dist_dir)?;
            let a = package::artifact(
                Kind::Sbom,
                Some(build.id()),
                Some(build.binary()),
                None,
                &dist_dir,
                &name,
            )?;
            sums.push((name.clone(), a.sha256.clone()));
            artifacts.push(a);
            uploads.push(dist_dir.join(name));
        }
    }

    let sums_path = dist_dir.join(&cfg.checksum);
    std::fs::write(&sums_path, checksums::render(&sums))
        .with_context(|| format!("write {}", sums_path.display()))?;
    artifacts.push(package::artifact(
        Kind::Checksums,
        None,
        None,
        None,
        &dist_dir,
        &cfg.checksum,
    )?);
    uploads.push(sums_path.clone());

    let has_env = |v: &str| std::env::var_os(v).is_some_and(|x| !x.is_empty());
    match cosign::decide(
        cfg.sign,
        args.sign,
        cosign::find_on_path("cosign"),
        &has_env,
    )? {
        cosign::Decision::Skip(reason) => crate::ui_info!(format!("signing skipped: {reason}")),
        cosign::Decision::Sign(bin) => {
            let (sig, pem) = cosign::sign_blob(&bin, &sums_path)?;
            for p in [sig, pem] {
                let rel = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .context("signature name")?;
                artifacts.push(package::artifact(
                    Kind::Signature,
                    None,
                    None,
                    None,
                    &dist_dir,
                    rel,
                )?);
                uploads.push(p);
            }
        }
    }

    Metadata {
        project: project_name.clone(),
        version: version.version.clone(),
        tag: version.tag.clone(),
        commit: state.commit.clone(),
        snapshot: args.snapshot,
        prerelease,
        source_date_epoch: sde,
        artifacts,
    }
    .write(&dist_dir)?;
    crate::ui_info!("artifacts"; dir = dist_dir.display());
    {
        let _files = crate::ui::indent();
        for file in &uploads {
            crate::ui_info!(file.file_name().unwrap_or_default().to_string_lossy());
        }
    }

    if let Some(plan) = publish {
        let options = publish::ReleaseOptions {
            draft: cfg.draft || args.draft,
            prerelease,
            sort: cfg.changelog.sort,
            exclude: &exclude,
        };
        publish::publish(&plan, &options, &graph.root, &uploads)?;
    }
    crate::ui::took(release_start);
    Ok(0)
}
