use fg_core::{
    ConnectionConfig, ConnectionGroup, ConnectionId, GroupId, Result, SecretKind, SecretRecord,
    UnixMillis,
};

use crate::Store;
use crate::kinds;
use crate::record::Loaded;

/// A connection as found in the store. A record that cannot be read (written by a newer version,
/// or damaged) is kept and surfaced so the UI can say why; it is never silently dropped.
// A connection list holds tens of entries, so boxing the config would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum StoredConnection {
    Ok(ConnectionConfig),
    Unreadable { id: ConnectionId, reason: String },
}

impl StoredConnection {
    pub fn id(&self) -> &ConnectionId {
        match self {
            StoredConnection::Ok(c) => &c.id,
            StoredConnection::Unreadable { id, .. } => id,
        }
    }
}

/// How [`Store::save_connection_with_password`] treats the database password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionPassword {
    Keep,
    Set(String),
    Clear,
}

/// Records that belong to a connection and go away with it. Audit entries deliberately do not:
/// they must outlive the connection.
const OWNED_BY_CONNECTION: &[&str] = &[
    kinds::SECRET,
    kinds::SNIPPET,
    kinds::DOCUMENT,
    kinds::HISTORY,
    kinds::WORKSPACE,
    kinds::TABLE_VIEW,
];

impl Store {
    /// Insert or update. Sets `created_at` on first save and `updated_at` every time; returns the
    /// stored version.
    pub fn save_connection(&self, c: ConnectionConfig) -> Result<ConnectionConfig> {
        self.save_connection_with_password(c, ConnectionPassword::Keep)
    }

    /// Save the connection and its password change in one transaction.
    ///
    /// A failure of the password step rolls the config write back, so a stricter policy cannot
    /// land on disk without the password that belongs to it.
    pub fn save_connection_with_password(
        &self,
        mut c: ConnectionConfig,
        password: ConnectionPassword,
    ) -> Result<ConnectionConfig> {
        let now = UnixMillis::now();
        c.created_at.get_or_insert(now);
        c.updated_at = Some(now);
        let config_enc = self.seal_value(kinds::CONNECTION, c.id.as_str(), &c)?;
        let secret_id = SecretKind::DatabasePassword.record_id(&c.id);
        let secret_enc = match &password {
            ConnectionPassword::Keep | ConnectionPassword::Clear => None,
            ConnectionPassword::Set(value) => {
                let record = SecretRecord {
                    connection_id: c.id.clone(),
                    kind: SecretKind::DatabasePassword,
                    value: value.clone(),
                };
                Some(self.seal_value(kinds::SECRET, &secret_id, &record)?)
            }
        };
        let id = c.id.clone();
        self.with_write_tx(|tx| {
            Self::replace_row(
                tx,
                &crate::SealedRow {
                    kind: kinds::CONNECTION,
                    id: id.as_str(),
                    parent_id: None,
                    sort_ts: Some(now.0),
                    device_id: &self.device_id,
                    key_id: self.key_id,
                    enc: &config_enc,
                },
            )?;
            match &password {
                ConnectionPassword::Keep => Ok(()),
                ConnectionPassword::Set(_) => {
                    let enc = secret_enc.as_deref().ok_or_else(|| {
                        fg_core::Error::Storage("password payload was not sealed".into())
                    })?;
                    Self::replace_row(
                        tx,
                        &crate::SealedRow {
                            kind: kinds::SECRET,
                            id: &secret_id,
                            parent_id: Some(id.as_str()),
                            sort_ts: None,
                            device_id: &self.device_id,
                            key_id: self.key_id,
                            enc,
                        },
                    )
                }
                ConnectionPassword::Clear => {
                    Self::tombstone_tx(tx, kinds::SECRET, &secret_id, &self.device_id, self.key_id)
                }
            }
        })?;
        Ok(c)
    }

    /// Sorted by `sort_order`, then name. Sorting happens here because names are encrypted.
    pub fn load_connections(&self) -> Result<Vec<StoredConnection>> {
        let mut all = self.list_json::<ConnectionConfig>(kinds::CONNECTION, None, None)?;
        all.sort_by(|a, b| match (a, b) {
            (Loaded::Ok(a), Loaded::Ok(b)) => {
                (a.sort_order, &a.name, &a.id).cmp(&(b.sort_order, &b.name, &b.id))
            }
            (Loaded::Ok(_), Loaded::Unreadable { .. }) => std::cmp::Ordering::Less,
            (Loaded::Unreadable { .. }, Loaded::Ok(_)) => std::cmp::Ordering::Greater,
            (Loaded::Unreadable { id: a, .. }, Loaded::Unreadable { id: b, .. }) => a.cmp(b),
        });
        Ok(all
            .into_iter()
            .map(|l| match l {
                Loaded::Ok(c) => StoredConnection::Ok(c),
                Loaded::Unreadable { id, reason, .. } => StoredConnection::Unreadable {
                    id: ConnectionId::from_string(id),
                    reason,
                },
            })
            .collect())
    }

    pub fn get_connection(&self, id: &ConnectionId) -> Result<Option<StoredConnection>> {
        Ok(self
            .get_json::<ConnectionConfig>(kinds::CONNECTION, id.as_str())?
            .map(|l| match l {
                Loaded::Ok(c) => StoredConnection::Ok(c),
                Loaded::Unreadable { id, reason, .. } => StoredConnection::Unreadable {
                    id: ConnectionId::from_string(id),
                    reason,
                },
            }))
    }

    /// Delete a connection together with its secrets, history, drafts and saved UI state.
    /// Audit entries are kept.
    pub fn delete_connection(&self, id: &ConnectionId) -> Result<bool> {
        let existed = self.tombstone(kinds::CONNECTION, id.as_str())?;
        if existed {
            self.tombstone_children(OWNED_BY_CONNECTION, id.as_str())?;
        }
        Ok(existed)
    }

    /// Record that the connection was just opened.
    pub fn touch_connection(&self, id: &ConnectionId) -> Result<()> {
        if let Some(StoredConnection::Ok(mut c)) = self.get_connection(id)? {
            c.last_used_at = Some(UnixMillis::now());
            self.save_connection(c)?;
        }
        Ok(())
    }

    pub fn save_group(&self, mut g: ConnectionGroup) -> Result<ConnectionGroup> {
        let now = UnixMillis::now();
        g.created_at.get_or_insert(now);
        g.updated_at = Some(now);
        self.put_json(kinds::GROUP, g.id.as_str(), None, Some(now.0), &g)?;
        Ok(g)
    }

    pub fn load_groups(&self) -> Result<Vec<ConnectionGroup>> {
        let mut groups: Vec<_> = self
            .list_json::<ConnectionGroup>(kinds::GROUP, None, None)?
            .into_iter()
            .filter_map(Loaded::ok)
            .collect();
        groups.sort_by(|a, b| (a.sort_order, &a.name, &a.id).cmp(&(b.sort_order, &b.name, &b.id)));
        Ok(groups)
    }

    /// Delete a group. Its connections and child groups move to the top level.
    pub fn delete_group(&self, id: &GroupId) -> Result<bool> {
        for stored in self.load_connections()? {
            if let StoredConnection::Ok(mut c) = stored
                && c.group_id.as_ref() == Some(id)
            {
                c.group_id = None;
                self.save_connection(c)?;
            }
        }
        for mut g in self.load_groups()? {
            if g.parent_id.as_ref() == Some(id) {
                g.parent_id = None;
                self.save_group(g)?;
            }
        }
        self.tombstone(kinds::GROUP, id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{ConnectionParams, DbKind, Environment, SecretKind};

    use super::*;
    use crate::testutil::store;

    fn sample(name: &str) -> ConnectionConfig {
        ConnectionConfig::new(
            name,
            DbKind::Sqlite,
            ConnectionParams::File {
                path: "a.db".into(),
            },
        )
    }

    #[test]
    fn save_load_roundtrip_sets_timestamps_and_sorts_by_name() {
        let s = store();
        let mut c = sample("zeta");
        c.environment = Environment::Production;
        c.startup_sql.push("PRAGMA foreign_keys = ON".into());
        let saved = s.save_connection(c).unwrap();
        assert!(saved.created_at.is_some() && saved.updated_at.is_some());
        s.save_connection(sample("alpha")).unwrap();

        let names: Vec<_> = s
            .load_connections()
            .unwrap()
            .into_iter()
            .filter_map(|c| match c {
                StoredConnection::Ok(c) => Some(c.name),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);

        let mut changed = saved.clone();
        changed.name = "renamed".into();
        let again = s.save_connection(changed).unwrap();
        assert_eq!(again.created_at, saved.created_at);
        assert_eq!(s.load_connections().unwrap().len(), 2);
    }

    #[test]
    fn unreadable_records_are_kept_and_reported() {
        let s = store();
        s.put_json(
            kinds::CONNECTION,
            "x",
            None,
            None,
            &serde_json::json!({"id": "x", "params": {"type": "warp"}}),
        )
        .unwrap();
        s.save_connection(sample("other")).unwrap();
        let all = s.load_connections().unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            matches!(all[1], StoredConnection::Unreadable { .. }),
            "unreadable sorts last"
        );
    }

    #[test]
    fn groups_and_membership() {
        let s = store();
        let g = s.save_group(ConnectionGroup::new("Work")).unwrap();
        let mut c = sample("db");
        c.group_id = Some(g.id.clone());
        let c = s.save_connection(c).unwrap();

        assert_eq!(s.load_groups().unwrap(), vec![g.clone()]);
        assert!(s.delete_group(&g.id).unwrap());
        assert!(s.load_groups().unwrap().is_empty());
        match s.get_connection(&c.id).unwrap().unwrap() {
            StoredConnection::Ok(loaded) => assert_eq!(loaded.group_id, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn touch_records_last_use() {
        let s = store();
        let c = s.save_connection(sample("db")).unwrap();
        assert!(c.last_used_at.is_none());
        s.touch_connection(&c.id).unwrap();
        match s.get_connection(&c.id).unwrap().unwrap() {
            StoredConnection::Ok(loaded) => assert!(loaded.last_used_at.is_some()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn deleting_cascades_to_owned_records_but_keeps_audit() {
        use fg_core::{Actor, AuditEntry, AuditId, Decision, Outcome, StatementClass};
        let s = store();
        let c = s.save_connection(sample("db")).unwrap();
        s.set_secret(&c.id, SecretKind::DatabasePassword, "pw")
            .unwrap();
        s.append_audit(&AuditEntry {
            id: AuditId::new(),
            at: UnixMillis::now(),
            actor: Actor::User,
            connection_id: Some(c.id.clone()),
            connection_name: "db".into(),
            statement: "SELECT 1".into(),
            class: StatementClass::Read,
            danger: None,
            decision: Decision::Allowed,
            reason: None,
            outcome: Outcome::Ok,
            error: None,
            duration_ms: Some(1),
            row_count: Some(1),
        })
        .unwrap();

        assert!(s.delete_connection(&c.id).unwrap());
        assert!(s.get_connection(&c.id).unwrap().is_none());
        assert_eq!(
            s.get_secret(&c.id, SecretKind::DatabasePassword).unwrap(),
            None
        );
        assert_eq!(
            s.query_audit(&crate::AuditFilter::default()).unwrap().len(),
            1
        );
    }
}
