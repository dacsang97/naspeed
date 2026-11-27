//! Lifetime-safe garbage collection for lazy application patch generations.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

const LOCK: &str = "patch-gc.lock";
const LEASE: &str = "patch-lease";
const IMAGES: &str = "app-patch-images-v1";

fn invalid(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid app patch cache entry {}", path.display()),
    )
}

// The publisher holds this guard until both the plan and the live feed
// are published. Only its exclusive holder may collect.
pub(crate) fn begin(cache: &Path, collect: bool) -> io::Result<(File, bool)> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache.join(LOCK))?;
    if collect {
        loop {
            match file.try_lock() {
                Ok(()) => return Ok((file, true)),
                Err(TryLockError::WouldBlock) => break,
                Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
    }
    loop {
        match file.lock_shared() {
            Ok(()) => return Ok((file, false)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn sweep(
    cache: &Path,
    current: &Path,
    owner: &OsStr,
    keep: NonZeroUsize,
) -> io::Result<()> {
    if !fs::symlink_metadata(cache)?.is_dir() {
        return Err(invalid(cache));
    }
    let mut keep_all = Vec::new();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        let Some(nonce) = generation_nonce(&entry.file_name()) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let marker = read_entry(&path.join(LEASE))?;
        if path == current
            || (marker.is_some() && marker.as_deref() != Some(owner.as_bytes()))
        {
            keep_all.push(path);
        } else {
            // Owned by this binary, or orphaned without a lease: both age
            // out by count. A missing lease can never be flock-held.
            candidates.push((nonce, path));
        }
    }
    candidates.sort_unstable_by(|a, b| b.cmp(a));
    let mut remove_generations = Vec::new();
    for (index, (_, path)) in candidates.into_iter().enumerate() {
        if index < keep.get() - 1 || is_leased(&path.join(LEASE))? {
            keep_all.push(path);
        } else {
            remove_generations.push(path);
        }
    }

    let images = cache.join(IMAGES);
    let mut marked = HashSet::new();
    for path in keep_all {
        mark(&path.join("patch-plan"), &images, false, &mut marked)?;
    }
    if let Some(entries) = directory(&cache.join("live"))? {
        for entry in entries {
            let entry = entry?;
            if !entry.file_name().as_bytes().starts_with(b".") {
                mark(&entry.path(), &images, true, &mut marked)?;
            }
        }
    }

    // Snapshot candidates before deleting. Directory entries, rather than
    // paths supplied by a plan, delimit what may be removed.
    let mut remove_images = Vec::new();
    if let Some(scopes) = directory(&images)? {
        for scope in scopes {
            let scope = scope?;
            if !valid_key(&scope.file_name()) || !scope.file_type()?.is_dir() {
                continue;
            }
            for image in fs::read_dir(scope.path())? {
                let image = image?;
                if valid_key(&image.file_name()) && image.file_type()?.is_dir() {
                    let path = image.path();
                    if !marked.contains(&path) {
                        remove_images.push(path);
                    }
                }
            }
        }
    }

    let generations = remove_generations.len();
    let images = remove_images.len();
    for path in remove_generations.into_iter().chain(remove_images) {
        fs::remove_dir_all(path)?;
    }
    if (generations != 0 || images != 0) && std::env::var_os("NASPEED_LAZY_RUN_REPORT").as_deref() == Some(std::ffi::OsStr::new("1")) {
        eprintln!("naspeed app cache GC: removed {generations} generations and {images} images");
    }
    Ok(())
}

fn is_leased(path: &Path) -> io::Result<bool> {
    let file = match File::options().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // New launchers cannot acquire a lease while sweep holds the global
    // lock. Drop our probe fd here rather than retaining one per candidate.
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn read_entry(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= 128 * 1024 => fs::read(path).map(Some),
        Ok(_) => Err(invalid(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn directory(path: &Path) -> io::Result<Option<fs::ReadDir>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::read_dir(path).map(Some),
        Ok(_) => Err(invalid(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn mark(
    plan: &Path,
    images: &Path,
    allow_empty: bool,
    marked: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    let bytes = match read_entry(plan) {
        Ok(Some(bytes)) => bytes,
        // Missing or unreadable plans mark nothing; a generation that
        // failed to publish still has its directory collected by count.
        Ok(None) | Err(_) => return Ok(()),
    };
    if bytes.is_empty() && allow_empty {
        return Ok(());
    }
    let body = bytes.strip_suffix(&[0]).ok_or_else(|| invalid(plan))?;
    let mut fields = body.split(|byte| *byte == 0);
    let base = fields.next().ok_or_else(|| invalid(plan))?;
    mark_image(base, images, plan, marked)?;
    for index in 0..64 {
        let Some(slot) = fields.next() else {
            return if index == 0 {
                Err(invalid(plan))
            } else {
                Ok(())
            };
        };
        // Slot tokens carry their selector name ("<index>:<leaf>"); the
        // index alone is authoritative for GC marking.
        let slot = std::str::from_utf8(slot)
            .ok()
            .and_then(|s| s.split(':').next()?.parse::<usize>().ok());
        if slot != Some(index) {
            return Err(invalid(plan));
        }
        mark_image(
            fields.next().ok_or_else(|| invalid(plan))?,
            images,
            plan,
            marked,
        )?;
    }
    if fields.next().is_some() {
        return Err(invalid(plan));
    }
    Ok(())
}

fn mark_image(
    bytes: &[u8],
    images: &Path,
    plan: &Path,
    marked: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    let path = Path::new(OsStr::from_bytes(bytes));
    let relative = path.strip_prefix(images).map_err(|_| invalid(plan))?;
    let mut parts = relative.components();
    let (
        Some(Component::Normal(scope)),
        Some(Component::Normal(key)),
        Some(Component::Normal(name)),
        None,
    ) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid(plan));
    };
    if !valid_key(scope) || !valid_key(key) || name != "app.dylib" {
        return Err(invalid(plan));
    }
    marked.insert(images.join(scope).join(key));
    Ok(())
}

fn generation_nonce(name: &OsStr) -> Option<u128> {
    let (pid, nonce) = name.to_str()?.split_once('-')?;
    pid.parse::<u32>().ok()?;
    if nonce.is_empty() || !nonce.bytes().all(lower_hex) {
        return None;
    }
    u128::from_str_radix(nonce, 16).ok()
}

fn lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn valid_key(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 64 && bytes.iter().copied().all(lower_hex)
}
