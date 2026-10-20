use std::collections::HashMap;
use std::sync::Mutex;

use fg_core::{Error, Result};
use fg_crypto::SecretKey;

/// Where the device key lives. In the app this is the OS keychain (service `fastgrid`, account
/// `device-key:<store_id>`), implemented in `fg-service`; the store itself never touches it.
///
/// The key is looked up by `store_id`, so a store file copied to another machine does not silently
/// pick up an unrelated key.
pub trait DeviceKeyProvider {
    fn load(&self, store_id: &str) -> Result<Option<SecretKey>>;
    fn save(&self, store_id: &str, key: &SecretKey) -> Result<()>;
}

/// In-memory provider for tests and tools. Keys are lost when it is dropped.
#[derive(Default)]
pub struct MemoryKeyProvider {
    keys: Mutex<HashMap<String, SecretKey>>,
}

impl MemoryKeyProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_key(store_id: &str, key: SecretKey) -> Self {
        let p = Self::new();
        if let Ok(mut keys) = p.keys.lock() {
            keys.insert(store_id.to_owned(), key);
        }
        p
    }
}

impl DeviceKeyProvider for MemoryKeyProvider {
    fn load(&self, store_id: &str) -> Result<Option<SecretKey>> {
        let keys = self
            .keys
            .lock()
            .map_err(|_| Error::Other("key provider poisoned".into()))?;
        Ok(keys.get(store_id).cloned())
    }

    fn save(&self, store_id: &str, key: &SecretKey) -> Result<()> {
        let mut keys = self
            .keys
            .lock()
            .map_err(|_| Error::Other("key provider poisoned".into()))?;
        keys.insert(store_id.to_owned(), key.clone());
        Ok(())
    }
}
