//! Replay only Cargo's selected compiler action, never re-resolve dependencies.
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::hash::Hash;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::stable_hash::{StableHash, StableHasher};
use rustc_middle::ty::TyCtxt;
use rustc_span::FileName;

const VERSION: &[u8] = b"naspeed-cargo-demand-v2";

pub fn write(path: &Path, tcx: TyCtxt<'_>, argv: &[String]) -> io::Result<()> {
    let cwd = std::env::current_dir()?;
    let generation = path.parent().unwrap();
    let mut inputs = BTreeSet::new();
    for file in tcx.sess.source_map().files().iter() {
        if !file.is_imported()
            && let FileName::Real(file) = &file.name
        {
            let file = file
                .local_path()
                .ok_or_else(|| invalid("remapped source cannot be replayed"))?;
            inputs.insert(cwd.join(file));
        }
    }
    for name in tcx.sess.file_depinfo.borrow().iter() {
        let file = cwd.join(name.as_str());
        if !file.starts_with(generation) {
            inputs.insert(file);
        }
    }
    // Cargo already fingerprints all selected dependency units. Also stamp the
    // precise compiler files for a miss racing a second Cargo invocation.
    let mut namespace = StableHasher::new();
    VERSION.hash(&mut namespace);
    cwd.hash(&mut namespace);
    argv.hash(&mut namespace);
    let driver = std::env::current_exe()?;
    driver.hash(&mut namespace);
    // Treat dependency artifacts as opaque inputs. Across edits to the binary
    // crate, rustc can validate its own queries without trusting old externs.
    let mut externs = BTreeSet::from([driver]);
    for &krate in tcx.crates(()) {
        let source = tcx.used_crate_source(krate);
        for file in source.paths() {
            externs.insert(cwd.join(file));
        }
    }
    for file in externs {
        file.hash(&mut namespace);
        stamp(&file)?.hash(&mut namespace);
        inputs.insert(file);
    }
    let namespace = u128::from_le_bytes(namespace.finish::<Fingerprint>().to_le_bytes());
    let namespace = format!("{namespace:032x}");
    let mut out = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?,
    );
    field(&mut out, VERSION)?;
    field(&mut out, cwd.as_os_str().as_bytes())?;
    field(&mut out, namespace.as_bytes())?;
    field(&mut out, argv.len().to_string().as_bytes())?;
    for arg in argv {
        field(&mut out, arg.as_bytes())?;
    }
    field(&mut out, fingerprint(tcx).as_bytes())?;
    // Do not copy the invoking environment. These are only the env inputs
    // rustc itself records in Cargo's dep-info, including rustc-env overrides.
    let env = tcx.sess.env_depinfo.borrow();
    field(&mut out, env.len().to_string().as_bytes())?;
    for &(key, value) in env.iter() {
        field(&mut out, key.as_str().as_bytes())?;
        if let Some(value) = value {
            field(&mut out, b"1")?;
            field(&mut out, value.as_str().as_bytes())?;
        } else {
            field(&mut out, b"0")?;
        }
    }
    for file in inputs {
        field(&mut out, file.as_os_str().as_bytes())?;
        field(&mut out, stamp(&file)?.as_bytes())?;
    }
    out.flush()?;
    out.get_ref().set_modified(UNIX_EPOCH)
}

fn fingerprint(tcx: TyCtxt<'_>) -> String {
    // Use the expanded owners, rather than the crate hash: the latter
    // also hashes session output modes, which differ during replay.
    tcx.with_stable_hashing_context(|mut context| {
        let mut hash = StableHasher::new();
        let mut owners: Vec<_> = tcx.hir_crate_items(()).owners().collect();
        owners.sort_unstable_by_key(|owner| tcx.def_path_hash(owner.def_id.to_def_id()));
        // Spans can encode the compiler's invocation context. Source files
        // and dependency revisions are validated separately.
        context.while_hashing_spans(false, |context| {
            for owner in owners {
                if let Some(info) = tcx.lower_to_hir(owner.def_id).as_owner() {
                    owner.def_id.stable_hash(context, &mut hash);
                    info.node().stable_hash(context, &mut hash);
                    info.nodes.bodies.stable_hash(context, &mut hash);
                    info.attrs.map.stable_hash(context, &mut hash);
                    info.attrs.define_opaque.stable_hash(context, &mut hash);
                }
            }
        });
        hash.finish::<Fingerprint>().to_string()
    })
}

pub struct Recipe {
    pub argv: Vec<String>,
    pub namespace: String,
    cwd: PathBuf,
    env: Vec<(String, Option<String>)>,
    files: Vec<(PathBuf, String)>,
    hir: String,
}

impl Recipe {
    pub fn open(path: &Path) -> io::Result<Self> {
        let data = fs::read(path)?;
        let body = data
            .strip_suffix(b"\0")
            .ok_or_else(|| invalid("unterminated receipt"))?;
        let mut fields = body.split(|byte| *byte == 0);
        if fields.next() != Some(VERSION) {
            return Err(invalid("unknown receipt version"));
        }
        let mut next = || fields.next().ok_or_else(|| invalid("truncated receipt"));
        let cwd = PathBuf::from(OsStr::from_bytes(next()?));
        let namespace = text(next()?)?.to_owned();
        if namespace.len() != 32 || !namespace.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid incremental namespace"));
        }
        let mut argv = Vec::new();
        for _ in 0..count(next()?)? {
            argv.push(text(next()?)?.to_owned());
        }
        let hir = text(next()?)?.to_owned();
        let mut env = Vec::new();
        for _ in 0..count(next()?)? {
            let key = text(next()?)?.to_owned();
            if key.is_empty() || key.contains('=') {
                return Err(invalid("invalid compiler environment key"));
            }
            let value = match next()? {
                b"0" => None,
                b"1" => Some(text(next()?)?.to_owned()),
                _ => return Err(invalid("invalid compiler environment value")),
            };
            env.push((key, value));
        }
        let mut files = Vec::new();
        while let Some(file) = fields.next() {
            let expected = fields.next().ok_or_else(|| invalid("missing file stamp"))?;
            files.push((
                PathBuf::from(OsStr::from_bytes(file)),
                text(expected)?.to_owned(),
            ));
        }
        Ok(Self {
            argv,
            namespace,
            cwd,
            env,
            files,
            hir,
        })
    }

    pub fn verify(&self) -> io::Result<()> {
        for (file, expected) in &self.files {
            if !stamp(file).as_ref().is_ok_and(|stamp| stamp == expected) {
                return Err(invalid(format!(
                    "Cargo input changed during run: {}; rerun ns run",
                    file.display()
                )));
            }
        }
        Ok(())
    }

    pub fn verify_expansion(&self, tcx: TyCtxt<'_>) -> io::Result<()> {
        self.verify()?;
        if fingerprint(tcx) != self.hir {
            return Err(invalid(
                "Rust expansion differs from Cargo's checked input; rerun ns run",
            ));
        }
        Ok(())
    }

    pub fn enter(&self) -> io::Result<()> {
        self.verify()?;
        std::env::set_current_dir(&self.cwd)?;
        // This is the compiler worker's entry point, before rustc, logging,
        // or any compiler threads have been started.
        for (key, value) in &self.env {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        Ok(())
    }
}

fn field(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    out.write_all(bytes)?;
    out.write_all(b"\0")
}

fn stamp(path: &Path) -> io::Result<String> {
    let m = fs::metadata(path)?;
    Ok(format!(
        "{}:{}:{}:{}:{}",
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec()
    ))
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn text(bytes: &[u8]) -> io::Result<&str> {
    std::str::from_utf8(bytes).map_err(invalid)
}

fn count(bytes: &[u8]) -> io::Result<usize> {
    text(bytes)?
        .parse::<usize>()
        .ok()
        .filter(|&n| n <= 65536)
        .ok_or_else(|| invalid("invalid receipt field count"))
}
