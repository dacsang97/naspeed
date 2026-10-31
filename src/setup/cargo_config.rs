//! Edit `~/.cargo/config.toml` with `toml_edit` (comments and layout kept),
//! recording the original value of every key `setup` touches so `uninstall`
//! can undo exactly those keys and leave later owner edits alone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, TableLike, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyChange {
    pub path: Vec<String>,
    /// Original value as TOML source (`"other-wrapper"`, `2`, `false`), `None`
    /// when the key did not exist.
    pub original: Option<String>,
    /// What setup wrote, `None` when setup removed the key.
    pub written: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetupState {
    pub cargo_config: PathBuf,
    /// Backup taken before naspeed ever touched the file; never overwritten.
    pub first_backup: Option<PathBuf>,
    pub config_existed: bool,
    pub changes: Vec<KeyChange>,
    /// blake3 of the config exactly as setup last left it.
    pub written_hash: Option<String>,
    pub incremental_note: Option<String>,
    #[serde(default)]
    pub shims: Vec<PathBuf>,
}

pub fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join("setup-state.json")
}

pub fn load_state(config_dir: &Path) -> Option<SetupState> {
    let text = std::fs::read(state_path(config_dir)).ok()?;
    serde_json::from_slice(&text).ok()
}

pub fn save_state(config_dir: &Path, state: &SetupState) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(state_path(config_dir), serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

pub fn get(doc: &DocumentMut, path: &[String]) -> Option<String> {
    let mut item: &Item = doc.as_item();
    for seg in path {
        item = item.as_table_like()?.get(seg)?;
    }
    item.as_value()
        .map(|v| v.clone().decorated("", "").to_string())
}

pub fn set(doc: &mut DocumentMut, path: &[String], value: Value) {
    let (last, parents) = path.split_last().expect("non-empty key path");
    let mut current: &mut dyn TableLike = doc.as_table_mut();
    for seg in parents {
        if current.get(seg).is_none_or(|item| !item.is_table_like()) {
            // Implicit so `target.<triple>.linker` prints as a single
            // `[target.<triple>]` header without an empty `[target]`.
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            current.insert(seg, Item::Table(table));
        }
        current = current
            .get_mut(seg)
            .and_then(Item::as_table_like_mut)
            .expect("table inserted above");
    }
    match current.get_mut(last) {
        Some(Item::Value(existing)) => {
            let decor = existing.decor().clone();
            let mut value = value;
            *value.decor_mut() = decor;
            *existing = value;
        }
        _ => {
            current.insert(last, Item::Value(value));
        }
    }
}

pub fn remove(doc: &mut DocumentMut, path: &[String]) -> bool {
    let (last, parents) = path.split_last().expect("non-empty key path");
    let mut item: &mut Item = doc.as_item_mut();
    for seg in parents {
        let Some(next) = item.as_table_like_mut().and_then(|t| t.get_mut(seg)) else {
            return false;
        };
        item = next;
    }
    item.as_table_like_mut()
        .and_then(|t| t.remove(last))
        .is_some()
}

pub fn parse_value(src: &str) -> Result<Value> {
    src.parse::<Value>()
        .with_context(|| format!("parse TOML value {src}"))
}

pub fn read_config(path: &Path) -> Result<(String, DocumentMut)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))?;
    Ok((text, doc))
}

/// A requested edit: set to a TOML value, or remove.
#[derive(Debug, Clone)]
pub enum Edit {
    Set(Vec<String>, String),
    Remove(Vec<String>),
}

pub fn key(path: &[&str]) -> Vec<String> {
    path.iter().map(|s| s.to_string()).collect()
}

/// Apply edits, recording originals for keys not yet in `state.changes`.
/// Returns whether the document changed.
pub fn apply(doc: &mut DocumentMut, edits: &[Edit], state: &mut SetupState) -> Result<bool> {
    let mut changed = false;
    for edit in edits {
        let (path, written) = match edit {
            Edit::Set(path, value) => (path, Some(value.clone())),
            Edit::Remove(path) => (path, None),
        };
        let current = get(doc, path);
        let normalized_written = match &written {
            Some(v) => Some(parse_value(v)?.decorated("", "").to_string()),
            None => None,
        };
        if current == normalized_written {
            continue;
        }
        if let Some(existing) = state.changes.iter_mut().find(|c| &c.path == path) {
            existing.written = normalized_written.clone();
        } else {
            state.changes.push(KeyChange {
                path: path.clone(),
                original: current,
                written: normalized_written.clone(),
            });
        }
        match &written {
            Some(v) => set(doc, path, parse_value(v)?),
            None => {
                remove(doc, path);
            }
        }
        changed = true;
    }
    Ok(changed)
}

/// Undo `state.changes` on `doc` key by key. A key the owner changed after
/// setup is left as the owner set it; its path is returned in `kept`.
pub fn revert(doc: &mut DocumentMut, state: &SetupState) -> Result<Vec<Vec<String>>> {
    let mut kept = Vec::new();
    for change in state.changes.iter().rev() {
        let current = get(doc, &change.path);
        if current != change.written {
            kept.push(change.path.clone());
            continue;
        }
        match &change.original {
            Some(v) => set(doc, &change.path, parse_value(v)?),
            None => {
                remove(doc, &change.path);
            }
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "# Existing user config.\n[build]\nrustc-wrapper = \"sccache\"\njobs = 2\nincremental = false\n\n[env]\nSCCACHE_IDLE_TIMEOUT = \"600\"\n";

    fn edits() -> Vec<Edit> {
        vec![
            Edit::Set(
                key(&["build", "rustc-wrapper"]),
                "\"/home/dev/.cargo/bin/naspeed\"".into(),
            ),
            Edit::Set(key(&["build", "jobs"]), "4".into()),
            Edit::Remove(key(&["env", "SCCACHE_IDLE_TIMEOUT"])),
            Edit::Set(
                key(&["target", "x86_64-unknown-linux-musl", "linker"]),
                "\"/x/shim\"".into(),
            ),
        ]
    }

    #[test]
    fn apply_keeps_comments_and_revert_restores_values() {
        let mut doc: DocumentMut = ORIGINAL.parse().unwrap();
        let mut state = SetupState::default();
        assert!(apply(&mut doc, &edits(), &mut state).unwrap());
        let text = doc.to_string();
        assert!(text.starts_with("# Existing user config."));
        assert!(text.contains("rustc-wrapper = \"/home/dev/.cargo/bin/naspeed\""));
        assert!(text.contains("jobs = 4"));
        assert!(!text.contains("SCCACHE_IDLE_TIMEOUT"));
        assert!(
            text.contains("[target.x86_64-unknown-linux-musl]"),
            "{text}"
        );

        // Idempotent: nothing to change, originals untouched.
        let before = state.changes.clone();
        assert!(!apply(&mut doc, &edits(), &mut state).unwrap());
        assert_eq!(state.changes, before);

        // Owner edits another key and also a key setup wrote.
        set(
            &mut doc,
            &key(&["build", "target-dir"]),
            parse_value("\"/t\"").unwrap(),
        );
        set(
            &mut doc,
            &key(&["build", "jobs"]),
            parse_value("6").unwrap(),
        );
        let kept = revert(&mut doc, &state).unwrap();
        assert_eq!(kept, vec![key(&["build", "jobs"])]);
        assert_eq!(
            get(&doc, &key(&["build", "rustc-wrapper"])).as_deref(),
            Some("\"sccache\"")
        );
        assert_eq!(
            get(&doc, &key(&["env", "SCCACHE_IDLE_TIMEOUT"])).as_deref(),
            Some("\"600\"")
        );
        assert_eq!(
            get(&doc, &key(&["build", "target-dir"])).as_deref(),
            Some("\"/t\"")
        );
        assert_eq!(get(&doc, &key(&["build", "jobs"])).as_deref(), Some("6"));
        assert_eq!(
            get(
                &doc,
                &key(&["target", "x86_64-unknown-linux-musl", "linker"])
            ),
            None
        );
    }
}
