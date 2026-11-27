//! Observed workspace-library actions. Cargo still owns its fingerprints,
//! linker invocations and incremental sessions. The wrapper streams rustc's
//! diagnostics normally, then publishes the completed outputs to the CAS.

use std::path::PathBuf;

use anyhow::Result;
use filetime::FileTime;

use super::action_result::{ACTION_RESULT_VERSION, ActionResult, Provenance, StreamRef};
use crate::store::tree::{self, EntryKind, TreeEntry};
use crate::store::{DepInfoFile, ENTRY_VERSION, Entry, Manifest, OutputFile, Store, action_cache};

pub(crate) fn manifest_dir(store: &Store) -> PathBuf {
    store.manifests_dir().join("rust-actions")
}

fn manifest_path(store: &Store, tier1: &str) -> PathBuf {
    manifest_dir(store)
        .join(&tier1[..2])
        .join(format!("{tier1}.json"))
}

pub(crate) fn read_manifest(store: &Store, tier1: &str) -> Option<Manifest> {
    let bytes = std::fs::read(manifest_path(store, tier1)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// The tier-2 digest commits the normalized argv, compiler, source files,
// externs, declared build-script reads and compile-time environment.
fn action_key(tier2: &str) -> String {
    let mut hash = blake3::Hasher::new_derive_key("naspeed Rust workspace-library result v1");
    hash.update(tier2.as_bytes());
    hash.finalize().to_hex().to_string()
}

pub(crate) fn publish(store: &Store, result: &Entry, manifest: &Manifest) -> Result<()> {
    let action_key = action_key(&result.key);
    let mut entries = Vec::with_capacity(result.outputs.len() + 1);
    for o in &result.outputs {
        entries.push(TreeEntry {
            path: o.name.clone(),
            kind: EntryKind::File,
            digest: o.object.clone(),
            size: o.size,
            mode: o.mode,
            target: String::new(),
        });
    }
    if let Some(dep) = &result.dep_info {
        let object = action_cache::put_bytes(store, dep.text.as_bytes())?;
        entries.push(TreeEntry {
            path: dep.name.clone(),
            kind: EntryKind::File,
            digest: object.digest,
            size: object.size,
            mode: 0o644,
            target: String::new(),
        });
    }
    let stream = |text: &str| {
        (!text.is_empty())
            .then(|| action_cache::put_bytes(store, text.as_bytes()))
            .transpose()
    };
    let record = ActionResult {
        version: ACTION_RESULT_VERSION,
        action_key,
        exit_status: 0,
        stdout: stream(&result.stdout)?,
        stderr: stream(&result.stderr)?,
        output_tree: tree::write_tree(store, entries)?,
        duration_ms: result.compile_ms,
        resources: Default::default(),
        provenance: Provenance {
            producer: "rustc".into(),
            trust: "trusted".into(),
            origin: String::new(),
        },
        created: result.created,
    };
    action_cache::store_result(store, &record.action_key, &record)?;
    store.write_file(
        &manifest_path(store, &result.tier1),
        &serde_json::to_vec(manifest)?,
    )
}

fn text(store: &Store, stream: Option<StreamRef>) -> Option<String> {
    stream.map_or_else(
        || Some(String::new()),
        |s| std::fs::read_to_string(store.object_path(&s.digest)).ok(),
    )
}

/// Decode a verified action to the native rustc replay format. Every path
/// must be a single rustc artifact name, never a link or nested tree path.
pub(crate) fn read(
    store: &Store,
    tier1: &str,
    tier2: &str,
    stem: &str,
    crate_name: &str,
) -> Option<Entry> {
    let action_key = action_key(tier2);
    let (record, tree) = match action_cache::lookup(store, &action_key) {
        action_cache::Lookup::Hit(record, tree) => (record, tree),
        action_cache::Lookup::Miss(reason) => {
            tracing::debug!(%crate_name, ?reason, "rust action miss");
            return None;
        }
    };
    let dep_name = format!("{stem}.d");
    let auxiliary_prefix = format!("{stem}.");
    let artifact_prefix = format!("lib{stem}.");
    let mut outputs = Vec::with_capacity(tree.entries.len());
    let mut dep_info = None;
    for e in tree.entries {
        if e.kind != EntryKind::File
            || e.path.contains('/')
            || tree::validate_path(&e.path).is_err()
        {
            return None;
        }
        if e.path == dep_name && dep_info.is_none() {
            dep_info = Some(DepInfoFile {
                name: e.path,
                text: std::fs::read_to_string(store.object_path(&e.digest)).ok()?,
            });
        } else if e.path != dep_name
            && (e.path.starts_with(&artifact_prefix) || e.path.starts_with(&auxiliary_prefix))
        {
            outputs.push(OutputFile {
                name: e.path,
                object: e.digest,
                size: e.size,
                mode: e.mode & 0o777,
            });
        } else {
            return None;
        }
    }
    if outputs.is_empty() || dep_info.is_none() {
        return None;
    }
    let entry = Entry {
        version: ENTRY_VERSION,
        key: tier2.to_owned(),
        tier1: tier1.to_owned(),
        crate_name: crate_name.to_owned(),
        outputs,
        dep_info,
        stdout: text(store, record.stdout)?,
        stderr: text(store, record.stderr)?,
        created: record.created,
        texts: vec![],
        symlinks: vec![],
        dirs: vec![],
        compile_ms: record.duration_ms,
    };
    let now = FileTime::now();
    let _ = filetime::set_file_mtime(store.action_path(&action_key), now);
    let _ = filetime::set_file_mtime(manifest_path(store, tier1), now);
    Some(entry)
}
