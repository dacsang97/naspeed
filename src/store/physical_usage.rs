//! Logical vs physical size. `du` counts every clone at full size; on APFS
//! `getattrlist(ATTR_CMNEXT_PRIVATESIZE)` gives the bytes a file does not
//! share with any other file, which is what deleting it would free.

use std::path::Path;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub files: u64,
    /// Sum of file lengths.
    pub logical: u64,
    /// Sum of unshared bytes, when the filesystem can report it.
    pub private: Option<u64>,
}

impl Usage {
    fn add(&mut self, other: Usage) {
        self.files += other.files;
        self.logical += other.logical;
        self.private = match (self.private, other.private) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
    }
}

#[cfg(target_os = "macos")]
pub fn private_size(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;

    #[repr(C, packed(4))]
    struct Buf {
        length: u32,
        private_size: i64,
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
    list.bitmapcount = ATTR_BIT_MAP_COUNT;
    list.forkattr = ATTR_CMNEXT_PRIVATESIZE;
    let mut buf = Buf {
        length: 0,
        private_size: 0,
    };
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            &mut list as *mut _ as *mut libc::c_void,
            &mut buf as *mut _ as *mut libc::c_void,
            std::mem::size_of::<Buf>(),
            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if rc != 0 || (buf.length as usize) < std::mem::size_of::<Buf>() {
        return None;
    }
    let size = buf.private_size;
    u64::try_from(size).ok()
}

#[cfg(not(target_os = "macos"))]
pub fn private_size(_path: &Path) -> Option<u64> {
    None
}

pub fn file_usage(path: &Path) -> Usage {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Usage {
            files: 1,
            logical: meta.len(),
            private: private_size(path),
        },
        _ => Usage {
            private: Some(0),
            ..Usage::default()
        },
    }
}

/// Unshared bytes of a file inside a target dir. Cargo "uplifts" binaries
/// and build scripts by cloning them (`deps/app-<hash>` → `app`), so both
/// copies report 0 private bytes while the pair still occupies one copy.
/// A clone-shared file that naspeed did not place or store is counted at
/// half its size, which is exact for cargo's two-copy uplifts.
pub fn target_file_unshared(path: &Path) -> Option<u64> {
    let len = std::fs::symlink_metadata(path).ok()?.len();
    match private_size(path)? {
        0 if crate::wrapper::extern_identity::read_xattr(path).is_none() => Some(len / 2),
        p => Some(p),
    }
}

/// `tree_usage` with `target_file_unshared` as the private size.
pub fn target_tree_usage(dir: &Path) -> Usage {
    walk(dir, &|p| Usage {
        files: 1,
        logical: std::fs::symlink_metadata(p).map(|m| m.len()).unwrap_or(0),
        private: target_file_unshared(p),
    })
}

pub fn tree_usage(dir: &Path) -> Usage {
    walk(dir, &file_usage)
}

fn walk(dir: &Path, per_file: &dyn Fn(&Path) -> Usage) -> Usage {
    let mut total = Usage {
        private: Some(0),
        ..Usage::default()
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return total;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total.add(walk(&entry.path(), per_file));
        } else if file_type.is_file() {
            total.add(per_file(&entry.path()));
        }
    }
    total
}

pub fn human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_has_no_private_bytes_on_apfs() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::write(&a, vec![7u8; 256 * 1024]).unwrap();
        let b = dir.path().join("b");
        let method =
            crate::store::cow_copy::clone_or_copy(&a, &b, crate::config::CowMode::Auto).unwrap();
        let usage = tree_usage(dir.path());
        assert_eq!(usage.files, 2);
        assert_eq!(usage.logical, 512 * 1024);
        if cfg!(target_os = "macos") && method == crate::store::cow_copy::Method::Clone {
            assert_eq!(private_size(&b), Some(0));
        }
    }

    #[test]
    fn human_units() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }
}
