//! The sync key: one random key per vault, wrapped by the sync password.
//!
//! The wrapped key lives in the remote `keys/` directory (see `sync.rs`). Changing
//! the sync password only re-wraps the key, it never re-encrypts the remote data;
//! a new device unwraps the key with the password and can read everything. The
//! wrapping uses the same password seal as encrypted backups, so a wrapped key
//! opens on any device that knows the password, independent of device keys.

use fg_core::{Error, Result};
use fg_crypto::{CryptoError, KdfParams, SecretKey, password_open, password_seal_with};

/// Context authenticated into every wrapped sync key. Changing it invalidates
/// keys wrapped under an older context, so it only ever changes with a format bump.
pub const SYNC_KEY_CONTEXT: &[u8] = b"fastgrid-sync-key-v1";

fn kdf() -> Result<KdfParams> {
    // Unit tests use a cheap KDF; shipped builds always use the recommended cost.
    let params = if cfg!(test) {
        KdfParams::with_cost(8, 1, 1)
    } else {
        KdfParams::recommended()
    };
    params.map_err(|e| Error::Storage(e.to_string()))
}

/// A fresh random sync key for a new vault.
pub fn generate_sync_key() -> Result<SecretKey> {
    SecretKey::generate().map_err(|e| Error::Storage(e.to_string()))
}

/// Wrap `key` under `password`. An empty password is refused: a vault with no
/// password would be open to anyone who can read the remote.
pub fn wrap_sync_key(password: &str, key: &SecretKey) -> Result<Vec<u8>> {
    if password.is_empty() {
        return Err(Error::Other("a sync password is required".into()));
    }
    password_seal_with(&kdf()?, password, SYNC_KEY_CONTEXT, key.as_bytes())
        .map_err(|e| Error::Storage(e.to_string()))
}

/// Unwrap a key wrapped by [`wrap_sync_key`]. A wrong password and a damaged
/// blob are told apart from a key wrapped by a newer version.
pub fn unwrap_sync_key(password: &str, blob: &[u8]) -> Result<SecretKey> {
    let bytes = password_open(password, SYNC_KEY_CONTEXT, blob).map_err(|e| match e {
        CryptoError::Decrypt => {
            Error::Other("wrong sync password, or the wrapped key is damaged".into())
        }
        CryptoError::Format(_) => Error::Other("not a fastgrid sync key".into()),
        CryptoError::UnsupportedVersion(_) => {
            Error::Unsupported("this sync key was wrapped by a newer version of fastgrid".into())
        }
        other => Error::Storage(other.to_string()),
    })?;
    SecretKey::try_from_slice(&bytes)
        .map_err(|_| Error::Storage("wrapped sync key has the wrong length".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_roundtrip() {
        let key = generate_sync_key().unwrap();
        let blob = wrap_sync_key("correct horse", &key).unwrap();
        let back = unwrap_sync_key("correct horse", &blob).unwrap();
        assert_eq!(back.as_bytes(), key.as_bytes());
    }

    #[test]
    fn wrong_password_and_damage_are_refused() {
        let key = generate_sync_key().unwrap();
        let blob = wrap_sync_key("pw-1", &key).unwrap();
        let error = unwrap_sync_key("pw-2", &blob).unwrap_err().to_string();
        assert!(error.contains("wrong sync password"), "{error}");

        let mut damaged = blob.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        assert!(unwrap_sync_key("pw-1", &damaged).is_err());
        assert!(unwrap_sync_key("pw-1", b"not a wrapped key").is_err());
    }

    #[test]
    fn empty_password_is_refused() {
        let key = generate_sync_key().unwrap();
        assert!(wrap_sync_key("", &key).is_err());
    }
}
