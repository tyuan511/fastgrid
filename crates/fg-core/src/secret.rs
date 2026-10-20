use crate::ConnectionId;

/// Secrets live only in the OS keychain, never in the store, exports or logs.
/// The store records *that* a secret may exist (via the config), not its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    DatabasePassword,
    SshPassword,
    SshKeyPassphrase,
    TlsKeyPassphrase,
}

impl SecretKind {
    pub const SERVICE: &'static str = "fastgrid";

    /// Keychain account name. Stable forever: changing it orphans stored secrets.
    pub fn account(self, connection: &ConnectionId) -> String {
        let suffix = match self {
            SecretKind::DatabasePassword => "password",
            SecretKind::SshPassword => "ssh_password",
            SecretKind::SshKeyPassphrase => "ssh_key_passphrase",
            SecretKind::TlsKeyPassphrase => "tls_key_passphrase",
        };
        format!("conn:{connection}:{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_are_stable() {
        let id = ConnectionId::from_string("abc");
        assert_eq!(
            SecretKind::DatabasePassword.account(&id),
            "conn:abc:password"
        );
        assert_eq!(
            SecretKind::SshKeyPassphrase.account(&id),
            "conn:abc:ssh_key_passphrase"
        );
    }
}
