//! The device key lives in the OS credential store: macOS Keychain, Windows Credential Manager,
//! or the Secret Service on Linux. Nothing else is stored there (see
//! `docs/encryption-and-sync.md`).

use std::collections::HashMap;
use std::sync::Mutex;

use fg_core::{Error, Result};
use fg_crypto::SecretKey;
use fg_store::DeviceKeyProvider;

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
}
