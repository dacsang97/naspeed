//! GitHub Release through the REST API: create the release, then upload
//! every asset to its `upload_url`.
//!
//! The API base defaults to `https://api.github.com` and can be overridden by
//! `NS_GITHUB_API_URL` (tests point it at a local server). The token is only
//! ever placed in the `Authorization` header: it never appears in errors,
//! logs or output.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const API_ENV: &str = "NS_GITHUB_API_URL";
pub const DEFAULT_API: &str = "https://api.github.com";

pub struct NewRelease<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    pub tag: &'a str,
    pub body: &'a str,
    pub draft: bool,
    pub prerelease: bool,
}

pub struct GitHub {
    agent: ureq::Agent,
    api: String,
    token: String,
}

/// Status and body of a response, whatever its status code.
type Reply = (u16, String);

impl GitHub {
    pub fn new(api: &str, token: String) -> GitHub {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build();
        GitHub {
            agent: config.into(),
            api: api.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn post(&self, url: &str, content_type: &str, body: &[u8]) -> Result<Reply> {
        let mut resp = self
            .agent
            .post(url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(
                "User-Agent",
                concat!("ns-release/", env!("CARGO_PKG_VERSION")),
            )
            .header("Content-Type", content_type)
            .send(body)
            .with_context(|| format!("POST {}", strip_query(url)))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    }

    /// Create the release; returns `(html_url, upload_url template)`.
    pub fn create_release(&self, release: &NewRelease) -> Result<(String, String)> {
        let url = format!("{}/repos/{}/releases", self.api, release.repo);
        let payload = json!({
            "tag_name": release.tag,
            "name": release.tag,
            "body": release.body,
            "draft": release.draft,
            "prerelease": release.prerelease,
        });
        let (status, text) = self.post(&url, "application/json", payload.to_string().as_bytes())?;
        if status != 201 {
            bail!(
                "creating release {} failed: HTTP {status}: {}",
                release.tag,
                api_message(&text)
            );
        }
        let v: Value = serde_json::from_str(&text).context("parse create-release response")?;
        let html = v["html_url"].as_str().unwrap_or_default().to_string();
        let upload = v["upload_url"]
            .as_str()
            .context("create-release response has no upload_url")?
            .to_string();
        Ok((html, upload))
    }

    /// Upload one file as a release asset.
    pub fn upload_asset(&self, upload_url: &str, file: &Path) -> Result<()> {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .context("asset file name")?;
        let base = upload_url.split('{').next().unwrap_or(upload_url);
        let url = format!("{base}?name={}", percent_encode(name));
        let data = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
        let (status, text) = self.post(&url, "application/octet-stream", &data)?;
        if status != 201 {
            bail!(
                "uploading {name} failed: HTTP {status}: {}",
                api_message(&text)
            );
        }
        Ok(())
    }
}

/// The `message` field of a GitHub error body, else a trimmed prefix of it.
fn api_message(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.chars().take(200).collect())
}

fn strip_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_and_messages() {
        assert_eq!(percent_encode("a-1.0+x y.tar.gz"), "a-1.0%2Bx%20y.tar.gz");
        assert_eq!(
            api_message(r#"{"message":"Bad credentials"}"#),
            "Bad credentials"
        );
        assert_eq!(api_message("oops"), "oops");
        assert_eq!(strip_query("http://h/x?name=a"), "http://h/x");
    }
}
