//! Sync backend credentials: S3 keys and the WebDAV login, stored as one
//! local-only record. The kind is deliberately absent from `kinds::SYNCED`,
//! so these values never leave the device: they are encrypted with the device
//! key at rest, never pushed, never exported. The sync password itself is
//! never stored anywhere; the user types it when joining or changing it, and
//! the unwrapped key lives only in service memory for the session.

use fg_core::Result;
use serde::{Deserialize, Serialize};

use crate::record::Loaded;
use crate::{Store, kinds};

/// Backend credentials for the configured sync target. Only the fields for
/// the active backend are used; the rest stay empty.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncCredentials {
    #[serde(default)]
    pub s3_access_key: Option<String>,
    #[serde(default)]
    pub s3_secret_key: Option<String>,
    #[serde(default)]
    pub webdav_username: Option<String>,
    #[serde(default)]
    pub webdav_password: Option<String>,
}

impl SyncCredentials {
    /// Whether any backend credential is saved (presence only; values are
    /// never read back to the UI).
    pub fn has_any(&self) -> bool {
        self.s3_access_key.is_some()
            || self.s3_secret_key.is_some()
            || self.webdav_username.is_some()
            || self.webdav_password.is_some()
    }
}

impl std::fmt::Debug for SyncCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let present = |value: &Option<String>| value.as_ref().map(|_| "[redacted]");
        f.debug_struct("SyncCredentials")
            .field("s3_access_key", &present(&self.s3_access_key))
            .field("s3_secret_key", &present(&self.s3_secret_key))
            .field("webdav_username", &present(&self.webdav_username))
            .field("webdav_password", &present(&self.webdav_password))
            .finish()
    }
}

impl Store {
    /// The saved backend credentials, or empty when none were saved.
    pub fn sync_credentials(&self) -> Result<SyncCredentials> {
        Ok(self
            .get_json::<SyncCredentials>(kinds::SYNC_CREDENTIAL, "default")?
            .and_then(Loaded::ok)
            .unwrap_or_default())
    }

    /// Save backend credentials. They stay on this device.
    pub fn set_sync_credentials(&self, credentials: &SyncCredentials) -> Result<()> {
        self.put_json(kinds::SYNC_CREDENTIAL, "default", None, None, credentials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::store;

    #[test]
    fn credentials_roundtrip_and_stay_local() {
        let s = store();
        assert_eq!(s.sync_credentials().unwrap(), SyncCredentials::default());
        let creds = SyncCredentials {
            s3_access_key: Some("AKID".into()),
            s3_secret_key: Some("secret".into()),
            webdav_username: None,
            webdav_password: None,
        };
        s.set_sync_credentials(&creds).unwrap();
        assert_eq!(s.sync_credentials().unwrap(), creds);
        // Local-only: nothing dirty is ever pushed for this kind, and the
        // snapshot reader (which only covers synced kinds) never sees it.
        assert!(s.dirty_records(100).unwrap().is_empty());
        assert!(s.synced_records().unwrap().is_empty());
        // And exports never include it.
        let bundle = s
            .export_bundle(
                &crate::ExportOptions {
                    scope: crate::ExportScope::Everything,
                    include_secrets: true,
                },
                "pw",
            )
            .unwrap();
        assert!(!bundle.is_empty());
    }
}
