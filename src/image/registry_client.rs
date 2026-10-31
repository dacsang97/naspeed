//! Minimal Docker Registry HTTP API v2 client (the OCI distribution spec):
//! manifests (GET/PUT), blobs (HEAD/GET), uploads (POST then monolithic PUT)
//! and cross-repository mounts. Authentication lives in the transport.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use ureq::http::{Method, StatusCode};

use super::registry_transport::{Body, Transport, expect, header};

const MAX_MANIFEST_BYTES: u64 = 8 << 20;
const MAX_CONFIG_BYTES: u64 = 32 << 20;

/// One registry host, possibly serving several repositories.
pub struct RegistryClient {
    http: Transport,
}

/// A fetched manifest: raw bytes (digest-checked for digest references) and media type.
pub struct FetchedManifest {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl RegistryClient {
    /// `insecure` selects plain http (implied for loopback registries).
    pub fn new(registry: &str, insecure: bool) -> Result<RegistryClient> {
        Ok(RegistryClient {
            http: Transport::new(registry, insecure)?,
        })
    }

    /// Registry as written in the image name.
    pub fn registry(&self) -> &str {
        &self.http.registry
    }

    /// GET a manifest by tag or digest, accepting OCI and Docker v2 types.
    pub fn get_manifest(&self, repo: &str, reference: &str) -> Result<FetchedManifest> {
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.http.base_url);
        let accept = super::manifest::ACCEPTED_MANIFEST_TYPES.join(", ");
        let mut resp = self.http.send(
            Method::GET,
            &url,
            &[("Accept", &accept)],
            Body::Empty,
            &[(repo, "pull")],
        )?;
        expect(
            &mut resp,
            &[StatusCode::OK],
            &format!("GET manifest {repo}:{reference}"),
        )?;
        let media_type = header(&resp, "content-type").unwrap_or_default();
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_MANIFEST_BYTES)
            .read_to_vec()
            .with_context(|| format!("read manifest {repo}:{reference}"))?;
        let digest = sha256_digest(&bytes);
        if reference.starts_with("sha256:") && reference != digest {
            bail!("manifest {repo}@{reference} has digest {digest}");
        }
        Ok(FetchedManifest {
            bytes,
            media_type: media_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
        })
    }

    pub fn blob_exists(&self, repo: &str, digest: &str) -> Result<bool> {
        let url = format!("{}/v2/{repo}/blobs/{digest}", self.http.base_url);
        let mut resp = self
            .http
            .send(Method::HEAD, &url, &[], Body::Empty, &[(repo, "pull")])?;
        match resp.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => expect(&mut resp, &[], &format!("HEAD blob {repo}@{digest}")).map(|_| false),
        }
    }

    /// Small blob (an image config) read into memory, digest verified.
    pub fn get_blob_bytes(&self, repo: &str, digest: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.get_blob_into(repo, digest, &mut out, MAX_CONFIG_BYTES)?;
        Ok(out)
    }

    /// Stream a blob into `out`, verifying its sha256 against `digest`.
    pub fn get_blob_into(
        &self,
        repo: &str,
        digest: &str,
        out: &mut dyn Write,
        limit: u64,
    ) -> Result<u64> {
        let url = format!("{}/v2/{repo}/blobs/{digest}", self.http.base_url);
        let mut resp = self
            .http
            .send(Method::GET, &url, &[], Body::Empty, &[(repo, "pull")])?;
        expect(
            &mut resp,
            &[StatusCode::OK],
            &format!("GET blob {repo}@{digest}"),
        )?;
        let mut reader = resp.body_mut().with_config().limit(limit).reader();
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 << 10];
        let mut total = 0u64;
        loop {
            let n = reader
                .read(&mut buf)
                .with_context(|| format!("download blob {digest}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            total += n as u64;
        }
        let got = format!("sha256:{}", hex(&hasher.finalize()));
        if got != digest {
            bail!("blob {repo}@{digest} downloaded with digest {got}");
        }
        Ok(total)
    }

    /// Try a cross-repository mount. `Ok(None)` means mounted; otherwise the
    /// registry opened a regular upload session and returns its URL.
    pub fn mount_blob(&self, repo: &str, digest: &str, from_repo: &str) -> Result<Option<String>> {
        let url = format!(
            "{}/v2/{repo}/blobs/uploads/?mount={digest}&from={from_repo}",
            self.http.base_url
        );
        let scopes = [(repo, "pull,push"), (from_repo, "pull")];
        let mut resp = self
            .http
            .send(Method::POST, &url, &[], Body::Empty, &scopes)?;
        match resp.status() {
            StatusCode::CREATED => Ok(None),
            _ => {
                expect(
                    &mut resp,
                    &[StatusCode::ACCEPTED],
                    &format!("mount {digest} into {repo}"),
                )?;
                Ok(Some(self.http.location(&resp)?))
            }
        }
    }

    pub fn upload_blob_bytes(&self, repo: &str, digest: &str, bytes: &[u8]) -> Result<()> {
        self.upload(repo, digest, Body::Bytes(bytes), None)
    }

    /// Upload from a file; `session` reuses an upload URL (from a failed mount).
    pub fn upload_blob_file(
        &self,
        repo: &str,
        digest: &str,
        path: &Path,
        session: Option<String>,
    ) -> Result<()> {
        self.upload(repo, digest, Body::File(path), session)
    }

    fn upload(&self, repo: &str, digest: &str, body: Body, session: Option<String>) -> Result<()> {
        let scopes = [(repo, "pull,push")];
        let location = match session {
            Some(location) => location,
            None => {
                let url = format!("{}/v2/{repo}/blobs/uploads/", self.http.base_url);
                let mut resp = self
                    .http
                    .send(Method::POST, &url, &[], Body::Empty, &scopes)?;
                expect(
                    &mut resp,
                    &[StatusCode::ACCEPTED],
                    &format!("start upload to {repo}"),
                )?;
                self.http.location(&resp)?
            }
        };
        let sep = if location.contains('?') { '&' } else { '?' };
        let url = format!("{location}{sep}digest={digest}");
        let headers = [("Content-Type", "application/octet-stream")];
        let mut resp = self.http.send(Method::PUT, &url, &headers, body, &scopes)?;
        expect(
            &mut resp,
            &[StatusCode::CREATED],
            &format!("upload {digest} to {repo}"),
        )?;
        Ok(())
    }

    /// PUT a manifest or index under a tag or its digest; returns the digest.
    pub fn put_manifest(
        &self,
        repo: &str,
        reference: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<String> {
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.http.base_url);
        let headers = [("Content-Type", media_type)];
        let mut resp = self.http.send(
            Method::PUT,
            &url,
            &headers,
            Body::Bytes(bytes),
            &[(repo, "pull,push")],
        )?;
        expect(
            &mut resp,
            &[StatusCode::CREATED],
            &format!("PUT manifest {repo}:{reference}"),
        )?;
        Ok(sha256_digest(bytes))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}
