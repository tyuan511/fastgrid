//! The device key lives in the OS credential store: macOS Keychain, Windows Credential Manager,
//! or the Secret Service on Linux. Nothing else is stored there (see
//! `docs/encryption-and-sync.md`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fg_core::{Error, Result};
use fg_crypto::SecretKey;
use fg_store::DeviceKeyProvider;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "fastgrid";

/// Credential entry name. Stable forever: changing it would orphan every existing store.
pub fn account_name(store_id: &str) -> String {
    format!("device-key:{store_id}")
}

/// Raw access to a credential store; the seam that lets tests run without touching the real
/// keychain (which can prompt the user or be absent on CI).
pub trait SecretBackend: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()>;
}

/// The real thing, via the `keyring` crate.
#[derive(Default)]
pub struct OsKeychain;

fn keyring_error(e: keyring::Error) -> Error {
    Error::KeyUnavailable(format!("the system keychain is not usable: {e}"))
}

impl SecretBackend for OsKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        let entry = keyring::Entry::new(service, account).map_err(keyring_error)?;
        match entry.get_secret() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error(e)),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
        keyring::Entry::new(service, account)
            .and_then(|e| e.set_secret(secret))
            .map_err(keyring_error)
    }
}

/// In-memory backend for tests and tools.
#[derive(Default)]
pub struct MemoryBackend {
    items: Mutex<HashMap<(String, String), Vec<u8>>>,
}

impl SecretBackend for MemoryBackend {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        let items = self
            .items
            .lock()
            .map_err(|_| Error::Other("memory backend poisoned".into()))?;
        Ok(items
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
        let mut items = self
            .items
            .lock()
            .map_err(|_| Error::Other("memory backend poisoned".into()))?;
        items.insert((service.to_owned(), account.to_owned()), secret.to_vec());
        Ok(())
    }
}

impl SecretBackend for Box<dyn SecretBackend> {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        (**self).get(service, account)
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
        (**self).set(service, account, secret)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct DevKeyringFile {
    #[serde(default)]
    entries: HashMap<String, HashMap<String, String>>,
}

type SecretMap = HashMap<(String, String), Vec<u8>>;

/// File-backed secret store for development environments.
///
/// Avoids repeated OS keychain permission prompts during debug builds while still persisting
/// keys across application restarts. Secrets are stored in a local JSON file (mode 0600 on Unix)
/// with an optional fallback backend (e.g. [`OsKeychain`]) used to migrate existing keys.
pub struct FileBackend {
    path: PathBuf,
    fallback: Option<Box<dyn SecretBackend>>,
    items: Mutex<Option<SecretMap>>,
}

impl FileBackend {
    /// Creates a file backend at `path` without a fallback.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            fallback: None,
            items: Mutex::new(None),
        }
    }

    /// Creates a file backend at `path` that falls back to `fallback` if a requested secret is
    /// missing, caching it into the file once read.
    pub fn with_fallback(path: impl Into<PathBuf>, fallback: impl SecretBackend + 'static) -> Self {
        Self {
            path: path.into(),
            fallback: Some(Box::new(fallback)),
            items: Mutex::new(None),
        }
    }

    /// Path to the keyring file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this backend has a fallback configured.
    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    fn read_from_disk(&self) -> Result<SecretMap> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let content = std::fs::read(&self.path).map_err(|e| {
            Error::Storage(format!(
                "cannot read keyring file {}: {e}",
                self.path.display()
            ))
        })?;
        if content.is_empty() {
            return Ok(HashMap::new());
        }
        let data: DevKeyringFile = serde_json::from_slice(&content).map_err(|e| {
            Error::KeyUnavailable(format!(
                "dev keyring file {} corrupted: {e}",
                self.path.display()
            ))
        })?;
        let mut map = HashMap::new();
        for (service, accounts) in data.entries {
            for (account, hex_secret) in accounts {
                if let Some(secret) = decode_hex(&hex_secret) {
                    map.insert((service.clone(), account), secret);
                }
            }
        }
        Ok(map)
    }

    fn write_to_disk(&self, map: &SecretMap) -> Result<()> {
        let mut entries: HashMap<String, HashMap<String, String>> = HashMap::new();
        for ((service, account), secret) in map {
            entries
                .entry(service.clone())
                .or_default()
                .insert(account.clone(), encode_hex(secret));
        }
        let data = DevKeyringFile { entries };
        let json = serde_json::to_vec_pretty(&data)
            .map_err(|e| Error::Other(format!("cannot serialize dev keyring: {e}")))?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Storage(format!("cannot create directory {}: {e}", parent.display()))
            })?;
        }

        let tmp_path = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| {
                Error::Storage(format!(
                    "cannot open temp keyring file {}: {e}",
                    tmp_path.display()
                ))
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }

        use std::io::Write as _;
        file.write_all(&json).map_err(|e| {
            Error::Storage(format!(
                "cannot write temp keyring file {}: {e}",
                tmp_path.display()
            ))
        })?;
        file.flush().map_err(|e| {
            Error::Storage(format!(
                "cannot flush temp keyring file {}: {e}",
                tmp_path.display()
            ))
        })?;
        drop(file);

        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            Error::Storage(format!(
                "cannot replace keyring file {}: {e}",
                self.path.display()
            ))
        })?;

        Ok(())
    }

    fn write_entry(
        &self,
        service: &str,
        account: &str,
        secret: &[u8],
        guard: &mut Option<SecretMap>,
    ) -> Result<()> {
        let mut map = self.read_from_disk()?;
        map.insert((service.to_string(), account.to_string()), secret.to_vec());
        self.write_to_disk(&map)?;
        *guard = Some(map);
        Ok(())
    }
}

impl SecretBackend for FileBackend {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        let key = (service.to_string(), account.to_string());
        let mut guard = self
            .items
            .lock()
            .map_err(|_| Error::Other("file backend mutex poisoned".into()))?;

        if guard.is_none() {
            *guard = Some(self.read_from_disk()?);
        }

        if let Some(val) = guard.as_ref().and_then(|m| m.get(&key)) {
            return Ok(Some(val.clone()));
        }

        // Re-read from disk in case another process wrote it
        let fresh_map = self.read_from_disk()?;
        if let Some(val) = fresh_map.get(&key) {
            let cloned = val.clone();
            *guard = Some(fresh_map);
            return Ok(Some(cloned));
        }
        *guard = Some(fresh_map);

        if let Some(ref fallback) = self.fallback {
            match fallback.get(service, account) {
                Ok(Some(secret)) => {
                    self.write_entry(service, account, &secret, &mut guard)?;
                    Ok(Some(secret))
                }
                Ok(None) => Ok(None),
                Err(e) => {
                    tracing::warn!("fallback secret backend failed for {service}/{account}: {e}");
                    Err(e)
                }
            }
        } else {
            Ok(None)
        }
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
        let mut guard = self
            .items
            .lock()
            .map_err(|_| Error::Other("file backend mutex poisoned".into()))?;
        self.write_entry(service, account, secret, &mut guard)
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().as_chunks::<2>().0 {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// [`DeviceKeyProvider`] on top of a [`SecretBackend`]; the app uses [`OsKeychain`].
pub struct KeychainKeyProvider<B: SecretBackend = OsKeychain> {
    backend: B,
}

impl KeychainKeyProvider<OsKeychain> {
    pub fn os() -> Self {
        Self {
            backend: OsKeychain,
        }
    }
}

impl KeychainKeyProvider<FileBackend> {
    /// File-backed provider at `path`.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            backend: FileBackend::new(path),
        }
    }

    /// File-backed provider at `path` with fallback to `fallback`.
    pub fn file_with_fallback(
        path: impl Into<PathBuf>,
        fallback: impl SecretBackend + 'static,
    ) -> Self {
        Self {
            backend: FileBackend::with_fallback(path, fallback),
        }
    }
}

impl<B: SecretBackend> KeychainKeyProvider<B> {
    pub fn with_backend(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: SecretBackend> DeviceKeyProvider for KeychainKeyProvider<B> {
    fn load(&self, store_id: &str) -> Result<Option<SecretKey>> {
        match self.backend.get(SERVICE, &account_name(store_id))? {
            None => Ok(None),
            Some(bytes) => SecretKey::try_from_slice(&bytes).map(Some).map_err(|_| {
                Error::KeyUnavailable("the keychain entry for this store is damaged".into())
            }),
        }
    }

    fn save(&self, store_id: &str, key: &SecretKey) -> Result<()> {
        self.backend
            .set(SERVICE, &account_name(store_id), key.as_bytes())
    }
}

/// Default filename for the dev keyring file within `data_dir`.
pub const DEV_KEYRING_FILE: &str = ".dev-keyring.json";

/// Returns the default key provider for the application based on environment and build configuration.
///
/// Priority:
/// 1. `FASTGRID_KEYCHAIN=memory` or `FASTGRID_MEMORY_KEYCHAIN=1`: [`MemoryBackend`] (ephemeral).
/// 2. `FASTGRID_KEYCHAIN=os` or `FASTGRID_USE_OS_KEYCHAIN=1`: [`OsKeychain`].
/// 3. `FASTGRID_KEYCHAIN=file`: [`FileBackend`] (file only, no OS keychain fallback).
/// 4. `FASTGRID_KEYCHAIN=dev` or `FASTGRID_DEV=1` or `FASTGRID_DEV_KEYCHAIN=1`:
///    [`FileBackend`] with [`OsKeychain`] fallback.
/// 5. Default:
///    - In debug builds (`cfg!(debug_assertions)`): [`FileBackend`] with [`OsKeychain`] fallback.
///    - In release builds: [`OsKeychain`].
pub fn default_key_provider(data_dir: &Path) -> Box<dyn DeviceKeyProvider> {
    if let Ok(val) = std::env::var("FASTGRID_KEYCHAIN") {
        match val.to_lowercase().as_str() {
            "memory" => {
                return Box::new(KeychainKeyProvider::with_backend(MemoryBackend::default()));
            }
            "os" => {
                return Box::new(KeychainKeyProvider::os());
            }
            "file" => {
                return Box::new(KeychainKeyProvider::file(data_dir.join(DEV_KEYRING_FILE)));
            }
            "dev" => {
                return Box::new(KeychainKeyProvider::file_with_fallback(
                    data_dir.join(DEV_KEYRING_FILE),
                    OsKeychain,
                ));
            }
            other => {
                tracing::warn!("unknown FASTGRID_KEYCHAIN={other}, using default provider");
            }
        }
    }

    if std::env::var_os("FASTGRID_MEMORY_KEYCHAIN").is_some() {
        return Box::new(KeychainKeyProvider::with_backend(MemoryBackend::default()));
    }

    if std::env::var_os("FASTGRID_USE_OS_KEYCHAIN").is_some() {
        return Box::new(KeychainKeyProvider::os());
    }

    let dev_flag = std::env::var_os("FASTGRID_DEV").is_some()
        || std::env::var_os("FASTGRID_DEV_KEYCHAIN").is_some();

    if cfg!(debug_assertions) || dev_flag {
        Box::new(KeychainKeyProvider::file_with_fallback(
            data_dir.join(DEV_KEYRING_FILE),
            OsKeychain,
        ))
    } else {
        Box::new(KeychainKeyProvider::os())
    }
}

#[cfg(test)]
mod tests {
    use fg_store::Store;

    use super::*;

    #[test]
    fn account_names_are_stable() {
        assert_eq!(account_name("abc"), "device-key:abc");
    }

    #[test]
    fn keys_roundtrip_and_missing_is_none() {
        let p = KeychainKeyProvider::with_backend(MemoryBackend::default());
        assert!(p.load("s").unwrap().is_none());
        let key = SecretKey::generate().unwrap();
        p.save("s", &key).unwrap();
        assert_eq!(p.load("s").unwrap().unwrap(), key);
        assert!(p.load("other").unwrap().is_none(), "keys are per store");
    }

    #[test]
    fn a_damaged_entry_is_reported_not_replaced() {
        let backend = MemoryBackend::default();
        backend
            .set(SERVICE, &account_name("s"), b"too short")
            .unwrap();
        let p = KeychainKeyProvider::with_backend(backend);
        assert!(matches!(p.load("s"), Err(Error::KeyUnavailable(_))));
    }

    #[test]
    fn a_store_can_be_created_and_reopened_through_the_keychain() {
        let dir = std::env::temp_dir().join(format!("fg-keychain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.db");
        let p = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let id = Store::open(&path, &p).unwrap().store_id().to_owned();
        assert!(p.load(&id).unwrap().is_some(), "the new key was saved");
        assert!(Store::open(&path, &p).is_ok());
        // A different keychain (new machine / wiped) cannot open it.
        let other = KeychainKeyProvider::with_backend(MemoryBackend::default());
        assert!(matches!(
            Store::open(&path, &other).err(),
            Some(Error::KeyUnavailable(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Talks to the real OS keychain; may show a permission prompt. Run by hand on each OS:
    /// `cargo test -p fg-service -- --ignored os_keychain`
    #[test]
    #[ignore = "uses the real OS keychain"]
    fn os_keychain_roundtrip() {
        let p = KeychainKeyProvider::os();
        let id = format!("test-{}", std::process::id());
        let key = SecretKey::generate().unwrap();
        p.save(&id, &key).unwrap();
        assert_eq!(p.load(&id).unwrap().unwrap(), key);
    }

    #[test]
    fn file_backend_roundtrip_and_persistence() {
        let dir = std::env::temp_dir().join(format!("fg-file-backend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(DEV_KEYRING_FILE);

        let b1 = FileBackend::new(&path);
        assert_eq!(b1.get("fastgrid", "acc").unwrap(), None);
        b1.set("fastgrid", "acc", b"secret-123").unwrap();
        assert_eq!(
            b1.get("fastgrid", "acc").unwrap(),
            Some(b"secret-123".to_vec())
        );

        // Separate instance loads the persisted key from disk
        let b2 = FileBackend::new(&path);
        assert_eq!(
            b2.get("fastgrid", "acc").unwrap(),
            Some(b"secret-123".to_vec())
        );
        assert_eq!(b2.get("fastgrid", "other").unwrap(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_backend_fallback_and_caching() {
        let dir = std::env::temp_dir().join(format!("fg-file-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(DEV_KEYRING_FILE);

        let fallback = MemoryBackend::default();
        fallback
            .set("fastgrid", "migrated-key", b"from-os-keychain")
            .unwrap();

        let backend = FileBackend::with_fallback(&path, fallback);
        // Entry is not in file initially; read falls back and caches it into file
        assert_eq!(
            backend.get("fastgrid", "migrated-key").unwrap(),
            Some(b"from-os-keychain".to_vec())
        );

        // A new instance without fallback can now read it directly from the file
        let independent = FileBackend::new(&path);
        assert_eq!(
            independent.get("fastgrid", "migrated-key").unwrap(),
            Some(b"from-os-keychain".to_vec())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_backend_damaged_file_errors() {
        let dir = std::env::temp_dir().join(format!("fg-file-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DEV_KEYRING_FILE);
        std::fs::write(&path, b"not valid json").unwrap();

        let backend = FileBackend::new(&path);
        assert!(matches!(
            backend.get("fastgrid", "acc"),
            Err(Error::KeyUnavailable(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_backend_store_open_and_reopen() {
        let dir = std::env::temp_dir().join(format!("fg-store-dev-keyring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store_path = dir.join("store.db");
        let keyring_path = dir.join(DEV_KEYRING_FILE);

        let p1 = KeychainKeyProvider::file(&keyring_path);
        let store = Store::open(&store_path, &p1).unwrap();
        let id = store.store_id().to_owned();
        drop(store);

        // Reopen with fresh provider instance pointing to the same dev keyring
        let p2 = KeychainKeyProvider::file(&keyring_path);
        let reopened = Store::open(&store_path, &p2).unwrap();
        assert_eq!(reopened.store_id(), id);
        drop(reopened);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
