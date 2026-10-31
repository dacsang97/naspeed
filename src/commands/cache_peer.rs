//! `naspeed cache serve` / `naspeed cache push`: the peer cache commands.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::config::Settings;
use crate::remote::peer_client::PeerClient;
use crate::remote::peer_server::{self, ServeOptions};
use crate::store::physical_usage::human;
use crate::store::{Entry, Store, now_secs, usage_log};

pub const DEFAULT_PORT: u16 = 7878;

#[derive(Debug, Args)]
pub struct CacheServeArgs {
    /// Address to bind; default `<tailscale IPv4>:7878`. Binding 0.0.0.0
    /// exposes the store to every network the machine is on.
    #[arg(long)]
    pub listen: Option<SocketAddr>,
    /// Port used with the Tailscale address.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Accept uploads (`naspeed cache push`); requires a token.
    #[arg(long)]
    pub allow_push: bool,
    /// Store to serve; default: the first configured store.
    #[arg(long)]
    pub store: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CachePushArgs {
    /// Peer URL, e.g. `http://build-vps:7878`.
    pub url: String,
    /// Push entries used or created within this many hours.
    #[arg(long, default_value_t = 24.0)]
    pub since_hours: f64,
    /// Store to read; default: the first configured store.
    #[arg(long)]
    pub store: Option<PathBuf>,
}

fn open_store(settings: &Settings, explicit: Option<PathBuf>) -> Result<Store> {
    let root = explicit
        .or_else(|| settings.stores.first().cloned())
        .context("no store configured: run `naspeed setup` or pass --store")?;
    if !root.is_dir() {
        bail!("store {} does not exist", root.display());
    }
    Store::open(&root, settings.cow_mode)
}

pub fn serve(settings: &Settings, args: CacheServeArgs) -> Result<u8> {
    let store = open_store(settings, args.store)?;
    let token = settings.remote_token();
    if args.allow_push && token.is_none() {
        bail!(
            "--allow-push needs a token: put one in {} or NASPEED_REMOTE_TOKEN",
            settings.remote_token_file.display()
        );
    }
    let listen = match args.listen {
        Some(addr) => addr,
        None => SocketAddr::new(peer_server::tailscale_ipv4()?, args.port),
    };
    if listen.ip().is_unspecified() && token.is_none() {
        crate::ui_warn!(format!(
            "{listen} without a token lets every network this machine is on read the store"
        ));
    }
    let root = store.root.clone();
    let server = peer_server::start(
        store,
        ServeOptions {
            listen,
            token: token.clone(),
            allow_push: args.allow_push,
        },
    )?;
    crate::ui_info!("serving store";
        path = root.display(), url = format!("http://{}", server.addr),
        push = if args.allow_push { "allowed" } else { "read-only" },
        token = if token.is_some() { "required" } else { "none" });
    let _hint = crate::ui::indent();
    crate::ui_info!(format!(
        "peers use it with remotes = [\"http://{}\"] in ~/.config/naspeed/config.toml (or NASPEED_REMOTES)",
        server.addr
    ));
    server.wait();
    Ok(0)
}

/// Entries used (index) or written (mtime) since `since`, newest first.
fn recent_entries(store: &Store, since: u64) -> Result<Vec<Entry>> {
    if let Ok(Some(_lock)) = store.try_exclusive_lock()
        && let Ok(mut conn) = usage_log::open_index(&store.root)
    {
        let _ = usage_log::merge(&store.root, &mut conn);
    }
    let conn = usage_log::open_index(&store.root)?;
    let mut keys: Vec<(i64, String)> = conn
        .prepare("SELECT last_used, key FROM entries WHERE last_used >= ?1")?
        .query_map([since as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut files = Vec::new();
    crate::commands::cache_transfer::walk_store_files(&store.entries_dir(), &mut files);
    for path in files {
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if modified >= since
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            keys.push((modified as i64, stem.to_owned()));
        }
    }
    keys.sort_by(|a, b| b.cmp(a));
    let mut seen = std::collections::HashSet::new();
    Ok(keys
        .into_iter()
        .filter(|(_, k)| seen.insert(k.clone()))
        .filter_map(|(_, k)| store.read_entry(&k))
        .collect())
}

pub fn push(settings: &Settings, args: CachePushArgs) -> Result<u8> {
    let store = open_store(settings, args.store)?;
    let since = now_secs().saturating_sub((args.since_hours.max(0.0) * 3600.0) as u64);
    let entries = recent_entries(&store, since)?;
    let client = PeerClient::with_peers(&store, vec![args.url.clone()], settings.remote_token())
        .context("peer URL")?;
    let (mut sent, mut skipped, mut bytes) = (0, 0, 0);
    for entry in &entries {
        match client
            .push_entry(&args.url, entry)
            .with_context(|| format!("push {} ({})", entry.key, entry.crate_name))?
        {
            Some(b) => {
                sent += 1;
                bytes += b;
            }
            None => skipped += 1,
        }
    }
    crate::ui_info!(format!("pushed {sent} entries");
        size = human(bytes), url = args.url, already_there = skipped);
    Ok(0)
}
