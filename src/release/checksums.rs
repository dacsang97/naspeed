//! SHA-256 digests and the checksum file (`release.checksum`, default
//! `SHA256SUMS`) in `sha256sum -c` format: one `<64 lowercase hex>  <file
//! name>` line per file, sorted by name.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// `release.checksum` must be a plain file name that no other output uses.
pub fn check_file_name(name: &str) -> Result<()> {
    let reserved = [super::dist::MARKER, super::dist::METADATA];
    if name.is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\\'])
        || reserved.contains(&name)
    {
        bail!("release.checksum {name:?} is not a usable file name");
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streaming SHA-256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// `SHA256SUMS` content for `(file name, hex digest)` pairs.
pub fn render(entries: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = entries.iter().collect();
    sorted.sort();
    sorted
        .iter()
        .map(|(name, digest)| format!("{digest}  {name}\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn checksum_file_names() {
        check_file_name("checksums.txt").unwrap();
        check_file_name("SHA256SUMS").unwrap();
        for bad in ["", "a/b", ".hidden", "metadata.json"] {
            assert!(check_file_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sha256sum_format_sorted() {
        let text = render(&[
            ("b.zip".into(), "2".repeat(64)),
            ("a.tar.gz".into(), "1".repeat(64)),
        ]);
        assert_eq!(
            text,
            format!("{}  a.tar.gz\n{}  b.zip\n", "1".repeat(64), "2".repeat(64))
        );
    }
}
