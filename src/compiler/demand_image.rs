//! Link selected Cargo archives once into a position-independent dyld image.
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustc_middle::ty::TyCtxt;
use rustc_span::Symbol;
use sha2::{Digest, Sha256};

const ENABLE: &str = "NASPEED_LAZY_DEP_IMAGE";
const APP: &str = "NASPEED_LAZY_APP_IMAGE";
const PATCH: &str = "NASPEED_LAZY_APP_PATCH";

pub fn track(sess: &rustc_session::Session) {
    let mut tracked = sess.env_depinfo.borrow_mut();
    tracked.insert((Symbol::intern(ENABLE), value(ENABLE)));
    tracked.insert((Symbol::intern(APP), value(APP)));
    tracked.insert((Symbol::intern(PATCH), value(PATCH)));
    if enabled() || app_enabled() || patch_selectors().is_some() {
        for key in ["SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET"] {
            tracked.insert((Symbol::intern(key), value(key)));
        }
    }
}

fn value(key: &str) -> Option<Symbol> {
    std::env::var(key).ok().map(|v| Symbol::intern(&v))
}

pub fn enabled() -> bool {
    std::env::var_os(ENABLE).is_some_and(|v| v == "1")
}

pub fn app_enabled() -> bool {
    std::env::var_os(APP).is_some_and(|v| v == "1")
}

pub fn patch_selectors() -> Option<String> {
    std::env::var(PATCH).ok()
}

/// A `#[link]` native library resolved to a linkable input. `rustc` accepts
/// these (minus unsupported kinds and modifier flags); lazy run used to
/// reject any crate that declared one.
pub enum NativeInput {
    /// Static archive linked into an image with `-force_load`.
    Archive(PathBuf),
    /// Shared library the image links against; the `-rpath` keeps it
    /// findable after Cargo replaces the original file.
    Dylib(PathBuf),
    /// A framework linked by name (`-framework`).
    Framework(String),
    /// A system or toolchain shared library linked by name (`-l`); the
    /// linker resolves it through its own search paths and `.tbd` files.
    Shared(String),
}

impl NativeInput {
    /// Where the matching `-rpath` entry points, if one is needed.
    fn search_dir(&self) -> Option<&Path> {
        match self {
            Self::Dylib(path) => path.parent(),
            _ => None,
        }
    }
}

fn native_args(cmd: &mut Command, natives: &[NativeInput]) {
    for input in natives {
        match input {
            NativeInput::Archive(path) => {
                cmd.arg("-force_load").arg(path);
            }
            NativeInput::Dylib(path) => {
                cmd.arg(path);
            }
            NativeInput::Framework(name) => {
                cmd.arg("-framework").arg(name);
            }
            NativeInput::Shared(name) => {
                cmd.arg(format!("-l{name}"));
            }
        }
    }
    let mut rpaths: Vec<&Path> = natives
        .iter()
        .filter_map(NativeInput::search_dir)
        .collect();
    rpaths.sort_unstable();
    rpaths.dedup();
    for dir in rpaths {
        cmd.arg("-rpath").arg(dir);
    }
}

pub struct NativeLink {
    linker: PathBuf,
    sdk: PathBuf,
    std: PathBuf,
    version: String,
}

impl NativeLink {
    pub fn new(tcx: TyCtxt<'_>, std: &Path) -> io::Result<Self> {
        let linker = std
            .parent()
            .unwrap()
            .join("../bin/rust-lld")
            .canonicalize()?;
        let sdk = sdk()?;
        let target = tcx.sess.apple_deployment_target();
        let version = sdk_version().unwrap_or_else(|| {
            format!("{}.{}.{}", target.major, target.minor, target.patch)
        });
        Ok(Self {
            linker,
            sdk,
            std: std.to_path_buf(),
            // Linked objects may target a newer macOS than the Rust target's
            // default; rust-lld errors when inputs exceed -platform_version.
            version,
        })
    }

    fn command(&self, install_name: &Path) -> Command {
        let mut cmd = Command::new(&self.linker);
        cmd.args([
            "-flavor",
            "darwin",
            "-dylib",
            "-arch",
            "arm64",
            "-platform_version",
            "macos",
            &self.version,
            &self.version,
            "-syslibroot",
        ])
        .arg(&self.sdk)
        .arg("-install_name")
        .arg(install_name);
        cmd
    }

    fn finish(&self, mut cmd: Command, image: &Path, receipt: &Path) -> io::Result<()> {
        cmd.arg(&self.std)
            .arg("-rpath")
            .arg(self.std.parent().unwrap())
            .arg("-lSystem");
        let result = cmd
            .output()
            .map_err(|e| io::Error::other(format!("spawn {}: {e}", self.linker.display())))?;
        if !result.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&result.stderr).into_owned(),
            ));
        }
        let mut digest = Sha256::new();
        let file = File::open(image)
            .map_err(|e| io::Error::other(format!("open {}: {e}", image.display())))?;
        io::copy(&mut &file, &mut digest)?;
        fs::write(receipt, digest.finalize())
            .map_err(|e| io::Error::other(format!("write {}: {e}", receipt.display())))?;
        // Cargo recursively checks the generation directory from dep-info.
        file.set_modified(std::time::UNIX_EPOCH)?;
        File::open(receipt)
            .map_err(|e| io::Error::other(format!("open {}: {e}", receipt.display())))?
            .set_modified(std::time::UNIX_EPOCH)
    }

    pub fn application(
        &self,
        object: &Path,
        archives: &[PathBuf],
        natives: &[NativeInput],
        stage: &Path,
    ) -> io::Result<()> {
        let image = stage.join("app.dylib");
        let mut cmd = self.command(&image);
        cmd.arg("-o").arg(&image);
        cmd.args(["-dead_strip", "-exported_symbol", "_main"])
            .arg(object)
            .args(archives);
        native_args(&mut cmd, natives);
        self.finish(cmd, &image, &stage.join("app.sha256"))
    }

    pub fn application_patches(
        &self,
        launcher: &Path,
        bitcode: &Path,
        generation: &Path,
        cache: &Path,
        selectors: &str,
        archives: &[PathBuf],
        natives: &[NativeInput],
        source_file: Option<&Path>,
    ) -> io::Result<()> {
        let plan = generation.join("patch-plan");
        let work = generation.join("patch-work");
        fs::create_dir(&work)?;

        let result = (|| {
            let cache = self
                .patch_cache(cache, launcher, archives, natives)
                .map_err(|e| io::Error::other(format!("patch cache: {e}")))?;
            fs::create_dir_all(&cache)?;
            let prepare_start = std::time::Instant::now();
            let mut cmd = Command::new(launcher);
            cmd.arg("--app-patch-prepare")
                .arg(bitcode)
                .arg(&work)
                .arg(&cache)
                .arg(selectors);
            if let Some(source) = source_file {
                cmd.env("NASPEED_NASC_SOURCE", source);
            }
            let output = cmd.output()
                .map_err(|e| io::Error::other(format!("spawn {}: {e}", launcher.display())))?;
            if super::demand_analysis::report() {
                eprintln!(
                    "naspeed app patch prepare: {} ms",
                    prepare_start.elapsed().as_millis()
                );
            }
            if !output.status.success() {
                let detail = String::from_utf8_lossy(&output.stderr);
                return Err(io::Error::other(format!(
                    "patch preparation refused ({}){}{}",
                    output.status,
                    if detail.is_empty() { "" } else { ": " },
                    detail.trim_end()
                )));
            }
            let text = std::str::from_utf8(&output.stdout)
                .map_err(|_| io::Error::other("patch preparation returned non-UTF-8 output"))?;
            let mut lines = text.lines();
            let base_key = lines.next().filter(|key| valid_key(key)).ok_or_else(|| {
                io::Error::other("patch preparation returned an invalid base key")
            })?;
            let count = selectors.split(',').count();
            let mut objects: Vec<PathBuf> = Vec::new();
            let mut patches = Vec::with_capacity(count);
            for line in lines {
                let (kind, rest) = line.split_once(' ').ok_or_else(|| {
                    io::Error::other("patch preparation returned a malformed record")
                })?;
                match kind {
                    "o" => {
                        let path = PathBuf::from(rest);
                        if !path.is_file() {
                            return Err(io::Error::other(format!(
                                "patch preparation returned a missing object: {rest}"
                            )));
                        }
                        objects.push(path);
                    }
                    "p" => {
                        let (slot, key) = rest.split_once(' ').ok_or_else(|| {
                            io::Error::other(
                                "patch preparation returned a malformed patch key",
                            )
                        })?;
                        let expected = patches.len();
                        if !valid_slot(slot, expected) || !valid_key(key) {
                            return Err(io::Error::other(
                                "patch preparation returned an invalid patch slot or key",
                            ));
                        }
                        patches.push((expected, key));
                    }
                    _ => {
                        return Err(io::Error::other(
                            "patch preparation returned an unknown record",
                        ))
                    }
                }
            }
            if patches.len() != count {
                return Err(io::Error::other(
                    "patch preparation returned too few patch keys",
                ));
            }
            if !output.stdout.ends_with(b"\n") {
                return Err(io::Error::other(
                    "patch preparation returned unterminated output",
                ));
            }

            let link_start = std::time::Instant::now();
            let base = self
                .publish_base(base_key, &work, &cache, &objects, archives, natives)
                .map_err(|e| io::Error::other(format!("base {base_key}: {e}")))?;
            let mut images = Vec::with_capacity(patches.len());
            for (slot, key) in patches {
                let image = self.publish_patch(slot, key, &work, &cache, &base)?;
                images.push((slot, image));
            }
            if super::demand_analysis::report() {
                eprintln!(
                    "naspeed app patch link: {} ms",
                    link_start.elapsed().as_millis()
                );
            }
            let mut file = File::options().write(true).create_new(true).open(&plan)?;
            file.write_all(base.as_os_str().as_encoded_bytes())?;
            file.write_all(&[0])?;
            // Pin each slot to its selector name: a plan for different
            // functions at the same indexes is a layout change, not an
            // update the running process may install.
            let names: Vec<&str> = selectors.split(',').collect();
            for (slot, image) in images {
                let name = names.get(slot).copied().unwrap_or("");
                write!(file, "{slot}:{name}")?;
                file.write_all(&[0])?;
                file.write_all(image.as_os_str().as_encoded_bytes())?;
                file.write_all(&[0])?;
            }
            file.set_modified(std::time::UNIX_EPOCH)?;
            // Persist what `ns patch` needs to rebuild a patch without
            // rustc: the sig records, source root, item ledger and the
            // link recipe. Only the generation holds these — work/ is
            // swept on the next build.
            for entry in fs::read_dir(&work).into_iter().flatten().flatten() {
                let name = entry.file_name();
                let keep = name.to_string_lossy().starts_with("nasc-sig-")
                    || name.to_string_lossy() == "items.map"
                    || name.to_string_lossy() == "nasc-source.txt";
                if keep {
                    let dest = generation.join(name);
                    if fs::copy(entry.path(), &dest).is_ok() {
                        // EPOCH keeps the generation invisible to cargo's
                        // dep-info freshness scan — same trick as patch-plan.
                        let _ = File::open(&dest)
                            .and_then(|f| f.set_modified(std::time::UNIX_EPOCH));
                    }
                }
            }
            let env = format!(
                "linker={}\nsdk={}\nversion={}\nstd={}\n",
                self.linker.display(),
                self.sdk.display(),
                self.version,
                self.std.display()
            );
            fs::write(generation.join("link.env"), env)?;
            File::open(generation.join("link.env"))?.set_modified(std::time::UNIX_EPOCH)
        })();
        if std::env::var_os("NASPEED_LAZY_KEEP_WORK").is_none() {
            let _ = fs::remove_dir_all(work);
        }
        if result.is_err() {
            let _ = fs::remove_file(plan);
        }
        result
    }

    fn patch_cache(
        &self,
        cache: &Path,
        launcher: &Path,
        archives: &[PathBuf],
        natives: &[NativeInput],
    ) -> io::Result<PathBuf> {
        let mut digest = Sha256::new();
        digest.update(b"naspeed-app-patch-cache-v1\0");
        digest.update(self.version.as_bytes());
        digest.update(b"\0");
        let libsystem = self.sdk.join("usr/lib/libSystem.tbd");
        for path in [
            self.linker.as_path(),
            libsystem.as_path(),
            self.std.as_path(),
            launcher,
        ] {
            hash_identity(&mut digest, path, true)?;
        }
        for archive in archives {
            hash_identity(&mut digest, archive, false)?;
        }
        for input in natives {
            match input {
                NativeInput::Archive(path) | NativeInput::Dylib(path) => {
                    hash_identity(&mut digest, path, false)?;
                }
                NativeInput::Framework(name) | NativeInput::Shared(name) => {
                    digest.update(b"named\0");
                    digest.update(name.as_bytes());
                    digest.update(b"\0");
                }
            }
        }
        Ok(cache
            .join("app-patch-images-v1")
            .join(format!("{:x}", digest.finalize())))
    }


    fn publish_base(
        &self,
        key: &str,
        generation: &Path,
        cache: &Path,
        objects: &[PathBuf],
        archives: &[PathBuf],
        natives: &[NativeInput],
    ) -> io::Result<PathBuf> {
        let destination = cache.join(key);
        let image = destination.join("app.dylib");
        if cached(&destination) {
            report("base cache hit", &image);
            return Ok(image);
        }
        let exports = generation.join("base.exports");
        let import_file = generation.join("base.imports");
        let imports = symbol_text(&import_file).map_err(|e| {
            io::Error::other(format!("read {}: {e}", import_file.display()))
        })?;
        let mut cmd = self.command(&image);
        cmd.args(["-dead_strip", "-exported_symbols_list"])
            .arg(&exports);
        for symbol in imports.lines() {
            cmd.arg("-u").arg(symbol);
        }
        cmd.args(objects).args(archives);
        native_args(&mut cmd, natives);
        self.publish(cmd, generation, &destination)?;
        report("base built", &image);
        Ok(image)
    }

    fn publish_patch(
        &self,
        slot: usize,
        key: &str,
        generation: &Path,
        cache: &Path,
        base: &Path,
    ) -> io::Result<PathBuf> {
        let destination = cache.join(key);
        let image = destination.join("app.dylib");
        if cached(&destination) {
            report("patch cache hit", &image);
            return Ok(image);
        }
        let mut cmd = self.command(&image);
        cmd.arg("-dead_strip")
            .arg("-exported_symbol")
            .arg(format!("___ns_patch_impl_{slot}"))
            .arg(generation.join(format!("{key}.o")))
            .arg("-needed_library")
            .arg(base);
        self.publish(cmd, generation, &destination)?;
        report("patch built", &image);
        Ok(image)
    }

    fn publish(&self, mut cmd: Command, generation: &Path, destination: &Path) -> io::Result<()> {
        if destination.exists() {
            return Err(io::Error::other(format!(
                "invalid image cache entry {}",
                destination.display()
            )));
        }
        let key = destination.file_name().unwrap().to_str().unwrap();
        let stage = generation.join(format!("publish-{key}"));
        fs::create_dir(&stage)
            .map_err(|e| io::Error::other(format!("mkdir {}: {e}", stage.display())))?;
        let image = stage.join("app.dylib");
        cmd.arg("-o").arg(&image);
        let result = self.finish(cmd, &image, &stage.join("app.sha256"));
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        match fs::rename(&stage, destination) {
            Ok(()) => Ok(()),
            Err(_) if cached(destination) => {
                let _ = fs::remove_dir_all(stage);
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&stage);
                Err(io::Error::other(format!(
                    "rename {} -> {}: {error}",
                    stage.display(),
                    destination.display()
                )))
            }
        }
    }
}

fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn valid_slot(slot: &str, expected: usize) -> bool {
    slot.parse::<usize>() == Ok(expected) && (slot == "0" || !slot.starts_with('0'))
}

fn cached(directory: &Path) -> bool {
    // The launcher checks the receipt against the actual bytes before
    // mapping. Don't hash an unchanged base both here and at launch.
    directory.join("app.dylib").is_file() && directory.join("app.sha256").is_file()
}

fn report(action: &str, image: &Path) {
    if super::demand_analysis::report() {
        eprintln!("naspeed app patch {action}: {}", image.display());
    }
}

fn symbol_text(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(io::Error::other(format!(
            "{} is not newline terminated",
            path.display()
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::other(format!("{} is not UTF-8", path.display())))?;
    if text
        .lines()
        .any(|symbol| !symbol.starts_with('_') || symbol.chars().any(char::is_whitespace))
    {
        return Err(io::Error::other(format!(
            "{} contains an invalid Mach-O symbol",
            path.display()
        )));
    }
    Ok(text)
}

fn hash_identity(digest: &mut Sha256, path: &Path, include_path: bool) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if include_path {
        let bytes = path.as_os_str().as_encoded_bytes();
        digest.update(bytes.len().to_le_bytes());
        digest.update(bytes);
    }
    for value in [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
    ] {
        digest.update(value.to_le_bytes());
    }
    Ok(())
}

fn sdk() -> io::Result<PathBuf> {
    match std::env::var_os("SDKROOT").filter(|p| !p.is_empty() && p != "/") {
        Some(path) => PathBuf::from(path).canonicalize(),
        None => {
            let out = Command::new("/usr/bin/xcrun")
                .args(["--sdk", "macosx", "--show-sdk-path"])
                .output()?;
            if !out.status.success() {
                return Err(io::Error::other("xcrun could not resolve the macOS SDK"));
            }
            Path::new(
                std::str::from_utf8(&out.stdout)
                    .map_err(io::Error::other)?
                    .trim(),
            )
            .canonicalize()
        }
    }
}


fn sdk_version() -> Option<String> {
    let out = Command::new("/usr/bin/xcrun")
        .args(["--sdk", "macosx", "--show-sdk-version"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.trim().split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    Some(format!("{major}.{minor}.0"))
}
pub fn prepare(
    tcx: TyCtxt<'_>,
    cache: &Path,
    stage: &Path,
    std: &Path,
    sources: &[PathBuf],
    snapshots: &[PathBuf],
    natives: &[NativeInput],
) -> io::Result<PathBuf> {
    let link = NativeLink::new(tcx, std)?;
    let mut hash = DefaultHasher::new();
    link.version.hash(&mut hash);
    for path in [&link.linker, &link.sdk.join("usr/lib/libSystem.tbd"), std]
        .into_iter()
        .chain(sources.iter().map(PathBuf::as_path))
    {
        // Cargo publishes archives by rename. Hardlinking a snapshot only
        // alters ctime; the inode, mtime and size still identify its bytes.
        path.hash(&mut hash);
        let m = fs::metadata(path)?;
        (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()).hash(&mut hash);
    }
    for input in natives {
        match input {
            NativeInput::Archive(path) | NativeInput::Dylib(path) => {
                path.hash(&mut hash);
                let m = fs::metadata(path)?;
                (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec())
                    .hash(&mut hash);
            }
            NativeInput::Framework(name) | NativeInput::Shared(name) => {
                0xffu8.hash(&mut hash);
                name.hash(&mut hash);
            }
        }
    }
    let root = cache.join("dep-images-v1");
    let dest = root.join(format!("{:016x}", hash.finish()));
    let image = dest.join("image.dylib");
    if image.is_file() && dest.join("sha256").is_file() {
        if super::demand_analysis::report() {
            eprintln!("naspeed dependency image hit: {}", image.display());
        }
        return Ok(image);
    }
    fs::create_dir_all(&root)?;
    let stage = stage.join("dep-image-stage");
    fs::create_dir(&stage)?;
    let output = stage.join("image.dylib");
    let result = (|| {
        let mut cmd = link.command(&image);
        cmd.arg("-o").arg(&output);
        for archive in snapshots {
            cmd.arg("-force_load").arg(archive);
        }
        native_args(&mut cmd, natives);
        link.finish(cmd, &output, &stage.join("sha256"))?;
        fs::rename(&stage, &dest).or_else(|err| {
            if image.is_file() && dest.join("sha256").is_file() {
                Ok(())
            } else {
                Err(err)
            }
        })?;
        if super::demand_analysis::report() {
            eprintln!("naspeed dependency image built: {}", image.display());
        }
        Ok(image)
    })();
    let _ = fs::remove_dir_all(stage);
    result
}
