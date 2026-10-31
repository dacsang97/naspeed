//! Wrapper side of the peer cache: on a local miss, ask each peer for the
//! tier-1 manifest, compute the full key from it, fetch the entry and any
//! missing objects (hash-verified), and write them into the local store so
//! the normal restore path takes over. Also uploads entries (`cache push`).
//!
//! A peer that cannot be reached is skipped by every wrapper for
//! `DOWN_FOR` (a marker file in the store's `tmp/`), so a laptop off the
//! tailnet pays one connect timeout, not one per crate.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use ureq::http::{self, Method, StatusCode};

use super::{validate_entry, validate_manifest};
use crate::config::Settings;
use crate::store::usage_log::Record;
use crate::store::{Entry, Manifest, Store};

pub const DOWN_FOR: Duration = Duration::from_secs(60);
const JSON_LIMIT: u64 = 64 << 20;

pub struct PeerClient<'a> {
    store: &'a Store,
    peers: Vec<String>,
    token: Option<String>,
    agent: ureq::Agent,
}

/// The peer did not answer; anything else (404, bad data) is not an outage.
#[derive(Debug)]
struct Unreachable(String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer unreachable: {}", self.0)
    }
}

impl std::error::Error for Unreachable {}

impl<'a> PeerClient<'a> {
    /// `None` when no peer is configured (the common, zero-cost case).
    pub fn new(store: &'a Store, settings: &Settings) -> Option<PeerClient<'a>> {
        PeerClient::with_peers(store, settings.remotes.clone(), settings.remote_token())
    }

    pub fn with_peers(
        store: &'a Store,
        peers: Vec<String>,
        token: Option<String>,
    ) -> Option<PeerClient<'a>> {
        if peers.is_empty() {
            return None;
        }
        let connect_ms = std::env::var("NASPEED_REMOTE_CONNECT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(800);
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_resolve(Some(Duration::from_secs(2)))
            .timeout_connect(Some(Duration::from_millis(connect_ms)))
            // Generous enough for a busy peer that fsyncs a pushed object;
            // the first timeout marks the peer down for `DOWN_FOR`, so only
            // one wrapper per minute ever waits this long.
            .timeout_recv_response(Some(Duration::from_secs(20)))
            .timeout_per_read(Some(Duration::from_secs(20)))
            .user_agent(concat!("naspeed/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Some(PeerClient {
            store,
            peers: peers
                .into_iter()
                .map(|p| p.trim_end_matches('/').to_owned())
                .collect(),
            token,
            agent,
        })
    }

    fn down_marker(&self, peer: &str) -> PathBuf {
        let id = blake3::hash(peer.as_bytes()).to_hex();
        self.store
            .tmp_dir()
            .join(format!("peer-down.{}", &id.as_str()[..16]))
    }

    fn is_down(&self, peer: &str) -> bool {
        std::fs::metadata(self.down_marker(peer))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age < DOWN_FOR)
    }

    fn mark_down(&self, peer: &str) {
        let _ = std::fs::write(self.down_marker(peer), peer);
    }

    fn request<B: ureq::AsSendBody>(
        &self,
        method: Method,
        url: &str,
        body: B,
    ) -> Result<http::Response<ureq::Body>> {
        let mut builder = http::Request::builder().method(method).uri(url);
        if let Some(token) = &self.token {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        self.agent
            .run(builder.body(body)?)
            .map_err(|e| Unreachable(format!("{url}: {e}")).into())
    }

    /// GET a small JSON document; `None` on 404.
    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        let mut resp = self.request(Method::GET, url, ())?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Ok(None),
            s => bail!("GET {url}: {s}"),
        }
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(JSON_LIMIT)
            .read_to_vec()
            .map_err(|e| Unreachable(format!("{url}: {e}")))?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Ask the peers in order. On success the entry (and everything it
    /// references) is in the local store; returns its key.
    pub fn fetch(&self, tier1: &str, key_for: &dyn Fn(&Manifest) -> String) -> Option<String> {
        for peer in &self.peers {
            if self.is_down(peer) {
                continue;
            }
            let started = Instant::now();
            match self.fetch_from(peer, tier1, key_for) {
                Ok(Some((key, bytes))) => {
                    self.store.append_usage(&Record::RemoteHit {
                        key: key.clone(),
                        bytes,
                        fetch_ms: started.elapsed().as_millis() as u64,
                    });
                    tracing::info!(%peer, %key, bytes, "fetched from peer");
                    return Some(key);
                }
                Ok(None) => {}
                Err(e) => {
                    if e.downcast_ref::<Unreachable>().is_some() {
                        self.mark_down(peer);
                    }
                    tracing::warn!(%peer, "peer fetch failed: {e:#}");
                }
            }
        }
        None
    }

    fn fetch_from(
        &self,
        peer: &str,
        tier1: &str,
        key_for: &dyn Fn(&Manifest) -> String,
    ) -> Result<Option<(String, u64)>> {
        let Some(manifest) = self.get_json::<Manifest>(&format!("{peer}/v1/manifests/{tier1}"))?
        else {
            return Ok(None);
        };
        validate_manifest(&manifest)?;
        let key = key_for(&manifest);
        let Some(entry) = self.get_json::<Entry>(&format!("{peer}/v1/entries/{key}"))? else {
            return Ok(None);
        };
        validate_entry(&entry, &key, Some(tier1))?;
        let mut bytes = 0;
        for o in &entry.outputs {
            let local = self.store.object_path(&o.object);
            if std::fs::metadata(&local).is_ok_and(|m| m.len() == o.size) {
                continue;
            }
            self.download_object(peer, &o.object, o.size)?;
            bytes += o.size;
        }
        // Same order as a local store: objects, manifest, entry.
        self.store.write_manifest(tier1, &manifest)?;
        self.store.write_entry(&entry)?;
        Ok(Some((key, bytes)))
    }

    fn download_object(&self, peer: &str, hash: &str, size: u64) -> Result<()> {
        let url = format!("{peer}/v1/objects/{hash}");
        let mut resp = self.request(Method::GET, &url, ())?;
        if resp.status() != StatusCode::OK {
            bail!("GET {url}: {}", resp.status());
        }
        let tmp = self.store.tmp_path("peer");
        let result = (|| -> Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            let mut hasher = blake3::Hasher::new();
            let mut reader = resp.body_mut().as_reader().take(size + 1);
            let mut buf = vec![0u8; 256 * 1024];
            let mut got = 0u64;
            loop {
                // A stalled or dropped transfer is an outage, not bad data.
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| Unreachable(format!("{url}: {e}")))?;
                if n == 0 {
                    break;
                }
                got += n as u64;
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])?;
            }
            if got != size || hasher.finalize().to_hex().as_str() != hash {
                bail!("object {hash} from {peer} does not match its hash/size");
            }
            // No fsync, as for locally ingested objects: on macOS it is
            // F_FULLFSYNC, which stalls (and can fail with EIO) on a busy
            // disk; a torn object fails the size check on restore.
            self.store.install_object(&tmp, hash)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    fn exists(&self, url: &str) -> Result<bool> {
        let resp = self.request(Method::HEAD, url, ())?;
        match resp.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => bail!("HEAD {url}: {s}"),
        }
    }

    fn put<B: ureq::AsSendBody>(&self, url: &str, body: B) -> Result<()> {
        let mut resp = self.request(Method::PUT, url, body)?;
        if !resp.status().is_success() {
            let text = resp
                .body_mut()
                .with_config()
                .limit(4096)
                .read_to_string()
                .unwrap_or_default();
            bail!("PUT {url}: {} {}", resp.status(), text.trim());
        }
        Ok(())
    }

    /// Upload one entry unless the peer has it. Objects first, entry last,
    /// so the peer never holds an entry with missing objects. Returns the
    /// object bytes sent, `None` when the peer already had the entry.
    pub fn push_entry(&self, peer: &str, entry: &Entry) -> Result<Option<u64>> {
        let peer = peer.trim_end_matches('/');
        if self.exists(&format!("{peer}/v1/entries/{}", entry.key))? {
            return Ok(None);
        }
        let mut bytes = 0;
        for o in &entry.outputs {
            let url = format!("{peer}/v1/objects/{}", o.object);
            if self.exists(&url)? {
                continue;
            }
            let path = self.store.object_path(&o.object);
            let file =
                std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            self.put(&url, file)?;
            bytes += o.size;
        }
        let manifest = self
            .store
            .read_manifest(&entry.tier1)
            .with_context(|| format!("no manifest for {}", entry.tier1))?;
        self.put(
            &format!("{peer}/v1/manifests/{}", entry.tier1),
            serde_json::to_vec(&manifest)?,
        )?;
        self.put(
            &format!("{peer}/v1/entries/{}", entry.key),
            serde_json::to_vec(entry)?,
        )?;
        Ok(Some(bytes))
    }
}
