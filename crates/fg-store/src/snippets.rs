//! Saved SQL fragments (`kind = snippet`). `parent_id` is the connection, or NULL when the
//! fragment is global.

use fg_core::{ConnectionId, Result, Snippet, SnippetId};

use crate::record::Loaded;
use crate::{Store, kinds};

/// A snippet as found in the store. An unreadable payload is kept and listed so a settings screen
/// can show it as unavailable instead of offering the id for a new save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredSnippet {
    Ok(Snippet),
    Unreadable { id: SnippetId, reason: String },
}

impl StoredSnippet {
    pub fn id(&self) -> &SnippetId {
        match self {
            StoredSnippet::Ok(snippet) => &snippet.id,
            StoredSnippet::Unreadable { id, .. } => id,
        }
    }

    pub fn snippet(&self) -> Option<&Snippet> {
        match self {
            StoredSnippet::Ok(snippet) => Some(snippet),
            StoredSnippet::Unreadable { .. } => None,
        }
    }
}

impl Store {
    /// Insert or replace a snippet. A missing `connection_id` is stored with a NULL parent so it
    /// survives connection deletion and shows up for every connection.
    ///
    /// Refuses to replace a row this version cannot read.
    pub fn save_snippet(&self, s: &Snippet) -> Result<()> {
        self.put_json_if_readable(
            kinds::SNIPPET,
            s.id.as_str(),
            s.connection_id.as_ref().map(ConnectionId::as_str),
            Some(s.updated_at.0),
            s,
            |reason| format!("refusing to overwrite an unreadable snippet ({reason})"),
        )
    }

    /// Snippets visible for `connection`: globals (`connection_id` is `None`) plus that
    /// connection's own. `None` returns every snippet, including other connections'.
    ///
    /// Readable rows are ordered by `sort_order` ascending, then `updated_at` descending.
    /// Unreadable rows are listed after them and kept. A record this version cannot read is not
    /// omitted: missing and unreadable must stay distinguishable.
    pub fn snippets(&self, connection: Option<&ConnectionId>) -> Result<Vec<StoredSnippet>> {
        let parent = connection.map(ConnectionId::as_str);
        let rows = self.fetch(
            "kind = ?1 AND (?2 IS NULL OR parent_id IS NULL OR parent_id = ?2)",
            "",
            &[&kinds::SNIPPET, &parent],
        )?;
        let mut items = Vec::new();
        for row in &rows {
            match self.decode::<Snippet>(row) {
                Loaded::Ok(snippet) => {
                    let visible = match connection {
                        None => true,
                        Some(id) => snippet.connection_id.as_ref().is_none_or(|c| c == id),
                    };
                    if visible {
                        items.push(StoredSnippet::Ok(snippet));
                    }
                }
                Loaded::Unreadable { id, reason, .. } => {
                    items.push(StoredSnippet::Unreadable {
                        id: SnippetId::from_string(id),
                        reason,
                    });
                }
            }
        }
        items.sort_by(|a, b| match (a, b) {
            (StoredSnippet::Ok(a), StoredSnippet::Ok(b)) => a
                .sort_order
                .cmp(&b.sort_order)
                .then(b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.id.cmp(&b.id)),
            (StoredSnippet::Ok(_), StoredSnippet::Unreadable { .. }) => std::cmp::Ordering::Less,
            (StoredSnippet::Unreadable { .. }, StoredSnippet::Ok(_)) => std::cmp::Ordering::Greater,
            (StoredSnippet::Unreadable { id: a, .. }, StoredSnippet::Unreadable { id: b, .. }) => {
                a.cmp(b)
            }
        });
        Ok(items)
    }

    /// Tombstone the snippet. `false` when there was no live row. Globals and connection-scoped
    /// snippets use the same id space.
    pub fn delete_snippet(&self, id: &SnippetId) -> Result<bool> {
        self.tombstone(kinds::SNIPPET, id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{Error, UnixMillis};

    use super::*;
    use crate::testutil::store;

    fn snippet(conn: Option<&ConnectionId>, title: &str, order: i64, at: i64) -> Snippet {
        Snippet {
            id: SnippetId::new(),
            connection_id: conn.cloned(),
            title: title.into(),
            sql: format!("SELECT {title}"),
            tags: vec!["fav".into()],
            sort_order: order,
            created_at: UnixMillis(at),
            updated_at: UnixMillis(at),
        }
    }

    fn readable(items: &[StoredSnippet]) -> Vec<Snippet> {
        items
            .iter()
            .filter_map(StoredSnippet::snippet)
            .cloned()
            .collect()
    }

    #[test]
    fn roundtrip_update_and_tombstone() {
        let s = store();
        let conn = ConnectionId::new();
        let mut item = snippet(Some(&conn), "count", 0, 10);
        s.save_snippet(&item).unwrap();
        assert_eq!(
            readable(&s.snippets(Some(&conn)).unwrap()),
            vec![item.clone()]
        );

        item.sql = "SELECT count(*) FROM t".into();
        item.updated_at = UnixMillis(30);
        item.tags.push("agg".into());
        s.save_snippet(&item).unwrap();
        assert_eq!(
            readable(&s.snippets(Some(&conn)).unwrap()),
            vec![item.clone()]
        );

        assert!(s.delete_snippet(&item.id).unwrap());
        assert!(s.snippets(Some(&conn)).unwrap().is_empty());
        assert!(!s.delete_snippet(&item.id).unwrap());
        let row = s
            .fetch_any(kinds::SNIPPET, item.id.as_str())
            .unwrap()
            .expect("tombstone stays");
        assert!(row.deleted_at.is_some());
        assert!(row.enc.is_empty());
    }

    #[test]
    fn visibility_and_ordering() {
        let s = store();
        let a = ConnectionId::new();
        let b = ConnectionId::new();
        let global_late = snippet(None, "global-late", 1, 50);
        let global_new = snippet(None, "global-new", 1, 200);
        let own = snippet(Some(&a), "own", 2, 100);
        let foreign = snippet(Some(&b), "foreign", 0, 999);
        for item in [&global_late, &global_new, &own, &foreign] {
            s.save_snippet(item).unwrap();
        }

        let visible = readable(&s.snippets(Some(&a)).unwrap());
        assert_eq!(
            visible.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            ["global-new", "global-late", "own"]
        );
        assert!(visible.iter().all(|s| s.connection_id.as_ref() != Some(&b)));

        let everything = readable(&s.snippets(None).unwrap());
        assert_eq!(everything.len(), 4);
        assert_eq!(everything[0].title, "foreign");
    }

    #[test]
    fn unreadable_snippet_is_listed_and_kept() {
        let s = store();
        let conn = ConnectionId::new();
        let live = snippet(Some(&conn), "ok", 0, 1);
        s.save_snippet(&live).unwrap();
        s.put_json(
            kinds::SNIPPET,
            "newer-shape",
            Some(conn.as_str()),
            Some(2),
            &serde_json::json!({"id": "newer-shape"}),
        )
        .unwrap();

        let listed = s.snippets(Some(&conn)).unwrap();
        assert!(listed.iter().any(|item| item.snippet() == Some(&live)));
        assert!(
            listed.iter().any(|item| matches!(
                item,
                StoredSnippet::Unreadable { id, .. } if id.as_str() == "newer-shape"
            )),
            "an unreadable snippet must not be hidden: {listed:?}"
        );
        assert_eq!(
            s.fetch(
                "kind = ?1 AND id = ?2",
                "",
                &[&kinds::SNIPPET, &"newer-shape"]
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn save_refuses_to_overwrite_an_unreadable_snippet() {
        let s = store();
        let conn = ConnectionId::new();
        let id = "newer-shape";
        s.put_json(
            kinds::SNIPPET,
            id,
            Some(conn.as_str()),
            Some(2),
            &serde_json::json!({"id": id}),
        )
        .unwrap();
        let before = s
            .fetch("kind = ?1 AND id = ?2", "", &[&kinds::SNIPPET, &id])
            .unwrap();
        let enc = before[0].enc.clone();
        let mut replacement = snippet(Some(&conn), "clobber", 0, 3);
        replacement.id = SnippetId::from_string(id);
        let err = s.save_snippet(&replacement).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err:?}");
        let after = s
            .fetch("kind = ?1 AND id = ?2", "", &[&kinds::SNIPPET, &id])
            .unwrap();
        assert_eq!(after[0].enc, enc);
    }

    #[test]
    fn deleting_the_connection_removes_its_snippets_but_keeps_globals() {
        let s = store();
        let gone = s
            .save_connection(fg_core::ConnectionConfig::new(
                "db",
                fg_core::DbKind::Sqlite,
                fg_core::ConnectionParams::File {
                    path: "a.db".into(),
                },
            ))
            .unwrap();
        let other = ConnectionId::new();
        let dropped = snippet(Some(&gone.id), "local", 0, 1);
        let global = snippet(None, "global", 0, 2);
        let kept = snippet(Some(&other), "other", 0, 3);
        s.save_snippet(&dropped).unwrap();
        s.save_snippet(&global).unwrap();
        s.save_snippet(&kept).unwrap();

        assert!(s.delete_connection(&gone.id).unwrap());
        let left = readable(&s.snippets(None).unwrap());
        assert_eq!(left.len(), 2);
        assert!(left.iter().any(|s| s.id == global.id));
        assert!(left.iter().any(|s| s.id == kept.id));
        assert!(
            s.snippets(Some(&gone.id))
                .unwrap()
                .iter()
                .filter_map(StoredSnippet::snippet)
                .all(|s| s.connection_id.is_none())
        );
    }
}
