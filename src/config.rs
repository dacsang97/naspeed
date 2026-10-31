//! naspeed's own settings: `<config dir>/config.toml` plus `NASPEED_*` env
//! overrides. Read on every wrapper call, so it stays a tiny flat file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, value};

pub const DEFAULT_MIN_FREE_MEM_MB: u64 = 1536;
pub const DEFAULT_STORE_CAP_GB: f64 = 20.0;
pub const DEFAULT_STORE_MAX_AGE_DAYS: f64 = 14.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CowMode {
    /// clonefile / reflink, falling back to a plain copy.
    Auto,
    /// Never clone; always copy (testing, or filesystems with broken clones).
    Copy,
    /// Hardlink objects into target/. Opt-in only: objects must stay read-only.
    Hardlink,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub config_dir: PathBuf,
    /// Store roots, one per volume. A target dir is only cached through a
    /// store on the same device.
    pub stores: Vec<PathBuf>,
    pub slots: usize,
    pub min_free_mem_mb: u64,
    pub store_cap_gb: f64,
    pub cow_mode: CowMode,
    pub disabled: bool,
    /// Cache registry crates' build-script runs (OUT_DIR + output).
    pub build_script_cache: bool,
    /// gc evicts store entries unused for longer than this.
    pub store_max_age_days: f64,
    /// Cache workspace members and path dependencies (keyed per source file).
    pub workspace_cache: bool,
    /// Peer stores (`naspeed cache serve`, e.g. over Tailscale) asked on a
    /// local miss, in order.
    pub remotes: Vec<String>,
    /// Bearer token for peers; read only when a peer is contacted.
    pub remote_token_file: PathBuf,
    /// After a workspace crate is restored from the store, rebuild its
    /// incremental state in the background so the next edit is incremental.
    pub warm_incremental: bool,
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NASPEED_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    home_dir().join(".config").join("naspeed")
}

pub fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"))
}

pub fn default_slots() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0" && v != "false")
        .unwrap_or(false)
}

impl Settings {
    pub fn load() -> Settings {
        let config_dir = config_dir();
        let doc = read_doc(&config_dir.join("config.toml")).unwrap_or_default();
        let mut settings = Settings::from_doc(config_dir, &doc);
        settings.apply_env();
        settings
    }

    fn from_doc(config_dir: PathBuf, doc: &DocumentMut) -> Settings {
        let stores = doc
            .get("stores")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let int = |key: &str| doc.get(key).and_then(|v| v.as_integer());
        let cow_mode = match doc.get("cow_mode").and_then(|v| v.as_str()) {
            Some("copy") => CowMode::Copy,
            Some("hardlink") => CowMode::Hardlink,
            _ => CowMode::Auto,
        };
        let remotes = doc
            .get("remotes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let remote_token_file = doc
            .get("remote_token_file")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.join("remote.token"));
        Settings {
            config_dir,
            stores,
            slots: int("slots")
                .filter(|n| *n > 0)
                .map(|n| n as usize)
                .unwrap_or_else(default_slots),
            min_free_mem_mb: int("min_free_mem_mb")
                .map(|n| n.max(0) as u64)
                .unwrap_or(DEFAULT_MIN_FREE_MEM_MB),
            store_cap_gb: doc
                .get("store_cap_gb")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .unwrap_or(DEFAULT_STORE_CAP_GB),
            cow_mode,
            disabled: false,
            build_script_cache: doc
                .get("build_script_cache")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            store_max_age_days: doc
                .get("store_max_age_days")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .unwrap_or(DEFAULT_STORE_MAX_AGE_DAYS),
            workspace_cache: doc
                .get("workspace_cache")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            remotes,
            remote_token_file,
            warm_incremental: doc
                .get("warm_incremental")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        }
    }

    fn apply_env(&mut self) {
        if let Some(store) = std::env::var_os("NASPEED_STORE") {
            self.stores = vec![PathBuf::from(store)];
        }
        if let Some(n) = std::env::var("NASPEED_SLOTS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
        {
            self.slots = n;
        }
        if let Some(n) = std::env::var("NASPEED_MIN_FREE_MEM_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            self.min_free_mem_mb = n;
        }
        match std::env::var("NASPEED_COW_MODE").as_deref() {
            Ok("copy") => self.cow_mode = CowMode::Copy,
            Ok("hardlink") => self.cow_mode = CowMode::Hardlink,
            Ok("auto") => self.cow_mode = CowMode::Auto,
            _ => {}
        }
        self.disabled = env_flag("NASPEED_DISABLE");
        if let Ok(v) = std::env::var("NASPEED_BUILD_SCRIPT_CACHE") {
            self.build_script_cache = !(v == "0" || v == "false");
        }
        if let Ok(v) = std::env::var("NASPEED_WORKSPACE_CACHE") {
            self.workspace_cache = !(v == "0" || v == "false");
        }
        if let Ok(v) = std::env::var("NASPEED_WARM_INCREMENTAL") {
            self.warm_incremental = !(v == "0" || v == "false");
        }
        // Comma-separated; empty turns peers off for this shell.
        if let Ok(v) = std::env::var("NASPEED_REMOTES") {
            self.remotes = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }

    pub fn slots_dir(&self) -> PathBuf {
        self.config_dir.join("slots")
    }

    /// `NASPEED_REMOTE_TOKEN`, else the first line of `remote_token_file`.
    pub fn remote_token(&self) -> Option<String> {
        if let Ok(t) = std::env::var("NASPEED_REMOTE_TOKEN")
            && !t.trim().is_empty()
        {
            return Some(t.trim().to_owned());
        }
        std::fs::read_to_string(&self.remote_token_file)
            .ok()
            .and_then(|t| t.lines().next().map(|l| l.trim().to_owned()))
            .filter(|t| !t.is_empty())
    }
}

pub fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path)?;
    text.parse::<DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))
}

/// Persist the store list (and leave every other key as the owner wrote it).
pub fn save_stores(config_dir: &Path, stores: &[PathBuf]) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let path = config_dir.join("config.toml");
    let mut doc = read_doc(&path).unwrap_or_default();
    let mut arr = toml_edit::Array::new();
    for store in stores {
        arr.push(store.to_string_lossy().as_ref());
    }
    doc["stores"] = value(arr);
    std::fs::write(&path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_config() {
        let doc: DocumentMut = r#"
stores = ["/Volumes/Work/.naspeed"]
slots = 6
min_free_mem_mb = 1024
store_cap_gb = 12.5
cow_mode = "copy"
remotes = ["http://buildbox:7878"]
"#
        .parse()
        .unwrap();
        let s = Settings::from_doc(PathBuf::from("/tmp/x"), &doc);
        assert_eq!(s.stores, vec![PathBuf::from("/Volumes/Work/.naspeed")]);
        assert_eq!(s.slots, 6);
        assert_eq!(s.min_free_mem_mb, 1024);
        assert_eq!(s.store_cap_gb, 12.5);
        assert_eq!(s.cow_mode, CowMode::Copy);
        assert_eq!(s.remotes, vec!["http://buildbox:7878".to_string()]);
        assert_eq!(s.remote_token_file, PathBuf::from("/tmp/x/remote.token"));
    }

    #[test]
    fn defaults_when_empty() {
        let s = Settings::from_doc(PathBuf::from("/tmp/x"), &DocumentMut::new());
        assert!(s.stores.is_empty());
        assert_eq!(s.slots, default_slots());
        assert_eq!(s.store_cap_gb, DEFAULT_STORE_CAP_GB);
        assert_eq!(s.cow_mode, CowMode::Auto);
    }
}
