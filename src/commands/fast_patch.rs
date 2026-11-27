//! `ns patch`: rebuild an app patch without rustc. A selector body that
//! parses under `nasc` and keeps its signature compiles straight to a new
//! patch dylib and a staged live plan, so a running process flips its
//! dispatch table in well under a second. Any drift outside the selector
//! bodies — signature changes, added or removed functions, unsupported
//! bodies — refuses; the caller then pays for `ns build --lazy-ir` once.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

#[derive(Debug, Args)]
pub struct PatchArgs {
    /// Cargo bin target name to repatch (e.g. `native-rpc`).
    #[arg(long)]
    pub bin: Option<String>,
    /// Cargo target directory (defaults to `target/debug/deps` discovery).
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
}

struct Plan {
    base: String,
    slots: Vec<(String, String)>, // (selector leaf, image path)
    bytes: Vec<u8>,
}

fn parse_plan(bytes: &[u8]) -> Result<Plan> {
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    if fields.len() < 3 || fields.len() % 2 != 0 || !fields.last().is_some_and(|f| f.is_empty()) {
        bail!("invalid patch plan");
    }
    let base = String::from_utf8_lossy(fields[0]).into_owned();
    if !Path::new(&base).is_absolute() {
        bail!("patch plan base is not absolute");
    }
    let mut slots = Vec::new();
    let mut i = 1;
    while i + 1 < fields.len() {
        let token = String::from_utf8_lossy(fields[i]).into_owned();
        let image = String::from_utf8_lossy(fields[i + 1]).into_owned();
        let (index, name) = token
            .split_once(':')
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .unwrap_or((token.clone(), String::new()));
        if index.parse::<usize>().ok() != Some(slots.len()) || !Path::new(&image).is_absolute() {
            bail!("malformed patch plan slot {index}");
        }
        slots.push((name, image));
        i += 2;
    }
    Ok(Plan {
        base,
        slots,
        bytes: bytes.to_vec(),
    })
}

/// One row of items.map: qualified name, param list, return type, body hash.
struct Item {
    name: String,
    params: String,
    ret: String,
    hash: String,
}

fn items_map(text: &str) -> BTreeMap<String, Vec<Item>> {
    let mut by_leaf: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(4, '\t');
        let (Some(name), Some(params), Some(ret), Some(hash)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let leaf = name.rsplit("::").next().unwrap_or(name).to_string();
        by_leaf.entry(leaf).or_default().push(Item {
            name: name.to_string(),
            params: params.to_string(),
            ret: ret.to_string(),
            hash: hash.to_string(),
        });
    }
    by_leaf
}

fn sha256_hex(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut d = Sha256::new();
    d.update(&bytes);
    Ok(d.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn link_env(generation: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(generation.join("link.env"))
        .context("no link.env; run `ns build --lazy-ir` first")?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect())
}

fn find_tools() -> Result<(PathBuf, PathBuf)> {
    // `nasc` and `naspeed-lazy-launch` live next to the naspeed binary when
    // installed via `make lazy-driver` or dev-install.
    let exe = std::env::current_exe()?.canonicalize().unwrap_or_else(|_| std::env::current_exe().unwrap());
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = exe.parent() {
        dirs.push(parent.to_path_buf());
        dirs.push(parent.join("function-tools"));
        // Dev builds: target/debug/naspeed -> target/function-tools/.
        if let Some(grandparent) = parent.parent() {
            dirs.push(grandparent.join("function-tools"));
        }
    }
    for dir in dirs {
        let nasc = dir.join("nasc");
        let launch = dir.join("naspeed-lazy-launch");
        if nasc.is_file() && launch.is_file() {
            return Ok((nasc, launch));
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    for dir in [
        home.join(".local/bin/function-tools"),
    ] {
        let nasc = dir.join("nasc");
        let launch = dir.join("naspeed-lazy-launch");
        if nasc.is_file() && launch.is_file() {
            return Ok((nasc, launch));
        }
    }
    bail!("nasc + naspeed-lazy-launch not found; run `make lazy-driver`")
}

/// `ns patch`: recompile only the patched function bodies against the last
/// published plan — the no-rustc fast path over naspeed's patch scheme.
pub fn run(args: &PatchArgs) -> Result<u8> {
    let deps = args
        .target_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("target/debug/deps"));
    let lazy = deps.join(".naspeed-lazy");
    let live = lazy.join("live");
    if !live.is_dir() {
        bail!("no .naspeed-lazy/live under {}; run `ns build --lazy-ir` first", lazy.display());
    }

    // Find the live entry for the requested binary and the generation whose
    // patch-plan currently drives it.
    let mut chosen: Option<(PathBuf, Plan)> = None;
    for entry in fs::read_dir(&live)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if let Some(bin) = &args.bin {
            let stem = bin.replace('-', "_");
            if !name.starts_with(&format!("{stem}-")) && name != stem {
                continue;
            }
        }
        let bytes = fs::read(&path)?;
        let plan = parse_plan(&bytes).with_context(|| format!("plan {name}"))?;
        if chosen.is_some() {
            bail!("multiple live patch plans; pass --bin");
        }
        chosen = Some((path, plan));
    }
    let Some((live_path, plan)) = chosen else {
        bail!("no live patch plan found; run `ns build --lazy-ir` first");
    };

    // Locate the owning generation by matching plan bytes.
    let mut generation = None;
    for entry in fs::read_dir(&lazy)?.flatten() {
        let dir = entry.path();
        let path = dir.join("patch-plan");
        if path.is_file() && fs::read(&path).ok().as_deref() == Some(plan.bytes.as_slice()) {
            generation = Some(dir);
            break;
        }
    }
    let Some(generation) = generation else {
        bail!("live plan has no matching generation; run `ns build --lazy-ir` again");
    };
    let (nasc, launch) = find_tools()?;
    let env = link_env(&generation)?;

    // Fast-path guards: every non-selector body must hash identically to
    // the build-time ledger; selector bodies may change but not their
    // signature. Drift → refuse, the caller pays for the full build.
    let baseline = fs::read_to_string(generation.join("items.map"))
        .context("no items.map; run `ns build --lazy-ir` first")?;
    let source_dir = fs::read_to_string(generation.join("nasc-source.txt"))
        .context("no nasc-source.txt; run `ns build --lazy-ir` first")?;
    let source_root = Path::new(source_dir.trim())
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .join("src");
    let out = Command::new(&nasc)
        .arg("map")
        .arg(&source_root)
        .arg(&generation)
        .output()
        .context("spawn nasc map")?;
    if !out.status.success() {
        bail!("nasc map failed");
    }
    let baseline = items_map(&baseline);
    let current = items_map(&String::from_utf8_lossy(&out.stdout));

    let leaves: Vec<&str> = plan.slots.iter().map(|(n, _)| n.as_str()).collect();
    for (leaf, cur) in &current {
        let is_selector = leaves.contains(&leaf.as_str());
        let old = baseline.get(leaf);
        match old {
            None if !is_selector => bail!("new function {leaf}; run `ns build --lazy-ir`"),
            None => continue,
            Some(prev) => {
                // A same-leaf set (overloads/siblings) compares by qualified name.
                for now in cur {
                    let was = prev.iter().find(|p| p.name == now.name);
                    match was {
                        None if !is_selector => {
                            bail!("new function {}; run `ns build --lazy-ir`", now.name)
                        }
                        None => continue,
                        Some(prev) => {
                            if now.params != prev.params || now.ret != prev.ret {
                                if is_selector {
                                    bail!(
                                        "signature changed for {}; run `ns build --lazy-ir`",
                                        now.name
                                    );
                                }
                            } else if now.hash != prev.hash && !is_selector {
                                bail!(
                                    "non-selector body changed ({}); run `ns build --lazy-ir`",
                                    now.name
                                );
                            }
                        }
                    }
                }
                for prev in prev {
                    if !cur.iter().any(|n| n.name == prev.name) && !is_selector {
                        bail!("removed function {}; run `ns build --lazy-ir`", prev.name);
                    }
                }
            }
        }
    }
    for leaf in baseline.keys() {
        if !current.contains_key(leaf) && !leaves.contains(&leaf.as_str()) {
            bail!("removed function {leaf}; run `ns build --lazy-ir`");
        }
    }

    // Recompile selector bodies with nasc against the persisted sigs.
    let work = generation.join("patch-work");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir(&work)?;
    for entry in fs::read_dir(&generation)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("nasc-sig-") || name == "nasc-source.txt" {
            fs::copy(entry.path(), work.join(&name))?;
        }
    }
    let status = Command::new(&nasc)
        .arg("batch")
        .arg(&work)
        .arg(&source_root)
        .status()
        .context("spawn nasc batch")?;
    if !status.success() {
        bail!("nasc batch failed; run `ns build --lazy-ir`");
    }

    // impl-i.ll → .o → dylib, reusing unchanged patch images by content key.
    let scope = Path::new(&plan.slots[0].1)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("patch image scope missing")?;
    let mut new_images: Vec<String> = Vec::with_capacity(plan.slots.len());
    for (i, (name, image)) in plan.slots.iter().enumerate() {
        let impl_ll = work.join(format!("impl-{i}.ll"));
        let changed = current
            .get(name)
            .and_then(|items| {
                baseline.get(name).and_then(|prev| {
                    items
                        .iter()
                        .zip(prev.iter())
                        .next()
                        .map(|(a, b)| a.hash != b.hash)
                })
            })
            .unwrap_or(false);
        if !impl_ll.is_file() && changed {
            bail!(
                "body for `{name}` is not compilable by nasc; run `ns build --lazy-ir`"
            );
        }
        let image = if impl_ll.is_file() {
            let obj = work.join(format!("impl-{i}.o"));
            let status = Command::new(&launch)
                .arg("--emit-obj")
                .arg(&impl_ll)
                .arg(&obj)
                .status()
                .context("spawn emit-obj")?;
            if !status.success() {
                bail!("emit-obj failed for slot {i}; run `ns build --lazy-ir`");
            }
            let key = sha256_hex(&obj)?;
            let dest = scope.join(&key).join("app.dylib");
            if !dest.is_file() {
                let stage = work.join(format!("publish-{key}"));
                fs::create_dir(&stage)?;
                let image = stage.join("app.dylib");
                let status = Command::new(env.get("linker").unwrap())
                    .args([
                        "-flavor", "darwin", "-dylib", "-arch", "arm64",
                        "-platform_version", "macos",
                    ])
                    .arg(env.get("version").unwrap())
                    .arg(env.get("version").unwrap())
                    .arg("-syslibroot")
                    .arg(env.get("sdk").unwrap())
                    .arg("-install_name")
                    .arg(&image)
                    .arg("-dead_strip")
                    .arg("-exported_symbol")
                    .arg(format!("___ns_patch_impl_{i}"))
                    .arg(&obj)
                    .arg("-needed_library")
                    .arg(&plan.base)
                    .arg(env.get("std").unwrap())
                    .arg("-rpath")
                    .arg(Path::new(env.get("std").unwrap()).parent().unwrap())
                    .arg("-lSystem")
                    .arg("-o")
                    .arg(&image)
                    .status()
                    .context("spawn linker")?;
                if !status.success() {
                    bail!("link failed for slot {i}");
                }
                fs::create_dir(scope.join(&key))?;
                fs::rename(&stage, scope.join(&key)).or_else(|_| {
                    if dest.is_file() {
                        let _ = fs::remove_dir_all(&stage);
                        Ok(())
                    } else {
                        Err(std::io::Error::other("publish rename failed"))
                    }
                })?;
            }
            dest.to_string_lossy().into_owned()
        } else {
            image.clone()
        };
        new_images.push(image);
    }

    // Rewrite this generation's plan and stage it live atomically. Cold
    // start reads patch-plan; the running process reads live/<name> — both
    // must agree, and both must list the selector names.
    let mut plan_bytes = plan.base.clone().into_bytes();
    plan_bytes.push(0);
    for (i, image) in new_images.iter().enumerate() {
        plan_bytes.extend(format!("{i}:{}", plan.slots[i].0).as_bytes());
        plan_bytes.push(0);
        plan_bytes.extend(image.as_bytes());
        plan_bytes.push(0);
    }
    let stage = lazy.join(format!("patch-plan-{}", std::process::id()));
    fs::write(&stage, &plan_bytes)?;
    File::open(&stage)?.set_modified(std::time::UNIX_EPOCH)?;
    fs::rename(&stage, generation.join("patch-plan"))?;
    let live_stage = live.join(format!(".pending-{}", std::process::id()));
    fs::write(&live_stage, &plan_bytes)?;
    fs::File::open(&live_stage)?.set_modified(std::time::UNIX_EPOCH)?;
    fs::rename(&live_stage, &live_path)?;
    let _ = fs::remove_dir_all(&work);
    println!("ns patch: {} slot(s) repatched", plan.slots.len());
    Ok(0)
}

use std::fs::File;
