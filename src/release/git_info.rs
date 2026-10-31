//! Git facts a release needs: the `vX.Y.Z` tag on HEAD, the release version,
//! `SOURCE_DATE_EPOCH`, the changelog and the pre-publish checks.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Output of `git <args>` in `dir`, or `None` when git is missing, `dir` is
/// not in a repository, or the command fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like [`git`], but a failure is an error carrying git's stderr.
fn git_checked(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Debug, Clone)]
pub struct GitState {
    /// Full HEAD sha; `None` outside a repository or before the first commit.
    pub commit: Option<String>,
    pub short: Option<String>,
    /// HEAD committer time (unix seconds).
    pub commit_time: Option<u64>,
    /// Tags pointing at HEAD.
    pub head_tags: Vec<String>,
}

impl GitState {
    pub fn read(dir: &Path) -> GitState {
        let commit = git(dir, &["rev-parse", "--verify", "HEAD"]);
        let short = commit
            .as_ref()
            .and_then(|_| git(dir, &["rev-parse", "--short", "HEAD"]));
        let commit_time = commit
            .as_ref()
            .and_then(|_| git(dir, &["log", "-1", "--format=%ct", "HEAD"]))
            .and_then(|t| t.parse().ok());
        let head_tags = commit
            .as_ref()
            .and_then(|_| git(dir, &["tag", "--points-at", "HEAD"]))
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default();
        GitState {
            commit,
            short,
            commit_time,
            head_tags,
        }
    }
}

/// `v1.2.3` (optionally `v1.2.3-rc.1`) → `1.2.3…`; anything else → `None`.
pub fn tag_version(tag: &str) -> Option<&str> {
    let v = tag.strip_prefix('v')?;
    let core = v.split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    let ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then_some(v)
}

/// The release version and the tag it came from (`None` for snapshots).
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub version: String,
    pub tag: Option<String>,
}

/// Snapshot: `<package>-SNAPSHOT-<short sha|nogit>`. Otherwise the `vX.Y.Z`
/// tag on HEAD, which must equal the package version.
pub fn resolve_version(state: &GitState, package_version: &str, snapshot: bool) -> Result<Version> {
    if snapshot {
        let sha = state.short.as_deref().unwrap_or("nogit");
        return Ok(Version {
            version: format!("{package_version}-SNAPSHOT-{sha}"),
            tag: None,
        });
    }
    let tags: Vec<&String> = state
        .head_tags
        .iter()
        .filter(|t| tag_version(t).is_some())
        .collect();
    if tags.is_empty() {
        bail!(
            "HEAD has no vX.Y.Z tag; tag the release (git tag v{package_version}) or use --snapshot"
        );
    }
    match tags
        .iter()
        .find(|t| tag_version(t) == Some(package_version))
    {
        Some(tag) => Ok(Version {
            version: package_version.to_string(),
            tag: Some(tag.to_string()),
        }),
        None => bail!(
            "tag {} on HEAD does not match package version {package_version}",
            tags.iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `SOURCE_DATE_EPOCH` from the environment, else the HEAD commit time, else 0.
pub fn source_date_epoch(env: Option<&str>, state: &GitState) -> Result<u64> {
    match env {
        Some(v) => v
            .trim()
            .parse()
            .with_context(|| format!("SOURCE_DATE_EPOCH={v:?} is not unix seconds")),
        None => Ok(state.commit_time.unwrap_or(0)),
    }
}

/// `(short sha, subject)` of the commits in `<previous tag>..<tag>` (whole
/// history for a first tag), newest first.
pub fn changelog_commits(dir: &Path, tag: &str) -> Result<Vec<(String, String)>> {
    let prev = git(
        dir,
        &["describe", "--tags", "--abbrev=0", &format!("{tag}^")],
    );
    let range = match &prev {
        Some(p) => format!("{p}..{tag}"),
        None => tag.to_string(),
    };
    let log = git_checked(dir, &["log", "--format=%h%x09%s", &range])?;
    Ok(log
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(sha, subject)| (sha.to_string(), subject.to_string()))
        .collect())
}

/// `git status --porcelain` is empty (dist/ must be ignored by .gitignore).
pub fn ensure_clean(dir: &Path) -> Result<()> {
    let status = git_checked(dir, &["status", "--porcelain"])?;
    if !status.is_empty() {
        bail!("working tree is not clean (is dist/ in .gitignore?):\n{status}");
    }
    Ok(())
}

/// The tag exists on `remote` and points at HEAD there too.
pub fn ensure_tag_on_remote(dir: &Path, remote: &str, tag: &str, head: &str) -> Result<()> {
    let refname = format!("refs/tags/{tag}");
    let peeled = format!("{refname}^{{}}");
    let listing = git_checked(dir, &["ls-remote", "--tags", remote, &refname, &peeled])?;
    let mut direct = None;
    let mut deref = None;
    for line in listing.lines() {
        let mut cols = line.split_whitespace();
        let (Some(sha), Some(name)) = (cols.next(), cols.next()) else {
            continue;
        };
        if name == peeled {
            deref = Some(sha.to_string());
        } else if name == refname {
            direct = Some(sha.to_string());
        }
    }
    match deref.or(direct) {
        None => bail!("tag {tag} is not on remote {remote}; run `git push {remote} {tag}`"),
        Some(sha) if sha != head => {
            bail!("tag {tag} on remote {remote} points at {sha}, not HEAD {head}")
        }
        Some(_) => Ok(()),
    }
}

/// `owner/name` from a GitHub remote URL (https or ssh form).
pub fn github_slug(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.split_once("github.com/").map(|(_, r)| r))?;
    let slug = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = slug.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

pub fn remote_url(dir: &Path, remote: &str) -> Option<String> {
    git(dir, &["remote", "get-url", remote])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(tags: &[&str]) -> GitState {
        GitState {
            commit: Some("a".repeat(40)),
            short: Some("abc1234".into()),
            commit_time: Some(1_700_000_000),
            head_tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn snapshot_versions() {
        let v = resolve_version(&state(&[]), "0.3.1", true).unwrap();
        assert_eq!(v.version, "0.3.1-SNAPSHOT-abc1234");
        let none = GitState {
            commit: None,
            short: None,
            commit_time: None,
            head_tags: vec![],
        };
        let v = resolve_version(&none, "0.3.1", true).unwrap();
        assert_eq!(v.version, "0.3.1-SNAPSHOT-nogit");
        assert_eq!(source_date_epoch(None, &none).unwrap(), 0);
    }

    #[test]
    fn tag_must_match_package() {
        let v = resolve_version(&state(&["latest", "v0.3.1"]), "0.3.1", false).unwrap();
        assert_eq!(v.tag.as_deref(), Some("v0.3.1"));
        let err = resolve_version(&state(&["v0.4.0"]), "0.3.1", false).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        let err = resolve_version(&state(&[]), "0.3.1", false).unwrap_err();
        assert!(err.to_string().contains("--snapshot"), "{err}");
    }

    #[test]
    fn epoch_sources() {
        assert_eq!(source_date_epoch(None, &state(&[])).unwrap(), 1_700_000_000);
        assert_eq!(source_date_epoch(Some("12"), &state(&[])).unwrap(), 12);
        assert!(source_date_epoch(Some("yesterday"), &state(&[])).is_err());
    }

    #[test]
    fn tag_and_slug_parsing() {
        assert_eq!(tag_version("v1.2.3"), Some("1.2.3"));
        assert_eq!(tag_version("v1.2.3-rc.1"), Some("1.2.3-rc.1"));
        assert_eq!(tag_version("1.2.3"), None);
        assert_eq!(tag_version("v1.2"), None);
        assert_eq!(
            github_slug("git@github.com:o/r.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            github_slug("https://github.com/o/r").as_deref(),
            Some("o/r")
        );
        assert_eq!(github_slug("/tmp/remote.git"), None);
    }
}
