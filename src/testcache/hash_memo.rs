//! Content hashes of test inputs, remembered by stat like git's index:
//! `(path, size, mtime, inode) → blake3`. A file whose stat still matches is
//! not read again, so keying a test binary costs a walk and a `stat` per
//! file instead of hashing every source and fixture of the package and its
//! dependencies, for every binary of every run.
//!
//! Entries live in `<store>/memo/test-file-hashes.tsv`, appended by each
//! runner (one write per line, O_APPEND). A file modified within the last
//! `RACY` is hashed but not remembered: its mtime could still be shared by a
//! later edit (git's "racy clean" problem).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::store::content_hash::hash_file;

const FILE: &str = "test-file-hashes.tsv";
const RACY: Duration = Duration::from_secs(2);
/// Past this size the memo is dropped and rebuilt from scratch.
const MAX_BYTES: u64 = 64 << 20;

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime_ns: i128,
    ino: u64,
}

struct Memo {
    path: Option<PathBuf>,
    known: HashMap<PathBuf, (Stamp, String)>,
    new: Vec<String>,
}

static MEMO: Mutex<Option<Memo>> = Mutex::new(None);

/// Nanoseconds since the epoch, negative before it (like `st_mtime`).
fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

fn stamp(meta: &std::fs::Metadata) -> Stamp {
    Stamp {
        size: meta.len(),
        mtime_ns: mtime_ns(meta),
        ino: crate::platform::file_index(meta),
    }
}

fn load(path: &Path) -> HashMap<PathBuf, (Stamp, String)> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::remove_file(path);
        return HashMap::new();
    }
    let mut known = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return known;
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if let [p, size, mtime, ino, hash] = f.as_slice()
            && let (Ok(size), Ok(mtime_ns), Ok(ino)) = (size.parse(), mtime.parse(), ino.parse())
        {
            // Later lines win: a file re-hashed after an edit.
            known.insert(
                PathBuf::from(p),
                (
                    Stamp {
                        size,
                        mtime_ns,
                        ino,
                    },
                    (*hash).to_owned(),
                ),
            );
        }
    }
    known
}

/// Use the memo in `store` for this process.
pub fn init(store: &Path) {
    let path = store.join("memo").join(FILE);
    let known = load(&path);
    *MEMO.lock().unwrap_or_else(|e| e.into_inner()) = Some(Memo {
        path: Some(path),
        known,
        new: Vec::new(),
    });
}

/// blake3 of `path`'s content, from the memo when its stat is unchanged.
pub fn hash(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    let st = stamp(&meta);
    let mut guard = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(memo) = guard.as_mut()
        && let Some((known, h)) = memo.known.get(path)
        && *known == st
    {
        return Ok(h.clone());
    }
    let h = hash_file(path)?.to_hex().to_string();
    let settled = meta
        .modified()
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= RACY);
    let printable = path.to_str().is_some_and(|p| !p.contains(['\t', '\n']));
    if let Some(memo) = guard.as_mut()
        && settled
        && printable
    {
        memo.new.push(format!(
            "{}\t{}\t{}\t{}\t{h}\n",
            path.display(),
            st.size,
            st.mtime_ns,
            st.ino
        ));
        memo.known.insert(path.to_owned(), (st, h.clone()));
    }
    Ok(h)
}

/// Append what this process learned.
pub fn flush() {
    let mut guard = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    let Some(memo) = guard.as_mut() else {
        return;
    };
    let Some(path) = memo.path.as_ref() else {
        return;
    };
    if memo.new.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        for line in memo.new.drain(..) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_files_come_from_the_memo_and_edits_are_rehashed() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();
        // Older than RACY, so it may be remembered.
        let old = SystemTime::now() - Duration::from_secs(60);
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old)).unwrap();

        init(&store);
        let first = hash(&file).unwrap();
        flush();
        let memo = std::fs::read_to_string(store.join("memo").join(FILE)).unwrap();
        assert!(memo.contains(&first), "{memo}");

        // A fresh process sees the memo line.
        init(&store);
        assert_eq!(hash(&file).unwrap(), first);

        // An edit changes the stamp and the hash; a just-written file is
        // hashed but not remembered.
        std::fs::write(&file, "two").unwrap();
        let second = hash(&file).unwrap();
        assert_ne!(second, first);
        flush();
        let memo = std::fs::read_to_string(store.join("memo").join(FILE)).unwrap();
        assert!(!memo.contains(&second), "racy entry was remembered");
    }
}
