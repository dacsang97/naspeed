//! Darwin dev runs link the application against an immutable Rust dependency image.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rustc_interface::interface::Config;
use rustc_session::config::{
    CrateType, ExternEntry, ExternLocation, Externs, OptLevel, host_tuple,
};
use rustc_session::filesearch::make_target_lib_path;
use rustc_session::search_paths::PathKind;
use rustc_session::utils::CanonicalizedPath;
use rustc_span::Symbol;

use super::{NATIVE_RUSTC, SYSROOT};

const NAME: &str = "__naspeed_run_dependencies";
const LINK_ENV: &[&str] = &[
    "PATH",
    "SDKROOT",
    "DEVELOPER_DIR",
    "MACOSX_DEPLOYMENT_TARGET",
    "LIBRARY_PATH",
];

pub fn candidate(args: &[String]) -> bool {
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg.starts_with("-Z") {
            return false;
        }
        if let Some(option) = arg.strip_prefix("-C") {
            let option = if option.is_empty() {
                args.next().unwrap_or("")
            } else {
                option
            };
            let name = option.split('=').next().unwrap_or("");
            if !matches!(
                name,
                "metadata"
                    | "extra-filename"
                    | "incremental"
                    | "embed-bitcode"
                    | "debuginfo"
                    | "split-debuginfo"
                    | "debug-assertions"
                    | "overflow-checks"
                    | "codegen-units"
                    | "strip"
                    | "opt-level"
            ) {
                return false;
            }
        }
    }
    true
}

pub fn configure(config: &mut Config) {
    let opts = &config.opts;
    if !cfg!(target_os = "macos")
        || opts.target_triple.tuple() != host_tuple()
        || opts.crate_types != [CrateType::Executable]
        || opts.crate_name.as_deref() == Some(NAME)
        || opts.externs.get(NAME).is_some()
        || opts.optimize != OptLevel::No
        || opts.test
        || !opts.libs.is_empty()
        || opts
            .search_paths
            .iter()
            .any(|s| !matches!(s.kind, PathKind::Dependency | PathKind::Native))
    {
        return;
    }
    match bundle(config) {
        Ok(Some((image, reused))) => {
            let opts = &mut config.opts;
            let mut externs: BTreeMap<_, _> = opts
                .externs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            externs.insert(
                NAME.into(),
                ExternEntry {
                    location: ExternLocation::ExactPaths(BTreeSet::from([CanonicalizedPath::new(
                        image.clone(),
                    )])),
                    is_private_dep: true,
                    add_prelude: false,
                    nounused_dep: true,
                    force: true,
                },
            );
            opts.externs = Externs::new(externs);
            let libstd_dir = make_target_lib_path(Path::new(SYSROOT), host_tuple());
            for dir in [image.parent().unwrap(), &libstd_dir] {
                opts.cg.link_args.extend(rpath(dir));
            }
            if std::env::var_os("NASPEED_SPLIT_LINK_REPORT").as_deref() == Some("1".as_ref()) {
                eprintln!(
                    "naspeed split-link: {} {}",
                    if reused { "reused" } else { "built" },
                    image.display()
                );
            }
            config.track_state = Some(Box::new(move |sess| {
                sess.file_depinfo
                    .borrow_mut()
                    .insert(Symbol::intern(image.to_str().unwrap()));
                for key in LINK_ENV {
                    let val = std::env::var(key).ok().map(|v| Symbol::intern(&v));
                    sess.env_depinfo
                        .borrow_mut()
                        .insert((Symbol::intern(key), val));
                }
            }));
        }
        Ok(None) => {}
        Err(err) => eprintln!("naspeed split-link: using native link ({err:?})"),
    }
}

fn rpath(dir: &Path) -> [String; 4] {
    [
        "-Xlinker".into(),
        "-rpath".into(),
        "-Xlinker".into(),
        dir.to_str().unwrap().into(),
    ]
}

fn bundle(config: &Config) -> io::Result<Option<(PathBuf, bool)>> {
    let Some(out) = &config.output_dir else {
        return Ok(None);
    };
    let out = out.canonicalize().map_err(|e| {
        io::Error::other(format!("out dir {}: {e}", out.display()))
    })?;
    let mut externs = Vec::new();
    for (alias, entry) in config.opts.externs.iter() {
        let Some(mut paths) = entry.files() else {
            return Ok(None);
        };
        let Some(path) = paths.next() else {
            return Ok(None);
        };
        if paths.next().is_some() || entry.force || !entry.add_prelude {
            return Ok(None);
        }
        let path = path.canonicalized();
        if path.to_str().is_none() {
            return Ok(None);
        }
        if path.extension().is_some_and(|ext| ext == "rlib") {
            if !path.is_file() {
                // Cargo handed us an extern that no longer exists (a
                // concurrent sweep); fall back rather than cache a lie.
                return Ok(None);
            }
            externs.push((alias, path));
        } else if path.extension().is_none_or(|ext| ext != "dylib") {
            return Ok(None);
        }
    }
    if externs.is_empty() || out.to_str().is_none() {
        return Ok(None);
    }

    let mut hash = DefaultHasher::new();
    "naspeed-split-link-v3".hash(&mut hash);
    NATIVE_RUSTC.hash(&mut hash);
    stamp(Path::new(NATIVE_RUSTC), &mut hash)?;
    stamp(&std::env::current_exe()?, &mut hash)?;
    std::env::current_dir()?.hash(&mut hash);
    for key in LINK_ENV {
        let value = std::env::var_os(key);
        if *key == "PATH" {
            // Equal search semantics: dedupe identical entries. A removed
            // entry could change which tools a link finds, so PATH still
            // hashes in — but one added twice no longer does.
            let mut parts: Vec<_> = std::env::split_paths(&value.unwrap_or_default()).collect();
            parts.dedup();
            parts.hash(&mut hash);
        } else {
            value.hash(&mut hash);
        }
    }
    // The lookup key covers the direct externs only: rustc records every
    // file it actually reads into the image's dep-info, and that manifest
    // validates each hit. An rlib rewritten with identical bytes (Cargo
    // rebuild, naspeed clone-restore) keeps the image; unrelated artifacts
    // under -L no longer churn it — both did before this version.
    let mut memo = DigestMemo::new(&out);
    for (name, path) in &externs {
        name.hash(&mut hash);
        path.hash(&mut hash);
        memo.digest(path)?.hash(&mut hash);
    }
    memo.save()?;

    let cache = out.join(".naspeed-split");
    let filename = format!("lib{NAME}-{:016x}", hash.finish());
    let image = cache.join(format!("{filename}.dylib"));
    let manifest = cache.join(format!("{filename}.files"));
    if image.is_file() && manifest.is_file() && inputs_fresh(&manifest)? {
        // Older image generations stay for binaries still linked against
        // stale units.
        return Ok(Some((image, true)));
    }

    let mut search = Vec::with_capacity(config.opts.search_paths.len());
    for path in &config.opts.search_paths {
        // subdirectories only some builds create (boring-sys2's
        // out/build/crypto/ exists on Windows only). rustc accepts missing
        // -L dirs; a canonicalize failure must not fall back to the slow
        // link.
        let Ok(dir) = path.dir.canonicalize() else {
            continue;
        };
        let Some(text) = dir.to_str() else {
            return Ok(None);
        };
        let kind = if path.kind == PathKind::Dependency {
            "dependency"
        } else {
            "native"
        };
        search.push(format!("{kind}={text}"));
    }

    fs::create_dir_all(&cache)?;
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stage = cache.join(format!("stage-{}-{time}", std::process::id()));
    fs::create_dir(&stage)?;
    let result = (|| {
        use std::fmt::Write as _;
        let mut source = String::new();
        let libstd_dir = make_target_lib_path(Path::new(SYSROOT), host_tuple());
        let mut cmd = Command::new(NATIVE_RUSTC);
        let output = stage.join(format!("{filename}.dylib"));
        let dep = stage.join(format!("{filename}.d"));
        cmd.args([
            "--edition=2024",
            "--crate-type=dylib",
            "--crate-name",
            NAME,
            "-Cprefer-dynamic",
            "-Csplit-debuginfo=off",
            "-Crpath",
            "-o",
        ])
        .arg(&output);
        cmd.arg(format!(
            "--emit=link={},dep-info={}",
            output.display(),
            dep.display()
        ));
        for flag in rpath(&libstd_dir) {
            cmd.arg(format!("-Clink-arg={flag}"));
        }
        for (alias, path) in externs {
            writeln!(source, "extern crate r#{alias} as _;").unwrap();
            cmd.arg("--extern")
                .arg(format!("{alias}={}", path.display()));
        }
        for path in &search {
            cmd.arg("-L").arg(path);
        }
        let input = stage.join("bundle.rs");
        fs::write(&input, source)?;
        let result = cmd
            .arg(&input)
            .output()
            .map_err(|e| io::Error::other(format!("spawn {}: {e}", NATIVE_RUSTC)))?;
        if !result.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&result.stderr).into_owned(),
            ));
        }
        // The manifest is a stat snapshot of every file rustc reported
        // reading. A hit re-stats them; a vanished or touched input misses.
        let mut text = String::new();
        if let Ok(depinfo) = fs::read_to_string(&dep) {
            for file in depinfo.split([' ', '\\', '\n']) {
                if file.is_empty() || !file.starts_with('/') || file.ends_with(".dylib") {
                    continue;
                }
                // Inputs that lived only in the stage dir (the generated
                // bundle.rs, response files) vanish with it; the extern key
                // already covers what they encode.
                if Path::new(file).starts_with(&stage) {
                    continue;
                }
                if let Ok(meta) = fs::metadata(file) {
                    let _ = writeln!(
                        text,
                        "{}\t{}\t{}\t{}",
                        file,
                        meta.len(),
                        meta.mtime(),
                        meta.mtime_nsec()
                    );
                }
            }
        }
        // dep-info never lists the native archives rustc found through -L;
        // snapshot them so their rebuild invalidates the image. Rust
        // artifacts live behind names that already encode content.
        for path in &search {
            if !path.starts_with("native=") {
                continue;
            }
            let dir = Path::new(path.trim_start_matches("native="));
            if let Ok(read) = fs::read_dir(dir) {
                let mut natives: Vec<_> = read
                    .flatten()
                    .map(|e| e.path())
                    .filter(|f| {
                        f.extension()
                            .is_some_and(|ext| ext == "a" || ext == "dylib" || ext == "so")
                    })
                    .collect();
                natives.sort_unstable();
                for file in natives {
                    if let Ok(meta) = fs::metadata(&file) {
                        let _ = writeln!(
                            text,
                            "{}\t{}\t{}\t{}",
                            file.display(),
                            meta.len(),
                            meta.mtime(),
                            meta.mtime_nsec()
                        );
                    }
                }
            }
        }
        let stage_inputs = stage.join("files");
        fs::write(&stage_inputs, text)?;
        // Cargo compares dep-info mtimes to when the compilation started.
        // The image is immutable under its input key; track its existence,
        // not the wall clock time at which this compiler produced it.
        fs::File::open(&output)?.set_modified(SystemTime::UNIX_EPOCH)?;
        fs::rename(&output, &image)?;
        fs::rename(&stage_inputs, &manifest)?;
        // Older generations stay for still-linked binaries; the target
        // sweep reclaims them later.
        Ok(())
    })();
    let _ = fs::remove_dir_all(stage);
    result?;
    Ok(Some((image, false)))
}

/// Every recorded input must still exist with the same size and mtime.
/// A hit is one stat per file; content hashes live in the extern key, not
/// in this manifest.
fn inputs_fresh(manifest: &Path) -> io::Result<bool> {
    let text = fs::read_to_string(manifest)?;
    for line in text.lines() {
        let mut fields = line.rsplitn(4, '\t');
        let (Some(mn), Some(ms), Some(size), Some(path)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(size), Ok(ms), Ok(mn)) = (
            size.parse::<u64>(),
            ms.parse::<i64>(),
            mn.parse::<i64>(),
        ) else {
            return Ok(false);
        };
        match fs::metadata(path) {
            Ok(meta)
                if meta.len() == size && meta.mtime() == ms && meta.mtime_nsec() == mn => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}



fn stamp(path: &Path, hash: &mut DefaultHasher) -> io::Result<()> {
    let m = fs::metadata(path)
        .map_err(|e| io::Error::other(format!("stamp {}: {e}", path.display())))?;
    path.hash(hash);
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
        .hash(hash);
    Ok(())
}

/// Stat-keyed content digests for link inputs, persisted per output dir.
/// A fresh stat (size+mtime) reuses the recorded digest, so a rustc pass
/// over hundreds of archives costs stat() calls, not reads. A changed or
/// absent file produces a fresh digest or the `missing` sentinel.
struct DigestMemo {
    path: PathBuf,
    map: BTreeMap<(PathBuf, u64, i64, i64), [u8; 32]>,
    dirty: bool,
}

impl DigestMemo {
    fn new(out: &Path) -> Self {
        let dir = out.join(".naspeed-split");
        let path = dir.join("digests");
        let mut map = BTreeMap::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                // path \t size \t mtime_secs \t mtime_nanos \t hex
                let mut fields = line.splitn(5, '\t');
                let (Some(p), Some(s), Some(ms), Some(mn), Some(h)) =
                    (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let (Ok(size), Ok(mss), Ok(mnn), Ok(bytes)) = (
                    s.parse::<u64>(),
                    ms.parse::<i64>(),
                    mn.parse::<i64>(),
                    hex_to_bytes(h),
                ) else {
                    continue;
                };
                map.insert((PathBuf::from(p), size, mss, mnn), bytes);
            }
        }
        Self {
            path,
            map,
            dirty: false,
        }
    }

    /// Content digest of `path` as hex; `"missing"` when it cannot be
    /// stat'ed at all so an absent input changes the key instead of
    /// colliding with a real digest.
    fn digest(&mut self, path: &Path) -> io::Result<String> {
        use sha2::{Digest, Sha256};
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(_) => return Ok("missing".to_string()),
        };
        let key = (
            path.to_path_buf(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
        );
        if let Some(digest) = self.map.get(&key) {
            return Ok(hex(digest));
        }
        let mut file = File::open(path)
            .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)?;
        let bytes: [u8; 32] = hasher.finalize().into();
        self.map.insert(key, bytes);
        self.dirty = true;
        Ok(hex(&bytes))
    }

    /// Write through a lock so concurrent drivers on one target dir share
    /// entries instead of clobbering each other. Best effort: a lost write
    /// only re-hashes next time.
    fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(dir) = self.path.parent() else {
            return Ok(());
        };
        fs::create_dir_all(dir)?;
        let lock_path = dir.join("digests.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)?;
        lock.lock()?;
        // Re-read under the lock and merge entries other drivers added.
        if let Ok(text) = fs::read_to_string(&self.path) {
            for line in text.lines() {
                let mut fields = line.splitn(5, '\t');
                let (Some(p), Some(s), Some(ms), Some(mn), Some(h)) =
                    (fields.next(), fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                if let (Ok(size), Ok(mss), Ok(mnn), Ok(bytes)) = (
                    s.parse::<u64>(),
                    ms.parse::<i64>(),
                    mn.parse::<i64>(),
                    hex_to_bytes(h),
                ) {
                    self.map
                        .entry((PathBuf::from(p), size, mss, mnn))
                        .or_insert(bytes);
                }
            }
        }
        let tmp = dir.join(format!("digests.{}.tmp", std::process::id()));
        {
            let mut out = io::BufWriter::new(File::create(&tmp)?);
            for ((path, size, ms, mn), digest) in &self.map {
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}",
                    path.display(),
                    size,
                    ms,
                    mn,
                    hex(digest)
                )?;
            }
            out.flush()?;
        }
        fs::rename(&tmp, &self.path)?;
        drop(lock);
        self.dirty = false;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

fn hex_to_bytes(s: &str) -> io::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    let s = s.trim();
    if s.len() != 64 {
        return Err(io::Error::other("bad digest length"));
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    Ok(out)
}
