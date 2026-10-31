//! Reproducible release archives (`tar.gz` and `zip`).
//!
//! Same inputs give byte-identical archives: every entry gets mtime
//! `SOURCE_DATE_EPOCH`, uid/gid 0 with empty owner names, entries are sorted
//! by path, binaries are mode 0755 and everything else 0644 (directories
//! 0755). gzip headers carry mtime 0 and no file name.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::project::ns_config::ArchiveFormat;

/// One file to pack, below the archive's top directory.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path inside the top directory, `/`-separated (e.g. `README.md`).
    pub name: String,
    pub source: PathBuf,
    pub executable: bool,
}

enum Node<'a> {
    Dir,
    File(&'a Entry),
}

pub fn extension(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::TarGz => "tar.gz",
        ArchiveFormat::Zip => "zip",
    }
}

/// Every archive path (top dir, intermediate dirs, files) in sorted order.
fn layout<'a>(top: &str, entries: &'a [Entry]) -> Result<BTreeMap<String, Node<'a>>> {
    let mut nodes = BTreeMap::new();
    nodes.insert(format!("{top}/"), Node::Dir);
    for e in entries {
        if e.name.is_empty()
            || e.name.starts_with('/')
            || e.name
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            bail!("invalid archive entry name {:?}", e.name);
        }
        let mut prefix = String::new();
        let parts: Vec<&str> = e.name.split('/').collect();
        for dir in &parts[..parts.len() - 1] {
            prefix.push_str(dir);
            prefix.push('/');
            nodes.insert(format!("{top}/{prefix}"), Node::Dir);
        }
        if nodes
            .insert(format!("{top}/{}", e.name), Node::File(e))
            .is_some()
        {
            bail!("duplicate archive entry {:?}", e.name);
        }
    }
    Ok(nodes)
}

fn file_mode(e: &Entry) -> u32 {
    if e.executable { 0o755 } else { 0o644 }
}

/// Write `dest` with `entries` under `top/`, all stamped with `mtime`.
pub fn write_archive(
    format: ArchiveFormat,
    dest: &Path,
    top: &str,
    entries: &[Entry],
    mtime: u64,
) -> Result<()> {
    let nodes = layout(top, entries)?;
    let file = File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let out = BufWriter::new(file);
    let result = match format {
        ArchiveFormat::TarGz => write_tar_gz(out, &nodes, mtime),
        ArchiveFormat::Zip => write_zip(out, &nodes, mtime),
    };
    result.with_context(|| format!("write {}", dest.display()))
}

fn write_tar_gz<W: Write>(out: W, nodes: &BTreeMap<String, Node>, mtime: u64) -> Result<()> {
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(out, flate2::Compression::best());
    let mut tar = tar::Builder::new(gz);
    for (path, node) in nodes {
        let mut header = tar::Header::new_gnu();
        header.set_mtime(mtime);
        header.set_uid(0);
        header.set_gid(0);
        match node {
            Node::Dir => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                tar.append_data(&mut header, path, std::io::empty())?;
            }
            Node::File(e) => {
                let data = std::fs::read(&e.source)
                    .with_context(|| format!("read {}", e.source.display()))?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(file_mode(e));
                header.set_size(data.len() as u64);
                tar.append_data(&mut header, path, data.as_slice())?;
            }
        }
    }
    let gz = tar.into_inner()?;
    gz.finish()?.flush()?;
    Ok(())
}

fn write_zip<W: Write + std::io::Seek>(
    out: W,
    nodes: &BTreeMap<String, Node>,
    mtime: u64,
) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let stamp = dos_time(mtime);
    let base = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(stamp);
    let mut zip = zip::ZipWriter::new(out);
    for (path, node) in nodes {
        match node {
            Node::Dir => zip.add_directory(path.as_str(), base.unix_permissions(0o755))?,
            Node::File(e) => {
                let data = std::fs::read(&e.source)
                    .with_context(|| format!("read {}", e.source.display()))?;
                zip.start_file(path.as_str(), base.unix_permissions(file_mode(e)))?;
                zip.write_all(&data)?;
            }
        }
    }
    zip.finish()?.flush()?;
    Ok(())
}

/// Unix seconds → (year, month, day, hour, minute, second) in UTC.
pub fn civil_from_epoch(secs: u64) -> (i64, u8, u8, u8, u8, u8) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        (rem / 3600) as u8,
        (rem / 60 % 60) as u8,
        (rem % 60) as u8,
    )
}

/// ZIP stores DOS time (1980..=2107); epochs outside are clamped.
fn dos_time(secs: u64) -> zip::DateTime {
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    let clamped = if y < 1980 {
        zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
    } else if y > 2107 {
        zip::DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58)
    } else {
        zip::DateTime::from_date_and_time(y as u16, mo, d, h, mi, s)
    };
    clamped.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path) -> Vec<Entry> {
        std::fs::write(dir.join("bin"), b"\x7fELF fake binary").unwrap();
        std::fs::write(dir.join("README.md"), b"readme\n").unwrap();
        std::fs::write(dir.join("guide.md"), b"guide\n").unwrap();
        // Deliberately unsorted input.
        vec![
            Entry {
                name: "docs/guide.md".into(),
                source: dir.join("guide.md"),
                executable: false,
            },
            Entry {
                name: "app".into(),
                source: dir.join("bin"),
                executable: true,
            },
            Entry {
                name: "README.md".into(),
                source: dir.join("README.md"),
                executable: false,
            },
        ]
    }

    #[test]
    fn archives_are_byte_identical_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        let entries = fixture(dir.path());
        for format in [ArchiveFormat::TarGz, ArchiveFormat::Zip] {
            let a = dir.path().join(format!("a.{}", extension(format)));
            let b = dir.path().join(format!("b.{}", extension(format)));
            write_archive(format, &a, "app-1.0.0-x", &entries, 1_700_000_000).unwrap();
            // Touch the sources: filesystem mtimes must not leak in.
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(dir.path().join("README.md"), b"readme\n").unwrap();
            let mut reversed = entries.clone();
            reversed.reverse();
            write_archive(format, &b, "app-1.0.0-x", &reversed, 1_700_000_000).unwrap();
            assert_eq!(
                std::fs::read(&a).unwrap(),
                std::fs::read(&b).unwrap(),
                "{format:?}"
            );
        }
    }

    #[test]
    fn tar_entries_are_sorted_with_fixed_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let entries = fixture(dir.path());
        let dest = dir.path().join("x.tar.gz");
        write_archive(ArchiveFormat::TarGz, &dest, "top", &entries, 42).unwrap();
        let gz = flate2::read::GzDecoder::new(File::open(&dest).unwrap());
        let mut tar = tar::Archive::new(gz);
        let mut seen = Vec::new();
        for e in tar.entries().unwrap() {
            let e = e.unwrap();
            let h = e.header();
            assert_eq!(h.mtime().unwrap(), 42);
            assert_eq!((h.uid().unwrap(), h.gid().unwrap()), (0, 0));
            seen.push((
                e.path().unwrap().to_string_lossy().into_owned(),
                h.mode().unwrap(),
            ));
        }
        let expected = [
            ("top/", 0o755),
            ("top/README.md", 0o644),
            ("top/app", 0o755),
            ("top/docs/", 0o755),
            ("top/docs/guide.md", 0o644),
        ];
        let expected: Vec<(String, u32)> =
            expected.iter().map(|(p, m)| (p.to_string(), *m)).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn zip_keeps_modes_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let entries = fixture(dir.path());
        let dest = dir.path().join("x.zip");
        // 2023-11-14 22:13:20 UTC.
        write_archive(ArchiveFormat::Zip, &dest, "top", &entries, 1_700_000_000).unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
        let app = zip.by_name("top/app").unwrap();
        assert_eq!(app.unix_mode().unwrap() & 0o777, 0o755);
        let t = app.last_modified().unwrap();
        assert_eq!(
            (t.year(), t.month(), t.day(), t.hour(), t.minute()),
            (2023, 11, 14, 22, 13)
        );
        drop(app);
        assert_eq!(
            zip.by_name("top/README.md").unwrap().unix_mode().unwrap() & 0o777,
            0o644
        );
    }

    #[test]
    fn epoch_conversion_and_clamping() {
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_epoch(951_782_400), (2000, 2, 29, 0, 0, 0));
        let t = dos_time(0);
        assert_eq!((t.year(), t.month(), t.day()), (1980, 1, 1));
    }

    #[test]
    fn rejects_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let bad = Entry {
            name: "../x".into(),
            source: dir.path().join("x"),
            executable: false,
        };
        let err = write_archive(
            ArchiveFormat::TarGz,
            &dir.path().join("a.tar.gz"),
            "t",
            &[bad],
            0,
        );
        assert!(err.is_err());
    }
}
