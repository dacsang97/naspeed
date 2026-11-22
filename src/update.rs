//! Update notices and support for `naspeed update`: work out how naspeed
//! was installed (Homebrew tap, `cargo install`, or a manual release
//! download) and answer per channel — brew lags the GitHub tag until the
//! tap job updates the formula, crates.io lags until `publish` runs.
//!
//! The cached lookup lives in `<config>/update-check.json` so the check
//! costs nothing once fresh; `ns dev` refreshes it in the background and
//! prints a one-line notice through the log bus. `NASPEED_UPDATE_CHECK=0`
//! opts out of the notice and the refresh.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

/// The running build. Dev builds of the next release still carry the last
/// released version while the Release PR is open, so they never nag.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub const RELEASES_URL: &str = "https://github.com/dacsang97/naspeed/releases/latest";
const GITHUB_LATEST_API: &str = "https://api.github.com/repos/dacsang97/naspeed/releases/latest";
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates/naspeed";
const TAP_FORMULA_RAW: &str =
    "https://raw.githubusercontent.com/dacsang97/homebrew-tap/main/naspeed.rb";

/// Hours a cached "update available" answer is trusted before re-checking.
const UPDATE_CACHE_TTL_SECS: u64 = 20 * 3600;
/// A "no update" answer expires quickly so closely-spaced releases are seen.
const NO_UPDATE_CACHE_TTL_SECS: u64 = 15 * 60;
/// Update checks must never hang a development loop.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How the user installs naspeed; decides both the version to compare
/// against and the upgrade instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// `brew install dacsang97/tap/naspeed` (probe: the keg dir exists).
    Brew,
    /// `cargo install naspeed --locked` (probe: cargo's install registry).
    Cargo,
    /// Downloaded archive, built from source, or otherwise unmanaged.
    Other,
}

impl InstallMethod {
    /// The one-liner that brings this install to the latest release.
    pub fn instructions(self) -> &'static str {
        match self {
            // `setup` refreshes the copy at ~/.cargo/bin/naspeed that cargo
            // actually loads through the rustc-wrapper config.
            InstallMethod::Brew => "brew upgrade dacsang97/tap/naspeed && naspeed setup",
            InstallMethod::Cargo => "cargo install naspeed --locked && naspeed setup",
            InstallMethod::Other => {
                "download the latest release from \
                https://github.com/dacsang97/naspeed/releases/latest and run `naspeed setup`"
            }
        }
    }
}

/// The standard brew prefixes (macOS arm64/Intel, Linuxbrew).
const BREW_KEGS: &[&str] = &[
    "/opt/homebrew/Cellar/naspeed",
    "/usr/local/Cellar/naspeed",
    "/home/linuxbrew/.linuxbrew/Cellar/naspeed",
];

pub fn detect_install_method() -> InstallMethod {
    detect_with(BREW_KEGS, &config::cargo_home())
}

fn detect_with(kegs: &[&str], cargo_home: &Path) -> InstallMethod {
    if kegs.iter().any(|k| Path::new(k).is_dir()) {
        return InstallMethod::Brew;
    }
    // `cargo install` records every installed package here. The v1 file is
    // TOML lines like `"naspeed 0.12.0 (registry+...)" = {...}`, v2 is JSON
    // with the same strings as keys; a quoted `"naspeed ` prefix matches the
    // exact package name either way.
    for registry in [".crates.toml", ".crates2.json"] {
        if let Ok(text) = std::fs::read_to_string(cargo_home.join(registry))
            && text.contains("\"naspeed ")
        {
            return InstallMethod::Cargo;
        }
    }
    InstallMethod::Other
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct UpdateCache {
    latest: Option<String>,
    checked_at: u64,
}

fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("update-check.json")
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn cache_is_fresh(cache: &UpdateCache, now: u64) -> bool {
    let ttl = match cache.latest.as_deref() {
        Some(latest) if newer_than(latest, CURRENT) => UPDATE_CACHE_TTL_SECS,
        _ => NO_UPDATE_CACHE_TTL_SECS,
    };
    cache.checked_at.saturating_add(ttl) > now
}

/// `latest` ordered after `current`: numeric `major.minor.patch` compare,
/// ignoring pre-release/build suffixes beyond the patch number.
pub fn newer_than(latest: &str, current: &str) -> bool {
    fn triple(v: &str) -> Option<[u64; 3]> {
        let v = v.trim().trim_start_matches('v');
        let mut out = [0u64; 3];
        let core = v.split(['-', '+']).next()?;
        for (i, part) in core.split('.').enumerate() {
            if i >= 3 {
                return None;
            }
            out[i] = part.parse().ok()?;
        }
        Some(out)
    }
    match (triple(latest), triple(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn fetch(url: &str) -> Result<String> {
    let body = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent(concat!("naspeed/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .read_to_string()?;
    Ok(body)
}

/// The newest version installable through this channel right now.
pub fn latest_for(method: InstallMethod) -> Result<String> {
    match method {
        InstallMethod::Brew => {
            let formula = fetch(TAP_FORMULA_RAW)?;
            formula
                .lines()
                .find_map(|l| {
                    let l = l.trim();
                    Some(
                        l.strip_prefix("version ")?
                            .trim_matches('"')
                            .trim()
                            .to_string(),
                    )
                })
                .context("no `version` field in the tap formula")
        }
        InstallMethod::Cargo => {
            #[derive(Deserialize)]
            struct Resp {
                #[serde(rename = "crate")]
                krate: Krate,
            }
            #[derive(Deserialize)]
            struct Krate {
                max_stable_version: Option<String>,
                max_version: String,
            }
            let resp: Resp = serde_json::from_str(&fetch(CRATES_IO_API)?)?;
            Ok(resp
                .krate
                .max_stable_version
                .unwrap_or(resp.krate.max_version))
        }
        InstallMethod::Other => {
            #[derive(Deserialize)]
            struct Release {
                tag_name: String,
            }
            let release: Release = serde_json::from_str(&fetch(GITHUB_LATEST_API)?)?;
            Ok(release.tag_name.trim_start_matches('v').to_string())
        }
    }
}

/// The newest version worth telling the user about: cached if fresh,
/// fetched (and cached) otherwise. Network or parse failures are `Ok(None)`
/// — an update notice must never break a build tool.
pub fn latest_cached_or_fetched(config_dir: &Path, method: InstallMethod) -> Option<String> {
    let path = cache_path(config_dir);
    let cache = read_cache(&path);
    let now = crate::store::now_secs();
    if let Some(c) = &cache
        && cache_is_fresh(c, now)
    {
        return c.latest.clone();
    }
    let latest = latest_for(method)
        .inspect_err(|e| tracing::debug!("update check failed: {e:#}"))
        .ok();
    match &latest {
        Some(latest) => {
            let out = UpdateCache {
                latest: Some(latest.clone()),
                checked_at: now,
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, serde_json::to_vec(&out).unwrap_or_default());
        }
        // Keep the previous answer readable but force a retry next run.
        None => {
            if let Some(c) = &cache {
                let out = UpdateCache {
                    checked_at: 0,
                    ..c.clone()
                };
                let _ = std::fs::write(&path, serde_json::to_vec(&out).unwrap_or_default());
            }
        }
    }
    latest.or(cache.and_then(|c| c.latest))
}

/// One-line notice text when an update exists for this install.
pub fn notice(method: InstallMethod, latest: &str) -> Option<String> {
    if !newer_than(latest, CURRENT) {
        return None;
    }
    Some(format!(
        "update available: naspeed {CURRENT} → {latest} — update with: {}",
        method.instructions()
    ))
}

/// `ns dev`: check for an update and post the notice to the dev log bus.
/// Runs on a background thread: the answer comes from the fresh cache right
/// away, or from the network a moment later — both appear in the current
/// session since `ns dev` is long-running. Never blocks startup, never
/// breaks the loop on failure. `NASPEED_UPDATE_CHECK=0` disables it.
pub fn notify_async(bus: &crate::dev::log_bus::LogBus) {
    if std::env::var("NASPEED_UPDATE_CHECK").is_ok_and(|v| v == "0" || v == "false") {
        return;
    }
    let bus = bus.clone();
    let _ = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let method = detect_install_method();
            let dir = config::config_dir();
            if let Some(latest) = latest_cached_or_fetched(&dir, method)
                && let Some(text) = notice(method, &latest)
            {
                crate::dev::status_line::info(&bus, text);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_is_numeric_and_prefix_tolerant() {
        assert!(newer_than("0.13.0", "0.12.2"));
        assert!(newer_than("v0.12.3", "0.12.2"));
        assert!(!newer_than("0.12.2", "0.12.2"));
        assert!(!newer_than("0.12.2", "0.12.10"));
        assert!(!newer_than("garbage", "0.12.2"));
        assert!(!newer_than("0.12", "0.12.0"));
    }

    #[test]
    fn no_update_cache_expires_before_an_available_update_cache() {
        let checked_at = 1_000;
        let current = UpdateCache {
            latest: Some(CURRENT.to_string()),
            checked_at,
        };
        assert!(cache_is_fresh(
            &current,
            checked_at + NO_UPDATE_CACHE_TTL_SECS - 1
        ));
        assert!(!cache_is_fresh(
            &current,
            checked_at + NO_UPDATE_CACHE_TTL_SECS
        ));

        let available = UpdateCache {
            latest: Some("99.0.0".to_string()),
            checked_at,
        };
        assert!(cache_is_fresh(
            &available,
            checked_at + NO_UPDATE_CACHE_TTL_SECS
        ));
        assert!(!cache_is_fresh(
            &available,
            checked_at + UPDATE_CACHE_TTL_SECS
        ));
    }

    #[test]
    fn brew_wins_over_cargo_when_both_markers_exist() {
        let dir = tempfile::tempdir().unwrap();
        let keg = dir.path().join("Cellar/naspeed");
        std::fs::create_dir_all(&keg).unwrap();
        let cargo = dir.path().join("cargo");
        std::fs::create_dir_all(&cargo).unwrap();
        std::fs::write(
            cargo.join(".crates.toml"),
            "[[v1]]\n\"naspeed 0.12.0 (registry+x)\" = {}\n",
        )
        .unwrap();
        let keg = dir.path().join("Cellar/naspeed").display().to_string();
        let kegs = [keg.as_str()];
        assert_eq!(detect_with(&kegs, &cargo), InstallMethod::Brew);
        let no_keg_s = dir.path().join("nope").display().to_string();
        let no_keg = [no_keg_s.as_str()];
        assert_eq!(detect_with(&no_keg, &cargo), InstallMethod::Cargo);
        std::fs::remove_file(cargo.join(".crates.toml")).unwrap();
        assert_eq!(detect_with(&no_keg, &cargo), InstallMethod::Other);
    }

    #[test]
    fn notice_mentions_the_channels_own_command() {
        // A clearly future version: this test must survive every version bump.
        let future = "99.0.0";
        let n = notice(InstallMethod::Brew, future).unwrap();
        assert!(n.contains("brew upgrade dacsang97/tap/naspeed"));
        assert!(n.contains(CURRENT) && n.contains(future));
        assert!(notice(InstallMethod::Cargo, CURRENT).is_none());
    }
}
