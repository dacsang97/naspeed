//! Windows side of `platform`. Windows has no mode bits: the read-only
//! attribute stands in for "no write bit", and every file is "executable"
//! by extension. Attributes live in alternate data streams.

use std::ffi::OsString;
use std::fs::Metadata;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use filetime::FileTime;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    GetDiskFreeSpaceExW, GetFileInformationByHandle,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
};

/// NUL-terminated UTF-16 for Win32 calls.
pub(crate) fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Maps the write bits onto the read-only attribute.
#[allow(clippy::permissions_set_readonly_false)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    let readonly = mode & 0o222 == 0;
    if perms.readonly() == readonly {
        return Ok(());
    }
    perms.set_readonly(readonly);
    std::fs::set_permissions(path, perms)
}

/// `0o444` for read-only files, else `0o644`.
pub fn mode_of(meta: &Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

pub fn is_executable(meta: &Metadata) -> bool {
    meta.is_file()
}

/// A file or directory symlink, matching what `target` is. Needs Developer
/// Mode or the symlink privilege, like any symlink on Windows.
pub fn symlink(target: impl AsRef<Path>, link: &Path) -> io::Result<()> {
    let target = target.as_ref();
    let resolved = link.parent().map(|p| p.join(target));
    if resolved.is_some_and(|p| p.is_dir()) {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// `<exe>.old-<pid>-<n>`: where a running executable is moved aside.
fn aside_path(path: &Path, n: u32) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".old-{}-{n}", std::process::id()));
    name.into()
}

/// Deletes `<exe>.old-*` left behind by earlier replacements (best effort:
/// one still running stays until the next try).
fn remove_stale_asides(path: &Path) {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.old-", stem.to_string_lossy());
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Moves `path` aside when it cannot be replaced or deleted because it is
/// running: Windows forbids deleting a running executable but allows
/// renaming it.
fn move_aside(path: &Path) -> io::Result<()> {
    let mut n = 0;
    loop {
        match std::fs::rename(path, aside_path(path, n)) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 16 => n += 1,
            other => return other,
        }
    }
}

/// Puts `tmp` at `dst`, even while `dst` is running (it is moved aside).
pub fn replace_exe(tmp: &Path, dst: &Path) -> io::Result<()> {
    remove_stale_asides(dst);
    if std::fs::rename(tmp, dst).is_ok() {
        return Ok(());
    }
    if dst.exists() {
        move_aside(dst)?;
    }
    std::fs::rename(tmp, dst)?;
    remove_stale_asides(dst);
    Ok(())
}

/// Deletes an executable, moving it aside first if it is running.
pub fn remove_exe(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            move_aside(path)?;
            remove_stale_asides(path);
            Ok(())
        }
        other => other,
    }
}

fn file_information(path: &Path) -> Option<BY_HANDLE_FILE_INFORMATION> {
    // Backup semantics: directories can be opened too.
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    // SAFETY: valid handle owned by `file`; the struct is plain data.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    (ok != 0).then_some(info)
}

/// The volume serial number: clones never cross volumes.
pub fn device_of(path: &Path) -> Option<u64> {
    file_information(path).map(|i| u64::from(i.dwVolumeSerialNumber))
}

/// No file index from `Metadata` on stable Rust; stat memos fall back to
/// size and the 100 ns mtime.
pub fn file_index(_meta: &Metadata) -> u64 {
    0
}

/// Directories have no usable write bit on Windows; an existing directory
/// counts as writable and a failing write later means passthrough.
pub fn is_writable_dir(dir: &Path) -> bool {
    std::fs::metadata(dir).is_ok_and(|m| m.is_dir())
}

pub fn free_bytes(dir: &Path) -> Option<u64> {
    let path = wide(dir);
    let mut avail = 0u64;
    // SAFETY: NUL-terminated path; the two unused outputs may be null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

pub fn lower_priority() {
    // SAFETY: pseudo-handle of the current process.
    unsafe {
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// `user.naspeed.id` → `<path>:naspeed.id`
fn stream_path(path: &Path, name: &str) -> OsString {
    let mut s = path.as_os_str().to_owned();
    s.push(":");
    s.push(name.strip_prefix("user.").unwrap_or(name));
    s
}

pub fn get_attr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(stream_path(path, name)).ok()?;
    (data.len() <= 256).then_some(data)
}

/// Writes the stream, then puts the file's times back: writing a stream
/// bumps the file's mtime, and cargo's freshness checks compare mtimes.
pub fn set_attr(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let before = std::fs::metadata(path)?;
    if !before.is_file() {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    std::fs::write(stream_path(path, name), value)?;
    filetime::set_file_times(
        path,
        FileTime::from_last_access_time(&before),
        FileTime::from_last_modification_time(&before),
    )
}
