//! `naspeed cache serve`: expose this machine's store to peers. Reads need
//! no store lock (files are written by rename, objects are immutable); a
//! push holds the shared lock like a wrapper so gc cannot run underneath.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use super::{is_hex_name, validate_entry, validate_manifest};
use crate::store::usage_log::Record;
use crate::store::{Entry, Manifest, Store};
use crate::wrapper::cache_key::KEY_VERSION;

const JSON_LIMIT: u64 = 64 << 20;
const OBJECT_LIMIT: u64 = 8 << 30;
const WORKERS: usize = 8;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub listen: SocketAddr,
    /// Required as `Authorization: Bearer <token>` on every request when set.
    pub token: Option<String>,
    /// Accept PUTs (a shared VPS / CI cache); read-only otherwise.
    pub allow_push: bool,
}

pub struct RunningServer {
    server: Arc<Server>,
    workers: Vec<std::thread::JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl RunningServer {
    /// Block until the server stops (it never does on its own).
    pub fn wait(self) {
        for w in self.workers {
            let _ = w.join();
        }
    }

    pub fn stop(self) {
        self.server.unblock();
        for _ in 1..WORKERS {
            self.server.unblock();
        }
        for w in self.workers {
            let _ = w.join();
        }
    }
}

pub fn start(store: Store, opts: ServeOptions) -> Result<RunningServer> {
    let server =
        Server::http(opts.listen).map_err(|e| anyhow!("listen on {}: {e}", opts.listen))?;
    let addr = server
        .server_addr()
        .to_ip()
        .context("server has no IP address")?;
    let server = Arc::new(server);
    let ctx = Arc::new((store, opts));
    let workers = (0..WORKERS)
        .map(|_| {
            let server = server.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                while let Ok(req) = server.recv() {
                    handle(&ctx.0, &ctx.1, req);
                }
            })
        })
        .collect();
    Ok(RunningServer {
        server,
        workers,
        addr,
    })
}

/// Constant-time token comparison.
fn token_ok(req: &Request, token: &str) -> bool {
    let Some(given) = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
    else {
        return false;
    };
    let (a, b) = (given.trim().as_bytes(), token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn text(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(StatusCode(status))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Manifest,
    Entry,
    Object,
}

fn route(url: &str) -> Option<(Kind, &str)> {
    let rest = url.split('?').next()?.strip_prefix("/v1/")?;
    let (kind, name) = rest.split_once('/')?;
    let kind = match kind {
        "manifests" => Kind::Manifest,
        "entries" => Kind::Entry,
        "objects" => Kind::Object,
        _ => return None,
    };
    is_hex_name(name).then_some((kind, name))
}

fn path_of(store: &Store, kind: Kind, name: &str) -> PathBuf {
    match kind {
        Kind::Manifest => store.manifest_path(name),
        Kind::Entry => store.entry_path(name),
        Kind::Object => store.object_path(name),
    }
}

fn handle(store: &Store, opts: &ServeOptions, mut req: Request) {
    if let Some(token) = &opts.token
        && !token_ok(&req, token)
    {
        let _ = req.respond(text(401, "token required\n"));
        return;
    }
    let method = req.method().clone();
    let url = req.url().to_owned();
    if method == Method::Get && url == "/v1/info" {
        let body = serde_json::json!({
            "naspeed": env!("CARGO_PKG_VERSION"),
            "key_version": KEY_VERSION,
            "push": opts.allow_push,
        });
        let _ = req.respond(text(200, &body.to_string()));
        return;
    }
    let Some((kind, name)) = route(&url) else {
        let _ = req.respond(text(404, "not found\n"));
        return;
    };
    let path = path_of(store, kind, name);
    match method {
        Method::Get | Method::Head => {
            let Ok(file) = std::fs::File::open(&path) else {
                let _ = req.respond(text(404, "not found\n"));
                return;
            };
            if method == Method::Get && kind == Kind::Entry {
                store.append_usage(&Record::Served {
                    key: name.to_owned(),
                });
            }
            let len = file.metadata().map(|m| m.len()).ok();
            let mut resp = Response::new(
                StatusCode(200),
                Vec::new(),
                file,
                len.map(|l| l as usize),
                None,
            );
            if kind != Kind::Object {
                resp.add_header(
                    Header::from_bytes("Content-Type", "application/json").expect("static header"),
                );
            }
            let _ = req.respond(resp);
        }
        Method::Put if !opts.allow_push => {
            let _ = req.respond(text(403, "this peer is read-only (serve --allow-push)\n"));
        }
        Method::Put => {
            // The body is read before the store lock is taken (only for the
            // final write), so a stalled uploader never blocks gc.
            let result = receive(store, kind, name, &mut req);
            let _ = match result {
                Ok(()) => req.respond(text(201, "stored\n")),
                Err(e) => req.respond(text(400, &format!("{e:#}\n"))),
            };
        }
        _ => {
            let _ = req.respond(text(405, "method not allowed\n"));
        }
    }
}

fn read_json_body(req: &mut Request) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    req.as_reader()
        .take(JSON_LIMIT + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > JSON_LIMIT {
        bail!("body too large");
    }
    Ok(body)
}

fn receive(store: &Store, kind: Kind, name: &str, req: &mut Request) -> Result<()> {
    match kind {
        Kind::Object => {
            if std::fs::metadata(store.object_path(name)).is_ok() {
                // Drain so the connection can be reused.
                std::io::copy(
                    &mut req.as_reader().take(OBJECT_LIMIT),
                    &mut std::io::sink(),
                )?;
                return Ok(());
            }
            let tmp = store.tmp_path("push");
            let result = (|| -> Result<()> {
                let mut file = std::fs::File::create(&tmp)?;
                let mut hasher = blake3::Hasher::new();
                let mut reader = req.as_reader().take(OBJECT_LIMIT + 1);
                let mut buf = vec![0u8; 256 * 1024];
                let mut got = 0u64;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    got += n as u64;
                    if got > OBJECT_LIMIT {
                        bail!("object too large");
                    }
                    hasher.update(&buf[..n]);
                    file.write_all(&buf[..n])?;
                }
                if hasher.finalize().to_hex().as_str() != name {
                    bail!("content does not hash to {name}");
                }
                // No fsync, as for locally ingested objects: on macOS it is
                // F_FULLFSYNC, which stalls (and can fail with EIO) on a busy
                // disk; a torn object fails the size check on restore.
                let _lock = store.shared_lock()?;
                store.install_object(&tmp, name)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result
        }
        Kind::Manifest => {
            let manifest: Manifest = serde_json::from_slice(&read_json_body(req)?)?;
            validate_manifest(&manifest)?;
            let _lock = store.shared_lock()?;
            store.write_manifest(name, &manifest)
        }
        Kind::Entry => {
            let entry: Entry = serde_json::from_slice(&read_json_body(req)?)?;
            validate_entry(&entry, name, None)?;
            let _lock = store.shared_lock()?;
            if store.read_manifest(&entry.tier1).is_none() {
                bail!("push the manifest {} first", entry.tier1);
            }
            for o in &entry.outputs {
                if !std::fs::metadata(store.object_path(&o.object)).is_ok_and(|m| m.len() == o.size)
                {
                    bail!("object {} missing: push objects before the entry", o.object);
                }
            }
            // gc indexes entries it finds on disk; no usage record needed.
            store.write_entry(&entry)
        }
    }
}

/// `tailscale ip -4`, trying the macOS app bundle when the CLI is not on PATH.
pub fn tailscale_ipv4() -> Result<std::net::IpAddr> {
    let candidates = [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ];
    for exe in candidates {
        let Ok(out) = std::process::Command::new(exe).args(["ip", "-4"]).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        if let Some(ip) = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.trim().parse().ok())
        {
            return Ok(ip);
        }
    }
    bail!("no Tailscale IPv4 address (is tailscale up?); pass --listen <ip:port> instead")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_only_hex_names() {
        let h = "a".repeat(64);
        assert_eq!(
            route(&format!("/v1/objects/{h}")),
            Some((Kind::Object, h.as_str()))
        );
        assert_eq!(
            route(&format!("/v1/entries/{h}?x=1")),
            Some((Kind::Entry, h.as_str()))
        );
        assert_eq!(route("/v1/objects/../../etc/passwd"), None);
        assert_eq!(route(&format!("/v1/other/{h}")), None);
        assert_eq!(route("/"), None);
    }
}
