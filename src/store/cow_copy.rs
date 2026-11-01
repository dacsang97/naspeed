//! Copy-on-write file copies: `clonefile` on APFS, `FICLONE` reflink on
//! btrfs/xfs, block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) on ReFS and
//! Dev Drive, otherwise a real copy (`copy_file_range` on Linux). Hardlinks
//! only when explicitly configured.

use std::io;
use std::path::Path;

use crate::config::CowMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Clone,
    Copy,
    Hardlink,
}

/// Copy `src` to `dst` (which must not exist).
pub fn clone_or_copy(src: &Path, dst: &Path, mode: CowMode) -> io::Result<Method> {
    match mode {
        CowMode::Hardlink => {
            std::fs::hard_link(src, dst)?;
            Ok(Method::Hardlink)
        }
        CowMode::Copy => {
            plain_copy(src, dst)?;
            Ok(Method::Copy)
        }
        CowMode::Auto => match try_clone(src, dst) {
            Ok(()) => Ok(Method::Clone),
            Err(e) if clone_unsupported(&e) => {
                plain_copy(src, dst)?;
                Ok(Method::Copy)
            }
            Err(e) => Err(e),
        },
    }
}

#[cfg(unix)]
fn clone_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::EINVAL) | Some(libc::ENOSYS)
    ) || e.raw_os_error() == Some(libc::EOPNOTSUPP)
        || e.kind() == io::ErrorKind::Unsupported
}

#[cfg(windows)]
fn clone_unsupported(e: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_BLOCK_TOO_MANY_REFERENCES, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
        ERROR_NOT_SAME_DEVICE, ERROR_NOT_SUPPORTED,
    };
    e.kind() == io::ErrorKind::Unsupported
        || e.raw_os_error().is_some_and(|code| {
            [
                ERROR_INVALID_FUNCTION,
                ERROR_NOT_SUPPORTED,
                ERROR_NOT_SAME_DEVICE,
                ERROR_INVALID_PARAMETER,
                ERROR_BLOCK_TOO_MANY_REFERENCES,
            ]
            .contains(&(code as u32))
        })
}

#[cfg(target_os = "macos")]
fn try_clone(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    // CLONE_NOFOLLOW | CLONE_NOOWNERCOPY
    let rc = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0x0001 | 0x0002) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn try_clone(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let from = std::fs::File::open(src)?;
    let mode = from
        .metadata()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()))?;
    let to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode & 0o7777)
        .open(dst)?;
    match rustix::fs::ioctl_ficlone(&to, &from) {
        Ok(()) => Ok(()),
        Err(e) => {
            drop(to);
            let _ = std::fs::remove_file(dst);
            Err(e.into())
        }
    }
}

#[cfg(windows)]
use super::block_clone::try_clone;

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn try_clone(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

fn plain_copy(src: &Path, dst: &Path) -> io::Result<()> {
    // std::fs::copy uses copy_file_range on Linux, fcopyfile on macOS and
    // CopyFileExW on Windows.
    std::fs::copy(src, dst).map(|_| ())
}

/// Probe whether `dir` supports clones by cloning a scratch file.
pub fn supports_clone(dir: &Path) -> bool {
    let src = dir.join(format!(".naspeed-probe-src-{}", std::process::id()));
    let dst = dir.join(format!(".naspeed-probe-dst-{}", std::process::id()));
    let ok = std::fs::write(&src, b"naspeed clone probe").is_ok() && try_clone(&src, &dst).is_ok();
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_content_in_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        std::fs::write(&src, b"payload").unwrap();
        for (i, mode) in [CowMode::Auto, CowMode::Copy, CowMode::Hardlink]
            .into_iter()
            .enumerate()
        {
            let dst = dir.path().join(format!("b{i}"));
            clone_or_copy(&src, &dst, mode).unwrap();
            assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
        }
    }
}
