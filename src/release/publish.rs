//! `--publish`: preconditions (token, repository, clean tree, tag pushed)
//! checked before any build, then the GitHub Release with every asset.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;

use super::ReleaseArgs;
use super::changelog;
use super::git_info::{self, GitState, Version};
use super::github_release::{API_ENV, DEFAULT_API, GitHub, NewRelease};
use crate::project::ns_config::ChangelogSort;

/// Everything `--publish` needs, validated up front.
pub struct PublishPlan {
    repo: String,
    tag: String,
    token: String,
}

pub fn plan(
    args: &ReleaseArgs,
    root: &Path,
    state: &GitState,
    version: &Version,
) -> Result<PublishPlan> {
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .context("--publish needs GITHUB_TOKEN in the environment")?;
    let repo = args
        .repo
        .clone()
        .or_else(|| {
            std::env::var("GITHUB_REPOSITORY")
                .ok()
                .filter(|r| !r.is_empty())
        })
        .or_else(|| {
            git_info::remote_url(root, &args.remote).and_then(|u| git_info::github_slug(&u))
        })
        .context(
            "cannot tell the GitHub repository; pass --repo owner/name or set GITHUB_REPOSITORY",
        )?;
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        bail!("--repo must look like owner/name, got {repo:?}");
    }
    let tag = version
        .tag
        .clone()
        .context("--publish needs a release tag on HEAD")?;
    let head = state
        .commit
        .as_deref()
        .context("--publish needs a git repository")?;
    git_info::ensure_clean(root)?;
    git_info::ensure_tag_on_remote(root, &args.remote, &tag, head)?;
    Ok(PublishPlan { repo, tag, token })
}

/// How the release is presented on GitHub.
pub struct ReleaseOptions<'a> {
    pub draft: bool,
    pub prerelease: bool,
    pub sort: ChangelogSort,
    pub exclude: &'a [Regex],
}

pub fn publish(
    plan: &PublishPlan,
    options: &ReleaseOptions,
    root: &Path,
    uploads: &[PathBuf],
) -> Result<()> {
    let commits = git_info::changelog_commits(root, &plan.tag)?;
    let body = changelog::render(&commits, options.sort, options.exclude);
    let api = std::env::var(API_ENV).unwrap_or_else(|_| DEFAULT_API.into());
    let gh = GitHub::new(&api, plan.token.clone());
    let (html_url, upload_url) = gh.create_release(&NewRelease {
        repo: &plan.repo,
        tag: &plan.tag,
        body: &body,
        draft: options.draft,
        prerelease: options.prerelease,
    })?;
    for file in uploads {
        gh.upload_asset(&upload_url, file)?;
        crate::ui_info!("uploaded"; name = file.file_name().unwrap_or_default().to_string_lossy());
    }
    crate::ui_info!("published GitHub release"; url = html_url);
    Ok(())
}
