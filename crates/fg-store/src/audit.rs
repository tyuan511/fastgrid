use fg_core::{Actor, AuditEntry, AuditId, ConnectionId, Result, UnixMillis};

use crate::record::Loaded;
use crate::{Store, kinds};

#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub connection_id: Option<ConnectionId>,
    pub since: Option<UnixMillis>,
    pub until: Option<UnixMillis>,
    /// Only entries from this agent.
    pub mcp_client: Option<String>,
    /// Page size; default 200.
    pub limit: Option<usize>,
    /// Continue after this entry (the last one of the previous page): `(at, id)`.
    pub after: Option<(UnixMillis, AuditId)>,
}

const PAGE: usize = 500;

impl Store {
    /// Append-only: there is intentionally no edit and no per-entry delete.
    ///
    /// Insert, not replace. The primary key rejects a duplicate id, including a tombstone, so a
    /// replay cannot resurrect or overwrite an entry that was already written.
    pub fn append_audit(&self, e: &AuditEntry) -> Result<()> {
        self.insert_json(
            kinds::AUDIT,
            e.id.as_str(),
            e.connection_id.as_ref().map(ConnectionId::as_str),
            Some(e.at.0),
            e,
        )
        .map_err(|err| match err {
            fg_core::Error::Storage(message) if message.contains("already exists") => {
                fg_core::Error::Storage(format!("audit entry {} already exists", e.id.as_str()))
            }
            other => other,
        })
    }

    /// Newest first.
    pub fn query_audit(&self, f: &AuditFilter) -> Result<Vec<AuditEntry>> {
        let want = f.limit.unwrap_or(200);
        let mut out = Vec::new();
        let mut cursor = f.after.clone();
        while out.len() < want {
            let (cursor_ts, cursor_id) = match &cursor {
                Some((t, id)) => (Some(t.0), Some(id.as_str().to_owned())),
                None => (None, None),
            };
            let rows = self.fetch(
                "kind = ?1
                 AND (?2 IS NULL OR parent_id = ?2)
                 AND (?3 IS NULL OR sort_ts >= ?3)
                 AND (?4 IS NULL OR sort_ts <= ?4)
                 AND (?5 IS NULL OR (sort_ts, id) < (?5, ?6))",
                "ORDER BY sort_ts DESC, id DESC LIMIT ?7",
                &[
                    &kinds::AUDIT,
                    &f.connection_id.as_ref().map(ConnectionId::as_str),
                    &f.since.map(|t| t.0),
                    &f.until.map(|t| t.0),
                    &cursor_ts,
                    &cursor_id,
                    &(PAGE as i64),
                ],
            )?;
            let exhausted = rows.len() < PAGE;
            if let Some(last) = rows.last() {
                cursor = Some((
                    UnixMillis(last.sort_ts.unwrap_or(0)),
                    AuditId::from_string(last.id.clone()),
                ));
            }
            for row in &rows {
                if let Loaded::Ok(entry) = self.decode::<AuditEntry>(row) {
                    let from_agent = match (&f.mcp_client, &entry.actor) {
                        (None, _) => true,
                        (Some(name), Actor::Mcp { client }) => name == client,
                        (Some(_), _) => false,
                    };
                    if from_agent {
                        out.push(entry);
                        if out.len() == want {
                            return Ok(out);
                        }
                    }
                }
            }
            if exhausted {
                break;
            }
        }
        Ok(out)
    }

    /// Explicit user action only ("clear audit log older than ..."). Tombstones, so it syncs.
    pub fn purge_audit_before(&self, before: UnixMillis) -> Result<usize> {
        let ids: Vec<String> = self
            .fetch(
                "kind = ?1 AND sort_ts < ?2",
                "",
                &[&kinds::AUDIT, &before.0],
            )?
            .into_iter()
            .map(|r| r.id)
            .collect();
        let mut n = 0;
        for id in ids {
            if self.tombstone(kinds::AUDIT, &id)? {
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{DangerReason, Decision, Outcome, StatementClass};

    use super::*;
    use crate::testutil::store;

    fn entry(actor: Actor, at: i64, sql: &str) -> AuditEntry {
        AuditEntry {
            id: AuditId::new(),
            at: UnixMillis(at),
            actor,
            connection_id: Some(ConnectionId::from_string("c")),
            connection_name: "c".into(),
            statement: sql.into(),
            class: StatementClass::Write,
            danger: Some(DangerReason::DeleteWithoutWhere),
            decision: Decision::Denied,
            reason: Some("read-only".into()),
            outcome: Outcome::NotRun,
            error: None,
            duration_ms: None,
            row_count: None,
        }
    }

    #[test]
    fn roundtrip_filter_and_paging() {
        let s = store();
        let agent = Actor::Mcp {
            client: "claude".into(),
        };
        s.append_audit(&entry(Actor::User, 1, "a")).unwrap();
        s.append_audit(&entry(agent.clone(), 2, "b")).unwrap();
        s.append_audit(&entry(agent.clone(), 3, "c")).unwrap();

        let all = s.query_audit(&AuditFilter::default()).unwrap();
        assert_eq!(
            all.iter().map(|e| e.statement.as_str()).collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
        assert_eq!(all[0].actor, agent);
        assert_eq!(all[0].danger, Some(DangerReason::DeleteWithoutWhere));
        assert_eq!(all[0].decision, Decision::Denied);

        let only_agent = s
            .query_audit(&AuditFilter {
                mcp_client: Some("claude".into()),
                ..AuditFilter::default()
            })
            .unwrap();
        assert_eq!(only_agent.len(), 2);

        let page1 = s
            .query_audit(&AuditFilter {
                limit: Some(1),
                ..AuditFilter::default()
            })
            .unwrap();
        assert_eq!(page1[0].statement, "c");
        let after = Some((page1[0].at, page1[0].id.clone()));
        let page2 = s
            .query_audit(&AuditFilter {
                limit: Some(1),
                after,
                ..AuditFilter::default()
            })
            .unwrap();
        assert_eq!(page2[0].statement, "b");

        let window = s
            .query_audit(&AuditFilter {
                since: Some(UnixMillis(2)),
                until: Some(UnixMillis(2)),
                ..AuditFilter::default()
            })
            .unwrap();
        assert_eq!(window.len(), 1);
    }

    #[test]
    fn purge_removes_only_older_entries() {
        let s = store();
        s.append_audit(&entry(Actor::User, 1, "old")).unwrap();
        s.append_audit(&entry(Actor::User, 5, "new")).unwrap();
        assert_eq!(s.purge_audit_before(UnixMillis(3)).unwrap(), 1);
        let left = s.query_audit(&AuditFilter::default()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].statement, "new");
    }

    #[test]
    fn duplicate_audit_id_is_refused_and_the_original_survives() {
        let s = store();
        let original = entry(Actor::User, 1, "first");
        s.append_audit(&original).unwrap();
        let mut replay = original.clone();
        replay.statement = "replaced".into();
        replay.outcome = Outcome::Ok;
        let err = s.append_audit(&replay).unwrap_err();
        assert!(matches!(err, fg_core::Error::Storage(_)), "{err:?}");
        let left = s.query_audit(&AuditFilter::default()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].statement, "first");
        assert_eq!(left[0].outcome, Outcome::NotRun);
    }

    #[test]
    fn a_tombstoned_audit_id_cannot_be_reused() {
        let s = store();
        let original = entry(Actor::User, 1, "first");
        s.append_audit(&original).unwrap();
        assert_eq!(s.purge_audit_before(UnixMillis(2)).unwrap(), 1);
        let err = s.append_audit(&original).unwrap_err();
        assert!(matches!(err, fg_core::Error::Storage(_)), "{err:?}");
        assert!(s.query_audit(&AuditFilter::default()).unwrap().is_empty());
    }
}
