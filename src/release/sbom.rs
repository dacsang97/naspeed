//! SPDX 2.3 JSON SBOM for one release build, from `cargo metadata` with
//! dependencies resolved for the build's features (and a linux target of the
//! matrix when there is one).
//!
//! Only packages reachable from the build's package through normal
//! dependencies are listed (no dev or build dependencies). The document is
//! deterministic: packages and relationships are sorted, `created` is
//! `SOURCE_DATE_EPOCH`, and the namespace is derived from binary name,
//! version and commit.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::archive::civil_from_epoch;
use super::build_plan::ReleaseBuild;
use super::checksums::hex;

/// What identifies the document besides the dependency graph.
pub struct SbomInfo<'a> {
    pub binary: &'a str,
    pub version: &'a str,
    pub commit: Option<&'a str>,
    pub source_date_epoch: u64,
}

/// `<binary>-<version>.spdx.json`.
pub fn file_name(binary: &str, version: &str) -> String {
    format!("{binary}-{version}.spdx.json")
}

/// `cargo metadata` (with deps) for `build`, resolved for its features and
/// for the first linux target among its targets, if any.
pub fn cargo_metadata(manifest_path: &Path, build: &ReleaseBuild) -> Result<Value> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(manifest_path)
        .args(build.feature_args());
    if let Some(linux) = build.targets.iter().find(|t| t.contains("-linux-")) {
        let triple = linux.split_once(".").map_or(linux.as_str(), |(t, _)| t);
        cmd.args(["--filter-platform", triple]);
    }
    let out = cmd.output().context("run cargo metadata")?;
    if !out.status.success() {
        bail!(
            "cargo metadata for the SBOM of {} failed: {}",
            build.id(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse cargo metadata")
}

/// SPDX license expression from a Cargo `license` field.
fn license(pkg: &Value) -> String {
    match pkg["license"].as_str() {
        // Old Cargo style `MIT/Apache-2.0` means "either".
        Some(l) if !l.trim().is_empty() => l.replace('/', " OR "),
        _ => "NOASSERTION".into(),
    }
}

fn spdx_id_part(s: &str) -> String {
    s.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                true => c,
                false => '-',
            },
        )
        .collect()
}

fn download_location(pkg: &Value) -> String {
    let source = pkg["source"].as_str().unwrap_or_default();
    let crates_io = source == "registry+https://github.com/rust-lang/crates.io-index"
        || source == "sparse+https://index.crates.io/";
    match crates_io {
        true => format!(
            "https://crates.io/api/v1/crates/{}/{}/download",
            pkg["name"].as_str().unwrap_or_default(),
            pkg["version"].as_str().unwrap_or_default()
        ),
        false => "NOASSERTION".into(),
    }
}

/// The SPDX document for the package `package` of the workspace in `meta`.
pub fn document(meta: &Value, package: &str, info: &SbomInfo) -> Result<Value> {
    let members: BTreeSet<&str> = meta["workspace_members"]
        .as_array()
        .context("cargo metadata: workspace_members")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let packages: BTreeMap<&str, &Value> = meta["packages"]
        .as_array()
        .context("cargo metadata: packages")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    let root = packages
        .iter()
        .find(|(id, p)| members.contains(*id) && p["name"] == package)
        .map(|(id, _)| *id)
        .with_context(|| format!("package {package} is not in cargo metadata"))?;
    // id → normal dependency ids, from the resolve graph.
    let edges: BTreeMap<&str, Vec<&str>> = meta["resolve"]["nodes"]
        .as_array()
        .context("cargo metadata has no resolve graph")?
        .iter()
        .filter_map(|n| {
            let deps = n["deps"]
                .as_array()?
                .iter()
                .filter(|d| {
                    d["dep_kinds"]
                        .as_array()
                        .is_some_and(|k| k.iter().any(|k| k["kind"].is_null()))
                })
                .filter_map(|d| d["pkg"].as_str())
                .collect();
            Some((n["id"].as_str()?, deps))
        })
        .collect();

    let mut reachable = BTreeSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(id) = queue.pop_front() {
        for dep in edges.get(id).into_iter().flatten() {
            if reachable.insert(dep) {
                queue.push_back(dep);
            }
        }
    }

    // Stable order and ids: (name, version, source); ids never contain the
    // absolute paths that cargo puts in path-dependency package ids.
    let key = |id: &str| {
        let p = packages.get(id).copied().unwrap_or(&Value::Null);
        (
            p["name"].as_str().unwrap_or_default().to_string(),
            p["version"].as_str().unwrap_or_default().to_string(),
            p["source"].as_str().unwrap_or_default().to_string(),
        )
    };
    let mut ordered: Vec<&str> = reachable.into_iter().collect();
    ordered.sort_by_key(|id| key(id));
    let mut spdx_ids: BTreeMap<&str, String> = BTreeMap::new();
    let mut used = BTreeSet::new();
    for id in &ordered {
        let (name, version, _) = key(id);
        let base = format!(
            "SPDXRef-Package-{}-{}",
            spdx_id_part(&name),
            spdx_id_part(&version)
        );
        let mut candidate = base.clone();
        let mut n = 2;
        while !used.insert(candidate.clone()) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        spdx_ids.insert(id, candidate);
    }

    let spdx_packages: Vec<Value> = ordered
        .iter()
        .map(|id| {
            let p = packages[id];
            let (name, version, _) = key(id);
            json!({
                "SPDXID": spdx_ids[id],
                "name": name,
                "versionInfo": version,
                "downloadLocation": download_location(p),
                "filesAnalyzed": false,
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": license(p),
                "copyrightText": "NOASSERTION",
                "externalRefs": [{
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceType": "purl",
                    "referenceLocator": format!("pkg:cargo/{name}@{version}"),
                }],
            })
        })
        .collect();

    let mut relationships = vec![json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relationshipType": "DESCRIBES",
        "relatedSpdxElement": spdx_ids[root],
    })];
    let mut depends: Vec<(String, String)> = ordered
        .iter()
        .flat_map(|id| {
            edges
                .get(id)
                .into_iter()
                .flatten()
                .map(|dep| (spdx_ids[id].clone(), spdx_ids[dep].clone()))
        })
        .collect();
    depends.sort();
    depends.dedup();
    relationships.extend(depends.into_iter().map(|(from, to)| {
        json!({"spdxElementId": from, "relationshipType": "DEPENDS_ON", "relatedSpdxElement": to})
    }));

    let name = format!("{}-{}", info.binary, info.version);
    let digest = Sha256::digest(
        format!(
            "{}\n{}\n{}",
            info.binary,
            info.version,
            info.commit.unwrap_or_default()
        )
        .as_bytes(),
    );
    let (y, mo, d, h, mi, s) = civil_from_epoch(info.source_date_epoch);
    Ok(json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": name,
        "documentNamespace": format!("https://spdx.org/spdxdocs/{name}-{}", &hex(&digest)[..32]),
        "creationInfo": {
            "created": format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"),
            "creators": [concat!("Tool: naspeed-", env!("CARGO_PKG_VERSION"))],
        },
        "packages": spdx_packages,
        "relationships": relationships,
    }))
}

/// Write the SBOM of `build` to `<out_dir>/<binary>-<version>.spdx.json`;
/// returns the file name.
pub fn write(
    manifest_path: &Path,
    build: &ReleaseBuild,
    info: &SbomInfo,
    out_dir: &Path,
) -> Result<String> {
    let meta = cargo_metadata(manifest_path, build)?;
    let doc = document(&meta, &build.package, info)?;
    let name = file_name(info.binary, info.version);
    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    std::fs::write(out_dir.join(&name), text).with_context(|| format!("write {name}"))?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, source: Option<&str>, license: Option<&str>) -> Value {
        json!({"id": format!("{name}#{version}"), "name": name, "version": version,
               "source": source, "license": license})
    }

    fn dep(id: &str, kind: Option<&str>) -> Value {
        json!({"pkg": id, "dep_kinds": [{"kind": kind, "target": null}]})
    }

    #[test]
    fn reachable_normal_deps_only_and_deterministic() {
        let io = Some("registry+https://github.com/rust-lang/crates.io-index");
        let meta = json!({
            "workspace_members": ["app#1.0.0", "other#1.0.0"],
            "packages": [
                pkg("app", "1.0.0", None, None),
                pkg("other", "1.0.0", None, None),
                pkg("serde", "1.0.0", io, Some("MIT/Apache-2.0")),
                pkg("tempfile", "3.0.0", io, Some("MIT")),
                pkg("unused", "0.1.0", io, Some("MIT")),
            ],
            "resolve": {"nodes": [
                {"id": "app#1.0.0", "deps": [dep("serde#1.0.0", None), dep("tempfile#3.0.0", Some("dev"))]},
                {"id": "other#1.0.0", "deps": [dep("unused#0.1.0", None)]},
                {"id": "serde#1.0.0", "deps": []},
                {"id": "tempfile#3.0.0", "deps": []},
                {"id": "unused#0.1.0", "deps": []},
            ]},
        });
        let info = SbomInfo {
            binary: "app",
            version: "1.0.0",
            commit: Some("abc"),
            source_date_epoch: 951_782_400,
        };
        let doc = document(&meta, "app", &info).unwrap();
        assert_eq!(doc, document(&meta, "app", &info).unwrap());
        assert_eq!(doc["spdxVersion"], "SPDX-2.3");
        assert_eq!(doc["creationInfo"]["created"], "2000-02-29T00:00:00Z");
        let names: Vec<&str> = doc["packages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["app", "serde"]);
        let serde = &doc["packages"][1];
        assert_eq!(serde["licenseDeclared"], "MIT OR Apache-2.0");
        assert_eq!(
            serde["externalRefs"][0]["referenceLocator"],
            "pkg:cargo/serde@1.0.0"
        );
        assert!(
            serde["downloadLocation"]
                .as_str()
                .unwrap()
                .starts_with("https://crates.io/")
        );
        assert_eq!(doc["packages"][0]["licenseDeclared"], "NOASSERTION");
        let rels = doc["relationships"].as_array().unwrap();
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0]["relationshipType"], "DESCRIBES");
        assert_eq!(rels[1]["spdxElementId"], "SPDXRef-Package-app-1.0.0");
        assert_eq!(rels[1]["relatedSpdxElement"], "SPDXRef-Package-serde-1.0.0");
        let other = document(
            &meta,
            "app",
            &SbomInfo {
                commit: Some("def"),
                ..info
            },
        )
        .unwrap();
        assert_ne!(doc["documentNamespace"], other["documentNamespace"]);
        assert!(document(&meta, "missing", &info).is_err());
    }
}
