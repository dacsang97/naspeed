//! Windows block cloning: `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, the ReFS / Dev
//! Drive equivalent of `clonefile` and `FICLONE`. NTFS answers the integrity
//! query with `ERROR_INVALID_FUNCTION`, which `cow_copy` treats as "no
//! clones here" and falls back to a plain copy.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_INTEGRITY_INFORMATION,
    FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_INTEGRITY_INFORMATION,
    FSCTL_SET_INTEGRITY_INFORMATION_BUFFER,
};

/// One ioctl clones less than 4 GiB; 1 GiB is a multiple of every cluster size.
const CHUNK: u64 = 1 << 30;

/// `DeviceIoControl` with a fixed-size input and output (either may be empty).
fn ioctl<I, O>(
    file: &File,
    code: u32,
    input: Option<&I>,
    output: Option<&mut O>,
) -> io::Result<()> {
    let (in_ptr, in_len) = match input {
        Some(i) => (i as *const I as *const _, size_of::<I>() as u32),
        None => (std::ptr::null(), 0),
    };
    let (out_ptr, out_len) = match output {
        Some(o) => (o as *mut O as *mut _, size_of::<O>() as u32),
        None => (std::ptr::null_mut(), 0),
    };
    let mut returned = 0u32;
    // SAFETY: the handle is owned by `file`; the buffers are plain structs
    // of exactly the given sizes; synchronous call (no OVERLAPPED).
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            in_ptr,
            in_len,
            out_ptr,
            out_len,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn integrity(file: &File) -> io::Result<FSCTL_GET_INTEGRITY_INFORMATION_BUFFER> {
    // SAFETY: plain data, filled by the ioctl.
    let mut info: FSCTL_GET_INTEGRITY_INFORMATION_BUFFER = unsafe { std::mem::zeroed() };
    ioctl::<(), _>(file, FSCTL_GET_INTEGRITY_INFORMATION, None, Some(&mut info))?;
    Ok(info)
}

fn clone_into(from: &File, to: &File, len: u64) -> io::Result<()> {
    let src = integrity(from)?;
    let cluster = u64::from(src.ClusterSizeInBytes);
    if cluster == 0 || from.metadata()?.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    // Source and destination must agree on integrity streams.
    if integrity(to)?.ChecksumAlgorithm != src.ChecksumAlgorithm {
        let set = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: src.ChecksumAlgorithm,
            Reserved: 0,
            Flags: src.Flags,
        };
        ioctl::<_, ()>(to, FSCTL_SET_INTEGRITY_INFORMATION, Some(&set), None)?;
    }
    // Ranges are whole clusters: clone up to the rounded size, then trim.
    let total = len.div_ceil(cluster) * cluster;
    to.set_len(total)?;
    let mut offset = 0;
    while offset < total {
        let count = (total - offset).min(CHUNK);
        let data = DUPLICATE_EXTENTS_DATA {
            FileHandle: from.as_raw_handle(),
            SourceFileOffset: offset as i64,
            TargetFileOffset: offset as i64,
            ByteCount: count as i64,
        };
        ioctl::<_, ()>(to, FSCTL_DUPLICATE_EXTENTS_TO_FILE, Some(&data), None)?;
        offset += count;
    }
    to.set_len(len)
}

/// Clone `src` to the new file `dst`; on any error `dst` is removed.
pub fn try_clone(src: &Path, dst: &Path) -> io::Result<()> {
    let from = File::open(src)?;
    let meta = from.metadata()?;
    let to = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dst)?;
    let result = if meta.len() == 0 {
        // Nothing to share; still answer "unsupported" off ReFS so probes
        // and the copy fallback behave the same for empty files.
        integrity(&from).map(|_| ())
    } else {
        clone_into(&from, &to, meta.len())
    };
    drop(to);
    if let Err(e) = result {
        let _ = std::fs::remove_file(dst);
        return Err(e);
    }
    // Like clonefile/FICLONE callers expect: the clone keeps the mode.
    if meta.permissions().readonly() {
        crate::platform::set_mode(dst, 0o444)?;
    }
    Ok(())
}
