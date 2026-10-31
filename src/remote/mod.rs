//! Peer stores: one machine's store served over HTTP (`naspeed cache serve`,
//! meant for a Tailscale tailnet) and asked by other machines' wrappers on a
//! local miss.
//!
//! ```text
//! GET|HEAD|PUT /v1/manifests/<tier1>   Manifest JSON
//! GET|HEAD|PUT /v1/entries/<key>       Entry JSON
//! GET|HEAD|PUT /v1/objects/<blake3>    raw object bytes
//! GET          /v1/info                {"naspeed": version, "key_version": …}
//! ```
//!
//! Trust: a peer is trusted for what an entry *means* (nothing can check
//! that an rlib matches its key). What
//! crosses the wire is still validated: names must be lowercase hex, entries
//! must parse, match their key and only name paths inside the out dir, and
//! every object is blake3-verified before it enters a store.

pub mod peer_client;
pub mod peer_server;

use std::path::{Component, Path};

use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::store::path_placeholders::{CARGO_HOME_TOKEN, PROFILE_TOKEN, WORKSPACE_TOKEN};
use crate::store::{ENTRY_VERSION, Entry, Manifest};
use crate::wrapper::cache_key::FILE_DEP;

/// More env/file deps than any real crate has; bounds the work a manifest
/// from a peer can make a wrapper do.
const MAX_MANIFEST_DEPS: usize = 20_000;

/// Store file names (`blake3` hex) become paths: refuse anything else.
pub fn is_hex_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A relative path made of plain components only (no `..`, no root).
fn is_inside(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Check an entry received from a peer before it is written to a store.
pub fn validate_entry(entry: &Entry, key: &str, tier1: Option<&str>) -> Result<()> {
    if entry.version != ENTRY_VERSION {
        bail!("entry version {} (want {ENTRY_VERSION})", entry.version);
    }
    if entry.key != key || !is_hex_name(key) {
        bail!("entry key does not match {key}");
    }
    if !is_hex_name(&entry.tier1) || tier1.is_some_and(|t| t != entry.tier1) {
        bail!("entry tier-1 key does not match");
    }
    for o in &entry.outputs {
        if !is_hex_name(&o.object) {
            bail!("bad object name {:?}", o.object);
        }
        // Plain permission bits only: no setuid/setgid/sticky from a peer.
        if o.mode & !0o777 != 0 {
            bail!("output {:?} has mode {:o}", o.name, o.mode);
        }
    }
    let links: HashSet<&str> = entry.symlinks.iter().map(|(n, _)| n.as_str()).collect();
    let mut seen = HashSet::new();
    let names = entry
        .outputs
        .iter()
        .map(|o| o.name.as_str())
        .chain(entry.dep_info.iter().map(|d| d.name.as_str()))
        .chain(entry.texts.iter().map(|t| t.name.as_str()))
        .chain(entry.symlinks.iter().map(|(n, _)| n.as_str()));
    for name in names {
        if !is_inside(name) {
            bail!("path {name:?} leaves the out dir");
        }
        if !seen.insert(name) {
            bail!("path {name:?} appears twice");
        }
        // A file written through one of the entry's own symlinks could land
        // anywhere the link points.
        if Path::new(name)
            .ancestors()
            .skip(1)
            .any(|a| links.contains(a.to_string_lossy().as_ref()))
        {
            bail!("path {name:?} goes through a symlink of the entry");
        }
    }
    for dir in &entry.dirs {
        let through_link = Path::new(dir)
            .ancestors()
            .any(|a| links.contains(a.to_string_lossy().as_ref()));
        if !is_inside(dir) || through_link {
            bail!("dir {dir:?} leaves the out dir");
        }
    }
    Ok(())
}

/// Check a manifest received from a peer: its file deps are hashed on this
/// machine, so they must name files relative to the crate or under one of
/// the placeholder roots, never an arbitrary absolute path like `/`.
pub fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.env_deps.len() > MAX_MANIFEST_DEPS {
        bail!("manifest lists {} deps", manifest.env_deps.len());
    }
    for dep in &manifest.env_deps {
        let Some(path) = dep.strip_prefix(FILE_DEP) else {
            continue;
        };
        let rel = [PROFILE_TOKEN, CARGO_HOME_TOKEN, WORKSPACE_TOKEN]
            .iter()
            .find_map(|t| path.strip_prefix(t))
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(path);
        if !is_inside(rel) {
            bail!("manifest names file {path:?} outside the build");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::OutputFile;

    fn entry() -> Entry {
        Entry {
            version: ENTRY_VERSION,
            key: "a".repeat(64),
            tier1: "b".repeat(64),
            crate_name: "x".into(),
            outputs: vec![OutputFile {
                name: "libx-123.rlib".into(),
                object: "c".repeat(64),
                size: 1,
                mode: 0o644,
            }],
            dep_info: None,
            stdout: String::new(),
            stderr: String::new(),
            created: 0,
            texts: Vec::new(),
            symlinks: Vec::new(),
            dirs: vec!["include/x".into()],
            compile_ms: 0,
        }
    }

    #[test]
    fn accepts_a_plain_entry() {
        validate_entry(&entry(), &"a".repeat(64), Some(&"b".repeat(64))).unwrap();
    }

    #[test]
    fn rejects_writes_through_own_symlinks_and_odd_modes() {
        let key = "a".repeat(64);
        let mut e = entry();
        e.symlinks = vec![("a".into(), "/home/x/.ssh".into())];
        e.texts = vec![crate::store::DepInfoFile {
            name: "a/authorized_keys".into(),
            text: "k".into(),
        }];
        assert!(validate_entry(&e, &key, None).is_err());
        let mut e = entry();
        e.symlinks = vec![("include".into(), "/etc".into())];
        assert!(validate_entry(&e, &key, None).is_err(), "dir under a link");
        let mut e = entry();
        e.outputs[0].mode = 0o4755;
        assert!(validate_entry(&e, &key, None).is_err());
        let mut e = entry();
        e.outputs.push(e.outputs[0].clone());
        assert!(validate_entry(&e, &key, None).is_err(), "duplicate");
    }

    #[test]
    fn manifest_file_deps_stay_inside_the_build() {
        let ok = Manifest {
            env_deps: vec![
                "CARGO_PKG_NAME".into(),
                "file:src/lib.rs".into(),
                format!("file:{CARGO_HOME_TOKEN}/registry/src/x/lib.rs"),
                format!("file:{PROFILE_TOKEN}/build/x/out/gen.rs"),
            ],
        };
        validate_manifest(&ok).unwrap();
        for bad in ["file:/", "file:../../etc/passwd", "file:/etc/passwd"] {
            let m = Manifest {
                env_deps: vec![bad.into()],
            };
            assert!(validate_manifest(&m).is_err(), "{bad}");
        }
        let m = Manifest {
            env_deps: vec![format!("file:{WORKSPACE_TOKEN}/../x")],
        };
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn rejects_traversal_and_mismatches() {
        let key = "a".repeat(64);
        let mut e = entry();
        e.outputs[0].name = "../../.bashrc".into();
        assert!(validate_entry(&e, &key, None).is_err());
        let mut e = entry();
        e.symlinks = vec![("/etc/x".into(), "y".into())];
        assert!(validate_entry(&e, &key, None).is_err());
        let mut e = entry();
        e.outputs[0].object = "../objects".into();
        assert!(validate_entry(&e, &key, None).is_err());
        assert!(validate_entry(&entry(), &"d".repeat(64), None).is_err());
        assert!(validate_entry(&entry(), &key, Some(&"e".repeat(64))).is_err());
    }
}
