//! The one layer `ns image` adds on top of the base: a gzipped tar holding
//! the binary at its image path (e.g. `/usr/bin/<bin>`) plus every parent
//! directory. Like `COPY` in a Dockerfile, entries are owned by root
//! (0:0) with mode 0755 and mtime `SOURCE_DATE_EPOCH`. Every header field and
//! the gzip header are fixed, so the same binary always yields byte-identical
//! layers and digests.

use std::io::Write;

use anyhow::{Context, Result, bail};
use flate2::{Compression, GzBuilder};

use super::registry_client::sha256_digest;

pub struct Layer {
    /// gzip bytes; their sha256 is the layer digest.
    pub blob: Vec<u8>,
    /// `sha256:` of the gzip bytes (the descriptor digest).
    pub digest: String,
    /// `sha256:` of the uncompressed tar (`rootfs.diff_ids` entry).
    pub diff_id: String,
}

/// Components of an absolute, normalised file path inside the image
/// (`/usr/bin/api` → `["usr", "bin", "api"]`).
pub fn image_path_components(path: &str) -> Result<Vec<&str>> {
    let Some(rest) = path.strip_prefix('/') else {
        bail!("image path {path:?} must be absolute");
    };
    let parts: Vec<&str> = rest.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        bail!("image path {path:?} must be a normalised file path (no empty, `.` or `..` parts)");
    }
    Ok(parts)
}

fn header(entry_type: tar::EntryType, size: u64, mtime: u64) -> Result<tar::Header> {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    header
        .set_username("")
        .and_then(|_| header.set_groupname(""))
        .context("tar header")?;
    Ok(header)
}

/// Build the layer placing `contents` at the absolute image `path`.
pub fn build_layer(path: &str, contents: &[u8], mtime: u64) -> Result<Layer> {
    let parts = image_path_components(path)?;
    let mut tar = tar::Builder::new(Vec::new());
    for depth in 1..parts.len() {
        let dir = format!("{}/", parts[..depth].join("/"));
        let mut h = header(tar::EntryType::Directory, 0, mtime)?;
        tar.append_data(&mut h, &dir, std::io::empty())
            .with_context(|| format!("append {dir} to layer tar"))?;
    }
    let mut h = header(tar::EntryType::Regular, contents.len() as u64, mtime)?;
    tar.append_data(&mut h, parts.join("/"), contents)
        .context("append binary to layer tar")?;
    let tar_bytes = tar.into_inner().context("finish layer tar")?;

    // No file name and mtime 0 in the gzip header: compression output depends
    // only on the tar bytes.
    let mut gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    gz.write_all(&tar_bytes).context("gzip layer")?;
    let blob = gz.finish().context("finish gzip layer")?;
    Ok(Layer {
        digest: sha256_digest(&blob),
        diff_id: sha256_digest(&tar_bytes),
        blob,
    })
}

/// Which libc a Linux ELF binary needs, from its `PT_INTERP` program header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linkage {
    /// No interpreter: runs on `distroless/static`.
    Static,
    /// Dynamically linked: needs a glibc base (`distroless/cc`).
    Dynamic,
}

/// Check a 64-bit little-endian ELF for the expected machine and report its linkage.
pub fn inspect_elf(bytes: &[u8], architecture: &str) -> Result<Linkage> {
    const PT_INTERP: u32 = 3;
    let u16_at = |off: usize| {
        bytes
            .get(off..off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |off: usize| {
        bytes
            .get(off..off + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let u64_at = |off: usize| {
        bytes
            .get(off..off + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    };
    if bytes.get(..6) != Some(b"\x7fELF\x02\x01") {
        bail!("not a 64-bit little-endian ELF binary");
    }
    let machine = u16_at(0x12).context("truncated ELF header")?;
    let expected = match architecture {
        "amd64" => 62,
        "arm64" => 183,
        other => bail!("unsupported architecture {other}"),
    };
    if machine != expected {
        bail!("ELF machine {machine} does not match linux/{architecture}");
    }
    let phoff = u64_at(0x20).context("truncated ELF header")? as usize;
    let phentsize = u16_at(0x36).context("truncated ELF header")? as usize;
    let phnum = u16_at(0x38).context("truncated ELF header")? as usize;
    for i in 0..phnum {
        let p_type = u32_at(phoff + i * phentsize).context("truncated ELF program headers")?;
        if p_type == PT_INTERP {
            return Ok(Linkage::Dynamic);
        }
    }
    Ok(Linkage::Static)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn layer_digests_match_the_bytes_and_are_deterministic() {
        let a = build_layer("/usr/bin/hello", b"#!binary", 1_700_000_000).unwrap();
        let b = build_layer("/usr/bin/hello", b"#!binary", 1_700_000_000).unwrap();
        assert_eq!(a.blob, b.blob);
        assert_eq!(a.digest, sha256_digest(&a.blob));
        assert_ne!(a.digest, a.diff_id);
        let mut tar_bytes = Vec::new();
        flate2::read::GzDecoder::new(&a.blob[..])
            .read_to_end(&mut tar_bytes)
            .unwrap();
        assert_eq!(a.diff_id, sha256_digest(&tar_bytes));
        let other = build_layer("/usr/bin/hello", b"#!binary", 1_700_000_001).unwrap();
        assert_ne!(other.digest, a.digest);
    }

    #[test]
    fn layer_holds_parent_dirs_and_binary_owned_by_root() {
        let layer = build_layer("/usr/bin/hello", b"data", 42).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&layer.blob[..]));
        let entries: Vec<_> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let h = e.header().clone();
                (
                    h.path().unwrap().to_string_lossy().into_owned(),
                    h.entry_type(),
                    h.mode().unwrap(),
                    (h.uid().unwrap(), h.gid().unwrap()),
                    h.mtime().unwrap(),
                    h.size().unwrap(),
                )
            })
            .collect();
        use tar::EntryType::{Directory, Regular};
        assert_eq!(
            entries,
            vec![
                ("usr/".into(), Directory, 0o755, (0, 0), 42, 0),
                ("usr/bin/".into(), Directory, 0o755, (0, 0), 42, 0),
                ("usr/bin/hello".into(), Regular, 0o755, (0, 0), 42, 4),
            ]
        );
        let top = build_layer("/hello", b"data", 42).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&top.blob[..]));
        assert_eq!(archive.entries().unwrap().count(), 1);
        for bad in ["hello", "/usr//hello", "/usr/../hello", "/usr/bin/", "/"] {
            assert!(build_layer(bad, b"", 0).is_err(), "{bad}");
        }
    }

    fn fake_elf(machine: u16, interp: bool) -> Vec<u8> {
        let mut elf = vec![0u8; 64 + 56 * 2];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[0x12..0x14].copy_from_slice(&machine.to_le_bytes());
        elf[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        elf[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        elf[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
        elf[64..68].copy_from_slice(&6u32.to_le_bytes());
        elf[120..124].copy_from_slice(&(if interp { 3u32 } else { 1 }).to_le_bytes());
        elf
    }

    #[test]
    fn elf_linkage_and_machine() {
        assert_eq!(
            inspect_elf(&fake_elf(62, false), "amd64").unwrap(),
            Linkage::Static
        );
        assert_eq!(
            inspect_elf(&fake_elf(183, true), "arm64").unwrap(),
            Linkage::Dynamic
        );
        assert!(inspect_elf(&fake_elf(62, false), "arm64").is_err());
        assert!(inspect_elf(b"MZ", "amd64").is_err());
    }
}
