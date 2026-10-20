use fg_core::{ConnectionId, QueryHistoryEntry, Result};

use crate::record::Loaded;
use crate::{Store, kinds};

/// History search decrypts the newest entries and filters in memory (SQL text is encrypted).
const SEARCH_SCAN_LIMIT: usize = 5000;

impl Store {
    pub fn append_history(&self, e: &QueryHistoryEntry) -> Result<()> {
        self.put_json(
            kinds::HISTORY,
            e.id.as_str(),
            Some(e.connection_id.as_str()),
            Some(e.executed_at.0),
            e,
        )
    }

    /// Newest first. `search` is a case-insensitive substring of the SQL text, matched among the
    /// most recent 5000 entries.
    pub fn recent_history(
        &self,
        connection: &ConnectionId,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<QueryHistoryEntry>> {
        let scan = if search.is_some() {
            SEARCH_SCAN_LIMIT
        } else {
            limit
        };
        let needle = search.map(str::to_lowercase);
        Ok(self
            .list_json::<QueryHistoryEntry>(kinds::HISTORY, Some(connection.as_str()), Some(scan))?
            .into_iter()
            .filter_map(Loaded::ok)
            .filter(|e| {
                needle
                    .as_ref()
                    .is_none_or(|n| e.sql.to_lowercase().contains(n))
            })
            .take(limit)
            .collect())
    }

    /// Keep only the newest `keep` entries of a connection.
    pub fn prune_history(&self, connection: &ConnectionId, keep: usize) -> Result<usize> {
        let ids: Vec<String> = self
            .fetch(
                "kind = ?1 AND parent_id = ?2",
                "ORDER BY sort_ts DESC, id DESC",
                &[&kinds::HISTORY, &connection.as_str()],
            )?
            .into_iter()
            .skip(keep)
            .map(|r| r.id)
            .collect();
        let mut removed = 0;
        for id in ids {
            if self.tombstone(kinds::HISTORY, &id)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn clear_history(&self, connection: &ConnectionId) -> Result<usize> {
        self.tombstone_children(&[kinds::HISTORY], connection.as_str())
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{HistoryId, Outcome, QuerySource, UnixMillis};

    use super::*;
    use crate::testutil::store;

    fn entry(conn: &ConnectionId, sql: &str, at: i64) -> QueryHistoryEntry {
        QueryHistoryEntry {
            id: HistoryId::new(),
            connection_id: conn.clone(),
            namespace: Some("main".into()),
            sql: sql.into(),
            executed_at: UnixMillis(at),
            duration_ms: Some(5),
            outcome: Outcome::Ok,
            row_count: Some(3),
            error: None,
            source: QuerySource::Editor,
        }
    }

    #[test]
    fn newest_first_search_prune_and_clear() {
        let s = store();
        let id = ConnectionId::new();
        let other = ConnectionId::new();
        for (i, sql) in ["SELECT 1", "SELECT 100%", "UPDATE t SET a = 1"]
            .iter()
            .enumerate()
        {
            s.append_history(&entry(&id, sql, i as i64)).unwrap();
        }
        s.append_history(&entry(&other, "SELECT other", 9)).unwrap();

        let all = s.recent_history(&id, None, 10).unwrap();
        assert_eq!(
            all.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(),
            ["UPDATE t SET a = 1", "SELECT 100%", "SELECT 1"]
        );
        assert_eq!(all[0].source, QuerySource::Editor);

        assert_eq!(s.recent_history(&id, Some("100%"), 10).unwrap().len(), 1);
        assert_eq!(s.recent_history(&id, Some("select"), 10).unwrap().len(), 2);
        assert_eq!(s.recent_history(&id, Some("select"), 1).unwrap().len(), 1);

        assert_eq!(s.prune_history(&id, 1).unwrap(), 2);
        assert_eq!(s.recent_history(&id, None, 10).unwrap().len(), 1);
        assert_eq!(s.clear_history(&id).unwrap(), 1);
        assert_eq!(
            s.recent_history(&other, None, 10).unwrap().len(),
            1,
            "other connections untouched"
        );
    }
}
