//! Sync storage backends: dumb object stores with list/get/put-if-absent/delete.
//!
//! The sync protocol (see `sync.rs`) only needs these four operations, which is why
//! a local folder, an S3-compatible bucket and a WebDAV collection can all serve as
//! a sync target without any server-side logic. Every backend maps the same
//! `/`-separated keys; segment and snapshot files are immutable once written, so
//! `put_if_absent` (never overwrite) is the only write that matters. A torn write
//! is never silent: segment contents are authenticated on open, so a damaged object
//! fails the pull loudly instead of being applied.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fg_core::{Error, Result};

use crate::lock_error;

/// Dumb object store used as a sync target. Keys are `/`-separated relative paths
/// with no empty, `.` or `..` components; prefixes for [`SyncBackend::list`] have
/// no trailing slash. Implementations must be safe to call from any thread.
pub trait SyncBackend: Send + Sync {
    /// Keys under `prefix` ("" lists everything), sorted, `/`-separated.
    /// A missing prefix lists as empty, not an error.
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    /// File contents, or `None` when the key does not exist.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Create `key` only if it does not exist yet. Returns false when the key
    /// already exists; existing content is never overwritten.
    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<bool>;
    /// Unconditional overwrite, for the two mutable objects only: the manifest
    /// (key generation bumps) and the current generation's wrapped key (password
    /// changes). Segments, snapshots and older keys are immutable and always use
    /// `put_if_absent`. Local-folder writes are not atomic; a crash mid-write is
    /// recoverable because the engine still holds the key in memory.
    fn put(&self, key: &str, data: &[u8]) -> Result<()>;
    /// Delete `key`; missing keys are fine.
    fn delete(&self, key: &str) -> Result<()>;
}

pub(crate) fn check_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::InvalidInput("sync key must not be empty".into()));
    }
    for part in key.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(Error::InvalidInput(format!("invalid sync key: {key}")));
        }
    }
    Ok(())
}

pub(crate) fn check_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    check_key(prefix)
}

/// In-memory backend for tests and multi-device simulation.
pub struct MemorySyncBackend {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySyncBackend {
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemorySyncBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncBackend for MemorySyncBackend {
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        check_prefix(prefix)?;
        let objects = self.objects.lock().map_err(|_| lock_error())?;
        let mut keys: Vec<String> = objects
            .keys()
            .filter(|k| prefix.is_empty() || *k == prefix || k.starts_with(&format!("{prefix}/")))
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        let objects = self.objects.lock().map_err(|_| lock_error())?;
        Ok(objects.get(key).cloned())
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<bool> {
        check_key(key)?;
        let mut objects = self.objects.lock().map_err(|_| lock_error())?;
        use std::collections::hash_map::Entry;
        Ok(match objects.entry(key.to_string()) {
            Entry::Vacant(v) => {
                v.insert(data.to_vec());
                true
            }
            Entry::Occupied(_) => false,
        })
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        check_key(key)?;
        let mut objects = self.objects.lock().map_err(|_| lock_error())?;
        objects.insert(key.to_string(), data.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        let mut objects = self.objects.lock().map_err(|_| lock_error())?;
        objects.remove(key);
        Ok(())
    }
}

/// A local folder as a sync target. The same layout also makes iCloud Drive,
/// Dropbox and Syncthing folders usable: they just sync the files. Keys map to
/// paths under `root`; anything outside the root is rejected by [`check_key`],
/// and symlinks are never followed when listing.
pub struct LocalFolderBackend {
    root: PathBuf,
}

impl LocalFolderBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        let mut path = self.root.clone();
        for part in key.split('/') {
            path.push(part);
        }
        Ok(path)
    }
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error::Storage(format!("cannot {action} {}: {error}", path.display()))
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("list", dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error("list", dir, e))?;
        let file_type = entry.file_type().map_err(|e| io_error("list", dir, e))?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, root, out)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                Error::Storage(format!("path escapes sync root: {}", path.display()))
            })?;
            let mut key = String::new();
            for part in relative.components() {
                if !key.is_empty() {
                    key.push('/');
                }
                key.push_str(&part.as_os_str().to_string_lossy());
            }
            out.push(key);
        }
    }
    Ok(())
}

impl SyncBackend for LocalFolderBackend {
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        check_prefix(prefix)?;
        let base = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.path(prefix)?
        };
        let mut keys = Vec::new();
        if base.is_file() {
            keys.push(prefix.to_string());
        } else {
            walk(&base, &self.root, &mut keys)?;
        }
        keys.sort();
        Ok(keys)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read", &path, error)),
        }
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<bool> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error("create directory", parent, e))?;
        }
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(data)
                    .map_err(|e| io_error("write", &path, e))?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(io_error("create", &path, error)),
        }
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error("create directory", parent, e))?;
        }
        fs::write(&path, data).map_err(|e| io_error("write", &path, e))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("delete", &path, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_basics(backend: &dyn SyncBackend) {
        assert!(backend.list("").unwrap().is_empty());
        assert_eq!(backend.get("devices/a/seg-000001").unwrap(), None);

        assert!(
            backend
                .put_if_absent("devices/a/seg-000001", b"one")
                .unwrap()
        );
        assert!(
            !backend
                .put_if_absent("devices/a/seg-000001", b"two")
                .unwrap()
        );
        assert_eq!(
            backend.get("devices/a/seg-000001").unwrap().as_deref(),
            Some(b"one".as_slice())
        );

        assert!(
            backend
                .put_if_absent("devices/a/seg-000002", b"three")
                .unwrap()
        );
        assert!(
            backend
                .put_if_absent("devices/b/seg-000001", b"four")
                .unwrap()
        );
        assert_eq!(
            backend.list("devices/a").unwrap(),
            vec!["devices/a/seg-000001", "devices/a/seg-000002"]
        );
        assert_eq!(
            backend.list("").unwrap(),
            vec![
                "devices/a/seg-000001",
                "devices/a/seg-000002",
                "devices/b/seg-000001",
            ]
        );
        assert!(backend.list("devices/missing").unwrap().is_empty());

        // Unconditional overwrite replaces, and only replaces, the old content.
        backend.put("devices/a/seg-000002", b"replaced").unwrap();
        assert_eq!(
            backend.get("devices/a/seg-000002").unwrap().as_deref(),
            Some(b"replaced".as_slice())
        );

        backend.delete("devices/a/seg-000001").unwrap();
        backend.delete("devices/missing/seg-000001").unwrap();
        assert_eq!(
            backend.list("devices/a").unwrap(),
            vec!["devices/a/seg-000002"]
        );
    }

    fn check_bad_keys(backend: &dyn SyncBackend) {
        for key in ["", "a//b", "a/./b", "a/../b", "/a"] {
            assert!(backend.get(key).is_err(), "{key}");
            assert!(backend.put_if_absent(key, b"x").is_err(), "{key}");
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("fg-sync-backend-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn memory_backend_behaves() {
        let backend = MemorySyncBackend::new();
        check_basics(&backend);
        check_bad_keys(&backend);
    }

    #[test]
    fn local_folder_backend_behaves() {
        let root = temp_root("behaves");
        let backend = LocalFolderBackend::new(&root);
        check_basics(&backend);
        check_bad_keys(&backend);
        // Nested keys created their parent directories on disk.
        assert!(root.join("devices/a/seg-000002").is_file());
        let _ = fs::remove_dir_all(&root);
    }
}
