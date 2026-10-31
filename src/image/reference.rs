//! Image references (`host[:port]/repo[:tag|@digest]`) and platforms
//! (`linux/amd64`), parsed the way Docker does: a first path component with a
//! `.` or `:` (or `localhost`) names the registry, anything else is Docker Hub.

use std::fmt;

use anyhow::{Result, bail};

pub const DOCKER_HUB: &str = "docker.io";
/// Docker Hub's API host; `docker.io` itself only redirects.
const DOCKER_HUB_API: &str = "registry-1.docker.io";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Registry as written by the user (`gcr.io`, `localhost:5055`, `docker.io`).
    pub registry: String,
    /// Repository path inside the registry (`distroless/static-debian12`).
    pub repository: String,
    /// Tag or `sha256:` digest; `latest` when the user gave neither.
    pub reference: String,
}

impl Reference {
    pub fn parse(text: &str) -> Result<Reference> {
        let text = text.trim();
        if text.is_empty() || text.contains("://") || text.chars().any(char::is_whitespace) {
            bail!("invalid image reference {text:?}");
        }
        let (name, digest) = match text.split_once('@') {
            Some((name, digest)) => (name, Some(digest)),
            None => (text, None),
        };
        // A tag is a `:` after the last `/` (a `:` before it is a port).
        let last_slash = name.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match name[last_slash..].rfind(':') {
            Some(i) => (&name[..last_slash + i], Some(&name[last_slash + i + 1..])),
            None => (name, None),
        };
        let (registry, mut repository) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => (DOCKER_HUB.to_string(), name.to_string()),
        };
        if registry == DOCKER_HUB && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        let valid_repo = !repository.is_empty()
            && repository.split('/').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
            });
        if !valid_repo {
            bail!("invalid repository {repository:?} in image reference {text:?}");
        }
        let reference = match (digest, tag) {
            (Some(digest), _) => {
                if !is_digest(digest) {
                    bail!("invalid digest {digest:?} in image reference {text:?}");
                }
                digest.to_string()
            }
            (None, Some(tag)) => {
                if !is_valid_tag(tag) {
                    bail!("invalid tag {tag:?} in image reference {text:?}");
                }
                tag.to_string()
            }
            (None, None) => "latest".to_string(),
        };
        Ok(Reference {
            registry,
            repository,
            reference,
        })
    }

    /// Host the registry API lives on.
    pub fn api_host(&self) -> &str {
        api_host(&self.registry)
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if is_digest(&self.reference) { '@' } else { ':' };
        write!(
            f,
            "{}/{}{sep}{}",
            self.registry, self.repository, self.reference
        )
    }
}

pub fn api_host(registry: &str) -> &str {
    if registry == DOCKER_HUB || registry == "index.docker.io" {
        DOCKER_HUB_API
    } else {
        registry
    }
}

/// `localhost`, `127.0.0.1` and `[::1]` registries are plain http, as Docker does.
pub fn is_loopback(registry: &str) -> bool {
    let host = match registry.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => registry,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

pub fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Docker tag grammar: `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
pub fn is_valid_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '_')
        && tag.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Turn a version like `1.2.0+build.5` into a usable tag (`1.2.0-build.5`).
pub fn tag_from_version(version: &str) -> String {
    let mut tag: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(128)
        .collect();
    if tag.starts_with(['.', '-']) {
        tag.insert(0, 'v');
        tag.truncate(128);
    }
    tag
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

impl Platform {
    pub fn parse(text: &str) -> Result<Platform> {
        let mut parts = text.split('/');
        let (Some(os), Some(arch)) = (parts.next(), parts.next()) else {
            bail!("invalid platform {text:?}; expected os/arch like linux/amd64");
        };
        // A variant (`linux/arm64/v8`) adds nothing for the two arches we ship.
        if os != "linux" || !matches!(arch, "amd64" | "arm64") || parts.count() > 1 {
            bail!("unsupported platform {text:?}; ns image builds linux/amd64 and linux/arm64");
        }
        Ok(Platform {
            os: os.to_string(),
            architecture: arch.to_string(),
        })
    }

    /// `linux/amd64` for `x86_64-unknown-linux-*`, `linux/arm64` for `aarch64-unknown-linux-*`.
    pub fn for_target(target: &str) -> Option<Platform> {
        let arch = match target.split('-').next()? {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => return None,
        };
        target.contains("-linux-").then(|| Platform {
            os: "linux".into(),
            architecture: arch.into(),
        })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_repo_tag_and_digest() {
        let r = Reference::parse("gcr.io/distroless/static-debian12").unwrap();
        assert_eq!(
            (
                r.registry.as_str(),
                r.repository.as_str(),
                r.reference.as_str()
            ),
            ("gcr.io", "distroless/static-debian12", "latest")
        );
        let r = Reference::parse("localhost:5055/ns-fixture/hello-server:0.3.1").unwrap();
        assert_eq!(r.registry, "localhost:5055");
        assert_eq!(r.reference, "0.3.1");
        assert_eq!(
            r.to_string(),
            "localhost:5055/ns-fixture/hello-server:0.3.1"
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        let r = Reference::parse(&format!("ghcr.io/o/api@{digest}")).unwrap();
        assert_eq!(r.reference, digest);
        let r = Reference::parse("alpine:3").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("docker.io", "library/alpine")
        );
        assert_eq!(r.api_host(), "registry-1.docker.io");
    }

    #[test]
    fn rejects_bad_references() {
        for bad in [
            "",
            "https://gcr.io/x",
            "gcr.io/Upper/x",
            "gcr.io/x:",
            "gcr.io/x@sha256:12",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loopback_and_tags() {
        assert!(is_loopback("localhost:5055"));
        assert!(is_loopback("127.0.0.1"));
        assert!(!is_loopback("ghcr.io"));
        assert_eq!(tag_from_version("1.2.0+build.5"), "1.2.0-build.5");
        assert!(is_valid_tag("0.3.1-SNAPSHOT-abc123"));
        assert!(!is_valid_tag("-x"));
    }

    #[test]
    fn platforms_from_targets() {
        assert_eq!(
            Platform::for_target("x86_64-unknown-linux-musl")
                .unwrap()
                .to_string(),
            "linux/amd64"
        );
        assert_eq!(
            Platform::for_target("aarch64-unknown-linux-gnu")
                .unwrap()
                .to_string(),
            "linux/arm64"
        );
        assert!(Platform::for_target("aarch64-apple-darwin").is_none());
        assert!(Platform::parse("linux/riscv64").is_err());
    }
}
