//! HTTP side of the registry client: one `ureq` agent per registry host, the
//! `WWW-Authenticate` challenge (Bearer token or Basic) answered once per
//! scope set and replayed, tokens cached in memory and never printed, and
//! registry errors turned into messages carrying the registry's own text.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ureq::http::{self, Method, StatusCode};

use super::reference::api_host;
use super::registry_auth::{self, Challenge, Credentials};

/// Request body that can be rebuilt when a request is replayed after a 401.
pub enum Body<'a> {
    Empty,
    Bytes(&'a [u8]),
    File(&'a Path),
}

pub struct Transport {
    agent: ureq::Agent,
    /// Registry as written in the image name (`gcr.io`, `localhost:5055`).
    pub registry: String,
    /// `https://<api host>` (`http://` when insecure).
    pub base_url: String,
    credentials: Option<Credentials>,
    /// Authorization header value per scope set (`repository:<repo>:<actions> …`).
    auth: RefCell<HashMap<String, String>>,
}

impl Transport {
    /// `insecure` selects plain http (implied for loopback registries).
    pub fn new(registry: &str, insecure: bool) -> Result<Transport> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_per_read(Some(Duration::from_secs(120)))
            .user_agent(concat!("naspeed-ns-image/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let scheme = if insecure { "http" } else { "https" };
        Ok(Transport {
            agent,
            registry: registry.to_string(),
            base_url: format!("{scheme}://{}", api_host(registry)),
            credentials: registry_auth::lookup(registry)?,
            auth: RefCell::new(HashMap::new()),
        })
    }

    pub fn location(&self, resp: &http::Response<ureq::Body>) -> Result<String> {
        let location = header(resp, "location").context("registry sent no upload Location")?;
        Ok(
            if location.starts_with("http://") || location.starts_with("https://") {
                location
            } else if location.starts_with('/') {
                format!("{}{location}", self.base_url)
            } else {
                format!("{}/{location}", self.base_url)
            },
        )
    }

    /// Send once; on 401 answer the challenge and replay the request once.
    pub fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Body,
        scopes: &[(&str, &str)],
    ) -> Result<http::Response<ureq::Body>> {
        let scope_key: Vec<String> = scopes
            .iter()
            .map(|(r, a)| format!("repository:{r}:{a}"))
            .collect();
        let key = scope_key.join(" ");
        let cached = self.auth.borrow().get(&key).cloned();
        let resp = self.send_raw(&method, url, headers, &body, cached.as_deref())?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let challenge = header(&resp, "www-authenticate")
            .and_then(|h| Challenge::parse(&h))
            .with_context(|| {
                format!(
                    "{method} {} answered 401 without a usable challenge",
                    redact(url)
                )
            })?;
        let authorization = match challenge {
            Challenge::Basic => self
                .credentials
                .as_ref()
                .map(Credentials::basic_header)
                .with_context(|| {
                    format!(
                        "{} needs credentials (docker config.json `auths`)",
                        self.registry
                    )
                })?,
            Challenge::Bearer {
                realm,
                service,
                scope,
            } => {
                let mut wanted = scope_key.clone();
                wanted.extend(scope.filter(|s| !wanted.contains(s)));
                format!(
                    "Bearer {}",
                    self.fetch_token(&realm, service.as_deref(), &wanted)?
                )
            }
        };
        self.auth.borrow_mut().insert(key, authorization.clone());
        self.send_raw(&method, url, headers, &body, Some(&authorization))
    }

    fn send_raw(
        &self,
        method: &Method,
        url: &str,
        headers: &[(&str, &str)],
        body: &Body,
        authorization: Option<&str>,
    ) -> Result<http::Response<ureq::Body>> {
        let mut builder = http::Request::builder().method(method.clone()).uri(url);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        if let Some(auth) = authorization {
            builder = builder.header("Authorization", auth);
        }
        let what = || format!("{method} {}", redact(url));
        let result = match body {
            Body::Empty if *method == Method::POST => self.agent.run(builder.body(&[][..])?),
            Body::Empty => self.agent.run(builder.body(())?),
            Body::Bytes(bytes) => self.agent.run(builder.body(*bytes)?),
            Body::File(path) => {
                let file = std::fs::File::open(path)
                    .with_context(|| format!("open {}", path.display()))?;
                self.agent.run(builder.body(file)?)
            }
        };
        result.with_context(what)
    }

    fn fetch_token(&self, realm: &str, service: Option<&str>, scopes: &[String]) -> Result<String> {
        let mut url = realm.to_string();
        let mut sep = if url.contains('?') { '&' } else { '?' };
        for (k, v) in service
            .map(|s| ("service", s))
            .into_iter()
            .chain(scopes.iter().map(|s| ("scope", s.as_str())))
        {
            url.push_str(&format!("{sep}{k}={}", query_escape(v)));
            sep = '&';
        }
        let mut req = self.agent.get(&url);
        if let Some(creds) = &self.credentials {
            req = req.header("Authorization", creds.basic_header());
        }
        let mut resp = req
            .call()
            .with_context(|| format!("token request to {}", redact(realm)))?;
        expect(
            &mut resp,
            &[StatusCode::OK],
            &format!("token for {} from {}", self.registry, redact(realm)),
        )?;
        let json: serde_json::Value =
            serde_json::from_slice(&resp.body_mut().with_config().limit(1 << 20).read_to_vec()?)
                .context("parse token response")?;
        json.get("token")
            .or_else(|| json.get("access_token"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .context("token response carries no token")
    }
}

pub fn header(resp: &http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers().get(name)?.to_str().ok().map(str::to_string)
}

/// Fail with the registry's own error text when the status is unexpected.
pub fn expect(resp: &mut http::Response<ureq::Body>, ok: &[StatusCode], what: &str) -> Result<()> {
    if ok.contains(&resp.status()) {
        return Ok(());
    }
    let body = resp
        .body_mut()
        .with_config()
        .limit(4096)
        .lossy_utf8(true)
        .read_to_string()
        .unwrap_or_default();
    let hint = match resp.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            " (check registry credentials and push rights)"
        }
        _ => "",
    };
    bail!("{what}: HTTP {}{hint}: {}", resp.status(), body.trim())
}

/// Upload URLs may carry state tokens in the query; keep them out of errors.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~:/,".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
