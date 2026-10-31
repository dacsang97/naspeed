//! Registry credentials and the `WWW-Authenticate` challenge.
//!
//! Credentials come from `GITHUB_TOKEN` (ghcr.io only) or the `auths` map of
//! `$DOCKER_CONFIG/config.json` (default `~/.docker/config.json`). Credential
//! helpers (`credsStore`, `credHelpers`) are not executed; when one is the only
//! source for a host we say so once and continue anonymously. Secrets never
//! reach logs: [`Credentials`] has a redacting `Debug`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone)]
pub struct Credentials {
    /// `base64(user:password)`, ready for a `Basic` header.
    basic: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

impl Credentials {
    pub fn from_user_password(user: &str, password: &str) -> Credentials {
        Credentials {
            basic: base64_encode(format!("{user}:{password}").as_bytes()),
        }
    }

    pub fn basic_header(&self) -> String {
        format!("Basic {}", self.basic)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, DockerAuth>,
    creds_store: Option<String>,
    #[serde(default)]
    cred_helpers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct DockerAuth {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

fn docker_config_path() -> PathBuf {
    match std::env::var_os("DOCKER_CONFIG") {
        Some(dir) => PathBuf::from(dir).join("config.json"),
        None => crate::config::home_dir().join(".docker/config.json"),
    }
}

/// Credentials for `registry` (as written in the image name), if any.
pub fn lookup(registry: &str) -> Result<Option<Credentials>> {
    if registry == "ghcr.io"
        && let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        let user = std::env::var("GITHUB_ACTOR").unwrap_or_else(|_| "x-access-token".into());
        return Ok(Some(Credentials::from_user_password(&user, &token)));
    }
    let path = docker_config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let config: DockerConfig =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    from_docker_config(&config, registry)
        .with_context(|| format!("credentials for {registry} in {}", path.display()))
}

fn from_docker_config(config: &DockerConfig, registry: &str) -> Result<Option<Credentials>> {
    let keys: Vec<String> = if registry == "docker.io" {
        vec![
            "https://index.docker.io/v1/".into(),
            "index.docker.io".into(),
            "docker.io".into(),
        ]
    } else {
        vec![
            registry.to_string(),
            format!("https://{registry}"),
            format!("http://{registry}"),
            format!("https://{registry}/v1/"),
        ]
    };
    for key in &keys {
        let Some(entry) = config.auths.get(key) else {
            continue;
        };
        if let Some(auth) = entry.auth.as_deref().filter(|a| !a.is_empty()) {
            let decoded = base64_decode(auth).context("`auth` is not valid base64")?;
            if !decoded.contains(&b':') {
                bail!("`auth` must encode user:password");
            }
            return Ok(Some(Credentials {
                basic: auth.to_string(),
            }));
        }
        if let (Some(user), Some(password)) = (&entry.username, &entry.password) {
            return Ok(Some(Credentials::from_user_password(user, password)));
        }
    }
    let helper = keys
        .iter()
        .find_map(|key| config.cred_helpers.get(key))
        .or(config.creds_store.as_ref());
    if let Some(helper) = helper {
        crate::ui_warn!(
            "not running the docker credential helper; continuing anonymously (put an `auth` entry in docker config.json, or set GITHUB_TOKEN for ghcr.io)";
            helper = helper, registry = registry);
    }
    Ok(None)
}

/// A parsed `WWW-Authenticate` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Basic,
    Bearer {
        realm: String,
        service: Option<String>,
        scope: Option<String>,
    },
}

impl Challenge {
    pub fn parse(header: &str) -> Option<Challenge> {
        let (scheme, rest) = header.trim().split_once(' ').unwrap_or((header.trim(), ""));
        if scheme.eq_ignore_ascii_case("basic") {
            return Some(Challenge::Basic);
        }
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let params = parse_params(rest);
        Some(Challenge::Bearer {
            realm: params.get("realm")?.clone(),
            service: params.get("service").cloned(),
            scope: params.get("scope").cloned(),
        })
    }
}

/// `key="value", key2=value2`; quoted values may contain commas (scopes do).
fn parse_params(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = text.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        rest = rest[eq + 1..].trim_start();
        let value;
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            value = quoted[..end].to_string();
            rest = quoted.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        out.insert(key, value);
        rest = rest.trim_start().trim_start_matches(',');
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.trim().trim_end_matches('=').bytes() {
        let v = B64
            .iter()
            .position(|&b| b == c)
            .context("invalid base64 character")? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "user:pa$$word"] {
            assert_eq!(
                base64_decode(&base64_encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn parses_bearer_challenge_with_comma_in_scope() {
        let c = Challenge::parse(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/api:pull,push""#,
        )
        .unwrap();
        assert_eq!(
            c,
            Challenge::Bearer {
                realm: "https://ghcr.io/token".into(),
                service: Some("ghcr.io".into()),
                scope: Some("repository:o/api:pull,push".into()),
            }
        );
        assert_eq!(
            Challenge::parse(r#"Basic realm="Registry""#),
            Some(Challenge::Basic)
        );
    }

    #[test]
    fn docker_config_auths_and_helpers() {
        let config: DockerConfig = serde_json::from_str(
            r#"{"auths":{"https://registry.example.com":{"auth":"dXNlcjpwYXNz"},"other.io":{}},
                "credsStore":"osxkeychain"}"#,
        )
        .unwrap();
        let creds = from_docker_config(&config, "registry.example.com")
            .unwrap()
            .unwrap();
        assert_eq!(creds.basic_header(), "Basic dXNlcjpwYXNz");
        assert_eq!(format!("{creds:?}"), "Credentials(<redacted>)");
        assert!(from_docker_config(&config, "other.io").unwrap().is_none());
    }
}
