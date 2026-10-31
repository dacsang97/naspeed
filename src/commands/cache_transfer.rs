//! `ns cache export|import`: move the most recently used store entries through
//! a plain directory (what GitHub Actions cache saves and restores).
//!
//! The directory uses the store's own layout:
//!
//! ```text
//! <dir>/objects/<2>/<blake3>        referenced by the exported entries only
//! <dir>/entries/<2>/<key>.json
//! <dir>/manifests/<2>/<tier1>.json  for the exported entries' tier-1 keys
//! <dir>/memo/                       source-tree / toolchain memo (small)
//! <dir>/last_used.tsv               `<key>\t<unix secs>`, keeps LRU order
//! ```
//!
//! Everything read from `<dir>` is untrusted input (a restored cache archive):
//! names must be lowercase hex before they become paths, entries must parse
//! and match their file name, and objects are content-hashed before they
//! enter the store.

mod export;
mod import;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::config::Settings;
use crate::store::volume_root;

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Copy recently used entries and their objects into `<dir>`.
    Export(CacheExportArgs),
    /// Merge entries from `<dir>` into the store, skipping what is already there.
    Import(CacheImportArgs),
    /// Serve this store to peers over HTTP (default: on the Tailscale IP).
    Serve(super::cache_peer::CacheServeArgs),
    /// Upload recently used entries to a peer started with `serve --allow-push`.
    Push(super::cache_peer::CachePushArgs),
}

#[derive(Debug, Args)]
pub struct CacheExportArgs {
    pub dir: PathBuf,
    /// Size cap for the exported objects, in GB (GitHub allows 10 GB per repo).
    #[arg(long, default_value_t = 5.0)]
    pub max_gb: f64,
    /// Store to read; default: the configured store on the same volume as `<dir>`.
    #[arg(long)]
    pub store: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CacheImportArgs {
    pub dir: PathBuf,
    /// Store to fill; default: the configured store on the same volume as `<dir>`.
    #[arg(long)]
    pub store: Option<PathBuf>,
}

pub fn run(cmd: CacheCommand) -> Result<u8> {
    let settings = Settings::load();
    match cmd {
        CacheCommand::Export(args) => {
            if !(args.max_gb.is_finite() && args.max_gb >= 0.0) {
                bail!("--max-gb must be a non-negative number");
            }
            std::fs::create_dir_all(&args.dir)
                .with_context(|| format!("create {}", args.dir.display()))?;
            let root = pick_store(&settings, args.store, &args.dir, false)?;
            let cap = (args.max_gb * 1024.0 * 1024.0 * 1024.0) as u64;
            export::run(&root, &args.dir, cap, settings.cow_mode)?;
        }
        CacheCommand::Import(args) => {
            if !args.dir.join("entries").is_dir() {
                bail!(
                    "{} is not a naspeed cache export (no entries/ dir)",
                    args.dir.display()
                );
            }
            let root = pick_store(&settings, args.store, &args.dir, true)?;
            import::run(&root, &args.dir, settings.cow_mode)?;
        }
        CacheCommand::Serve(args) => return super::cache_peer::serve(&settings, args),
        CacheCommand::Push(args) => return super::cache_peer::push(&settings, args),
    }
    Ok(0)
}

/// `--store`, else the configured store on `dir`'s volume (clones work
/// there), else the first configured store. Import may create the store;
/// export needs an existing one.
fn pick_store(
    settings: &Settings,
    explicit: Option<PathBuf>,
    dir: &Path,
    create: bool,
) -> Result<PathBuf> {
    let root = explicit
        .or_else(|| volume_root::store_for(dir, &settings.stores))
        .or_else(|| settings.stores.first().cloned())
        .context("no store configured: run `naspeed setup --store <dir>` or set NASPEED_STORE")?;
    if !create && !root.is_dir() {
        bail!("store {} does not exist", root.display());
    }
    Ok(root)
}

/// Store file names (`blake3` hex) become paths: refuse anything else.
fn is_hex_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Regular files under a store dir, recursively (for other commands).
pub(crate) fn walk_store_files(dir: &Path, out: &mut Vec<PathBuf>) {
    walk_files(dir, out)
}

/// Regular files under `dir`, recursively (symlinks are never followed).
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_files(&e.path(), out),
            Ok(t) if t.is_file() => out.push(e.path()),
            _ => {}
        }
    }
}

/// File stem of a store file (`<hash>` or `<hash>.json`) if it is a valid name.
fn hex_stem(path: &Path) -> Option<String> {
    let stem = path.file_name()?.to_str()?;
    let stem = stem.strip_suffix(".json").unwrap_or(stem);
    is_hex_name(stem).then(|| stem.to_owned())
}

/// Write `bytes` to `dst` through a temp file in `dst`'s dir, then rename.
fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_sibling(dst)?;
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("rename to {}", dst.display()))
}

/// A not-yet-existing temp path next to `dst` (same dir, so the rename is
/// atomic), creating the parent dir. Starts with `.` and ends in `.tmp` so
/// the walkers never mistake it for a store file.
fn temp_sibling(dst: &Path) -> Result<PathBuf> {
    let parent = dst
        .parent()
        .with_context(|| format!("{} has no parent", dst.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(parent.join(format!(".{name}.{}.{nanos}.tmp", std::process::id())))
}

/// `last_used.tsv` inside an export dir.
const LAST_USED_FILE: &str = "last_used.tsv";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_names_only() {
        assert!(is_hex_name(&"a1".repeat(32)));
        assert!(!is_hex_name("../../etc/passwd"));
        assert!(!is_hex_name(&"A1".repeat(32)));
        assert!(!is_hex_name(&"a1".repeat(31)));
        let key = "0f".repeat(32);
        assert_eq!(hex_stem(Path::new(&format!("e/0f/{key}.json"))), Some(key));
        assert_eq!(hex_stem(Path::new("e/0f/.x.json.1.2.tmp")), None);
    }

    #[test]
    fn atomic_write_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("x/y.json");
        write_atomic(&dst, b"one").unwrap();
        write_atomic(&dst, b"two").unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(dir.path().join("x"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1);
    }
}
