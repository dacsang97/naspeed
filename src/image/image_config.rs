//! The runtime settings of the image (`[image]` in ns.toml, the counterpart
//! of a Dockerfile's `USER`/`WORKDIR`/`ENV`/`EXPOSE`/`VOLUME`/`ENTRYPOINT`/
//! `CMD`) and the config JSON derived from the base image's.
//!
//! The base config is kept field for field except for what `ns image` owns:
//! `created`, `User`, `Entrypoint`, `Cmd`, `WorkingDir` (when set), `Env`
//! entries with the same key, added `ExposedPorts`/`Volumes`, labels, the
//! appended `rootfs.diff_ids` entry and its history record.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::layer_builder::image_path_components;
use super::manifest::rfc3339;
use super::reference::Platform;
use crate::project::ns_config::ImageConfig;

/// `[image]` resolved for one binary and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Absolute path of the binary inside the image.
    pub path: String,
    pub entrypoint: Vec<String>,
    /// Empty: no `Cmd` (the base's is dropped, it would become our arguments).
    pub cmd: Vec<String>,
    /// Empty: no `User` (root).
    pub user: String,
    pub workdir: Option<String>,
    /// `KEY=value`.
    pub env: Vec<String>,
    /// `<port>/<proto>`.
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
}

impl RuntimeConfig {
    /// `bin` is the shipped binary name substituted for `{binary}` in `path`.
    pub fn new(image: &ImageConfig, bin: &str) -> Result<RuntimeConfig> {
        let path = image.path.replace("{binary}", bin);
        if path.contains(['{', '}']) {
            bail!(
                "[image].path {:?}: only {{binary}} is substituted",
                image.path
            );
        }
        image_path_components(&path).context("[image].path")?;
        let entrypoint = if image.entrypoint.is_empty() {
            vec![path.clone()]
        } else {
            image.entrypoint.clone()
        };
        if let Some(dir) = &image.workdir
            && !dir.starts_with('/')
        {
            bail!("[image].workdir {dir:?} must be an absolute path");
        }
        for entry in &image.env {
            match entry.split_once('=') {
                Some((key, _)) if !key.is_empty() && !key.contains(char::is_whitespace) => {}
                _ => bail!("[image].env entry {entry:?} must be KEY=value"),
            }
        }
        if let Some(bad) = image.volumes.iter().find(|v| !v.starts_with('/')) {
            bail!("[image].volumes entry {bad:?} must be an absolute path");
        }
        Ok(RuntimeConfig {
            path,
            entrypoint,
            cmd: image.cmd.clone(),
            user: image.user.clone(),
            workdir: image.workdir.clone(),
            env: image.env.clone(),
            ports: image
                .ports
                .iter()
                .map(|p| normalize_port(p))
                .collect::<Result<_>>()?,
            volumes: image.volumes.clone(),
        })
    }
}

/// `8317` → `8317/tcp`; `53/udp` stays.
fn normalize_port(text: &str) -> Result<String> {
    let (port, proto) = text.split_once('/').unwrap_or((text, "tcp"));
    let valid_port = port.parse::<u16>().is_ok_and(|p| p > 0);
    if !valid_port || !matches!(proto, "tcp" | "udp" | "sctp") {
        bail!("[image].ports entry {text:?}: expected <port> or <port>/<tcp|udp|sctp>");
    }
    Ok(format!("{port}/{proto}"))
}

/// What `ns image` changes in the base config.
pub struct ConfigChanges<'a> {
    pub diff_id: &'a str,
    pub runtime: &'a RuntimeConfig,
    pub labels: &'a BTreeMap<String, String>,
    pub created_epoch: u64,
}

pub fn derive_config(base: &[u8], platform: &Platform, changes: &ConfigChanges) -> Result<Vec<u8>> {
    let mut config: Value = serde_json::from_slice(base).context("parse base config")?;
    let Some(root) = config.as_object_mut() else {
        bail!("base config is not a JSON object");
    };
    for (key, want) in [
        ("os", &platform.os),
        ("architecture", &platform.architecture),
    ] {
        if let Some(have) = root.get(key).and_then(Value::as_str)
            && have != want
        {
            bail!("base config is for {key} {have}, expected {want}");
        }
    }
    let created = rfc3339(changes.created_epoch);
    root.insert("created".into(), json!(created));

    let rootfs = root
        .entry("rootfs")
        .or_insert_with(|| json!({"type": "layers", "diff_ids": []}));
    rootfs["diff_ids"]
        .as_array_mut()
        .context("base config rootfs.diff_ids is not an array")?
        .push(json!(changes.diff_id));

    // History must stay aligned with non-empty layers: only extend an existing one.
    if let Some(history) = root.get_mut("history").and_then(Value::as_array_mut) {
        history.push(
            json!({"created": created, "created_by": "ns image", "comment": "application binary"}),
        );
    }

    let cfg = root.entry("config").or_insert_with(|| json!({}));
    let cfg = cfg
        .as_object_mut()
        .context("base config `config` is not an object")?;
    let rt = changes.runtime;
    cfg.insert("Entrypoint".into(), json!(rt.entrypoint));
    if rt.cmd.is_empty() {
        cfg.remove("Cmd");
    } else {
        cfg.insert("Cmd".into(), json!(rt.cmd));
    }
    if rt.user.is_empty() {
        cfg.remove("User");
    } else {
        cfg.insert("User".into(), json!(rt.user));
    }
    if let Some(dir) = &rt.workdir {
        cfg.insert("WorkingDir".into(), json!(dir));
    }
    if !rt.env.is_empty() {
        let env = object_field(cfg, "Env", || json!([]))
            .as_array_mut()
            .context("base config Env is not an array")?;
        for entry in &rt.env {
            let key = entry.split_once('=').map_or(entry.as_str(), |(k, _)| k);
            let same_key = |v: &Value| {
                v.as_str()
                    .is_some_and(|s| s.split_once('=').map_or(s, |(k, _)| k) == key)
            };
            match env.iter_mut().find(|v| same_key(v)) {
                Some(slot) => *slot = json!(entry),
                None => env.push(json!(entry)),
            }
        }
    }
    for (field, keys) in [("ExposedPorts", &rt.ports), ("Volumes", &rt.volumes)] {
        if keys.is_empty() {
            continue;
        }
        let map = object_field(cfg, field, || json!({}))
            .as_object_mut()
            .with_context(|| format!("base config {field} is not an object"))?;
        for key in keys {
            map.insert(key.clone(), json!({}));
        }
    }
    let labels = object_field(cfg, "Labels", || json!({}))
        .as_object_mut()
        .context("base config Labels is not an object")?;
    for (k, v) in changes.labels {
        labels.insert(k.clone(), json!(v));
    }
    serde_json::to_vec(&config).context("serialize config")
}

/// `cfg[field]`, created (also when `null`) with `empty()`.
fn object_field<'a>(
    cfg: &'a mut serde_json::Map<String, Value>,
    field: &str,
    empty: impl Fn() -> Value,
) -> &'a mut Value {
    let value = cfg.entry(field).or_insert_with(&empty);
    if value.is_null() {
        *value = empty();
    }
    value
}

#[cfg(test)]
#[path = "image_config_tests.rs"]
mod tests;
