use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ConnectionId;

/// Secrets belonging to a connection. They are stored like any other record: sealed with the
/// device key (see `docs/encryption-and-sync.md`) and synced with the rest. The OS keychain holds
/// only the device key itself, never individual passwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SecretKind {
    DatabasePassword,
    SshPassword,
    SshKeyPassphrase,
    TlsKeyPassphrase,
    #[serde(other)]
    Unknown,
}

impl SecretKind {
    /// Stable name used in record ids. Changing it orphans stored secrets.
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::DatabasePassword => "password",
            SecretKind::SshPassword => "ssh_password",
            SecretKind::SshKeyPassphrase => "ssh_key_passphrase",
            SecretKind::TlsKeyPassphrase => "tls_key_passphrase",
            SecretKind::Unknown => "unknown",
        }
    }

    /// Record id of this secret: unique per connection and kind.
    pub fn record_id(self, connection: &ConnectionId) -> String {
        format!("{connection}:{}", self.as_str())
    }
}

/// One stored secret value. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRecord {
    pub connection_id: ConnectionId,
    pub kind: SecretKind,
    pub value: String,
}

impl fmt::Debug for SecretRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretRecord")
            .field("connection_id", &self.connection_id)
            .field("kind", &self.kind)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_ids_are_stable() {
        let id = ConnectionId::from_string("abc");
        assert_eq!(SecretKind::DatabasePassword.record_id(&id), "abc:password");
        assert_eq!(
            SecretKind::SshKeyPassphrase.record_id(&id),
            "abc:ssh_key_passphrase"
        );
    }

    #[test]
    fn debug_hides_the_value() {
        let s = SecretRecord {
            connection_id: ConnectionId::from_string("c"),
            kind: SecretKind::DatabasePassword,
            value: "hunter2".into(),
        };
        assert!(!format!("{s:?}").contains("hunter2"));
    }
}
