//! Unix side of `platform`: mode bits, xattrs, `st_dev`, `statvfs`.

use std::fs::Metadata;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Permission bits (`0o777`) of a file.
pub fn mode_of(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o777
}

pub fn is_executable(meta: &Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

pub fn symlink(target: impl AsRef<Path>, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Atomically puts `tmp` at `dst`; running processes keep the old inode.
pub fn replace_exe(tmp: &Path, dst: &Path) -> io::Result<()> {
    std::fs::rename(tmp, dst)
}

pub fn remove_exe(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// Identifies the volume holding `path`: clones never cross it.
pub fn device_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.dev())
}

/// Inode number, for stat-based memos.
pub fn file_index(meta: &Metadata) -> u64 {
    meta.ino()
}

pub fn is_writable_dir(dir: &Path) -> bool {
    rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok()
}

pub fn free_bytes(dir: &Path) -> Option<u64> {
    let st = rustix::fs::statvfs(dir).ok()?;
    Some(st.f_bavail * st.f_frsize)
}

/// Lowest-but-one CPU priority for the current process (background work).
pub fn lower_priority() {
    // SAFETY: plain libc call on the current process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 15);
    }
}

/// A small extended attribute (`user.*`), `None` when absent or unsupported.
pub fn get_attr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    let n = rustix::fs::getxattr(path, name, &mut buf).ok()?;
    Some(buf[..n].to_vec())
}

/// Sets a small extended attribute. Does not touch the file's mtime.
pub fn set_attr(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    rustix::fs::setxattr(path, name, value, rustix::fs::XattrFlags::empty())?;
    Ok(())
}
