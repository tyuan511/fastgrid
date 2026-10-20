use fg_core::{ConnectionId, Result, SecretKind, SecretRecord};

use crate::record::Loaded;
use crate::{Store, kinds};

impl Store {
    /// Store (or replace) a connection's secret. Sealed like every other record; the OS keychain
    /// only ever holds the device key.
    pub fn set_secret(
        &self,
        connection: &ConnectionId,
        kind: SecretKind,
        value: &str,
    ) -> Result<()> {
        let record = SecretRecord {
            connection_id: connection.clone(),
            kind,
            value: value.to_owned(),
        };
        self.put_json(
            kinds::SECRET,
            &kind.record_id(connection),
            Some(connection.as_str()),
            None,
            &record,
        )
    }

    pub fn get_secret(
        &self,
        connection: &ConnectionId,
        kind: SecretKind,
    ) -> Result<Option<String>> {
        Ok(self
            .get_json::<SecretRecord>(kinds::SECRET, &kind.record_id(connection))?
            .and_then(Loaded::ok)
            .map(|r| r.value))
    }

    pub fn delete_secret(&self, connection: &ConnectionId, kind: SecretKind) -> Result<bool> {
        self.tombstone(kinds::SECRET, &kind.record_id(connection))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::store;

    #[test]
    fn set_get_replace_delete() {
        let s = store();
        let c = ConnectionId::new();
        assert_eq!(
            s.get_secret(&c, SecretKind::DatabasePassword).unwrap(),
            None
        );
        s.set_secret(&c, SecretKind::DatabasePassword, "one")
            .unwrap();
        s.set_secret(&c, SecretKind::SshPassword, "ssh").unwrap();
        s.set_secret(&c, SecretKind::DatabasePassword, "two")
            .unwrap();
        assert_eq!(
            s.get_secret(&c, SecretKind::DatabasePassword)
                .unwrap()
                .as_deref(),
            Some("two")
        );
        assert_eq!(
            s.get_secret(&c, SecretKind::SshPassword)
                .unwrap()
                .as_deref(),
            Some("ssh")
        );
        assert!(s.delete_secret(&c, SecretKind::DatabasePassword).unwrap());
        assert_eq!(
            s.get_secret(&c, SecretKind::DatabasePassword).unwrap(),
            None
        );
    }
}
