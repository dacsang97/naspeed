//! Pick the store for a target dir: clones never cross volumes, so only a
//! store on the same device (`st_dev`, the volume serial on Windows) can
//! serve it.

use std::path::{Path, PathBuf};

pub use crate::platform::device_of;

/// The configured store on the same device as `dir`, if it exists and is
/// writable. A missing store (e.g. unplugged USB volume) means passthrough.
pub fn store_for(dir: &Path, stores: &[PathBuf]) -> Option<PathBuf> {
    let dev = device_of(dir)?;
    stores
        .iter()
        .find(|store| device_of(store) == Some(dev) && is_writable(store))
        .cloned()
}

/// `path` or its nearest existing ancestor (a target dir that cargo has
/// not created yet lives on its parent's volume).
pub fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|p| p.exists())
}

fn is_writable(dir: &Path) -> bool {
    crate::platform::is_writable_dir(dir)
}

/// Mount point of the volume holding `path` (walk up while `st_dev` stays).
pub fn mount_point(path: &Path) -> Option<PathBuf> {
    let path = crate::platform::canonicalize(path).ok()?;
    let dev = device_of(&path)?;
    let mut current = path.as_path();
    while let Some(parent) = current.parent() {
        if device_of(parent) != Some(dev) {
            break;
        }
        current = parent;
    }
    Some(current.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_store_on_same_device_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        assert_eq!(
            store_for(&target, std::slice::from_ref(&store)),
            Some(store.clone())
        );
        assert_eq!(store_for(&target, &[dir.path().join("missing")]), None);
    }
}
