//! Editor drafts (`kind = document`). One row per query tab, parented by its connection.

use fg_core::{ConnectionId, DocumentId, QueryDocument, Result};

use crate::record::Loaded;
use crate::{Store, kinds};

/// A draft as found in the store. A payload this version cannot parse is kept and surfaced so the
/// UI can say why; it is never reported as missing, and a save must not overwrite it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredDocument {
    Ok(QueryDocument),
    Unreadable { id: DocumentId, reason: String },
}

impl StoredDocument {
    pub fn id(&self) -> &DocumentId {
        match self {
            StoredDocument::Ok(doc) => &doc.id,
            StoredDocument::Unreadable { id, .. } => id,
        }
    }

    /// The draft, when this version could read it.
    pub fn document(&self) -> Option<&QueryDocument> {
        match self {
            StoredDocument::Ok(doc) => Some(doc),
            StoredDocument::Unreadable { .. } => None,
        }
    }
}

impl Store {
    /// Insert or replace a draft. `sort_ts` is `updated_at` so a newest-first scan does not need
    /// to open every payload. The caller owns both timestamps; this does not rewrite them.
    ///
    /// Refuses to replace a row this version cannot read. Overwriting it would destroy a newer
    /// record the UI cannot even show.
    pub fn save_document(&self, doc: &QueryDocument) -> Result<()> {
        self.put_json_if_readable(
            kinds::DOCUMENT,
            doc.id.as_str(),
            Some(doc.connection_id.as_str()),
            Some(doc.updated_at.0),
            doc,
            |reason| format!("refusing to overwrite an unreadable document ({reason})"),
        )
    }

    /// The live draft, or `None` when it was never saved or has been tombstoned.
    ///
    /// A payload this version cannot read is [`StoredDocument::Unreadable`], not `None`: missing
    /// and unreadable are different, and only the former may be created under that id.
    pub fn document(&self, id: &DocumentId) -> Result<Option<StoredDocument>> {
        Ok(self
            .get_json::<QueryDocument>(kinds::DOCUMENT, id.as_str())?
            .map(stored_document))
    }

    /// Drafts of one connection, newest `updated_at` first. Other connections are excluded.
    /// Unreadable rows are included and kept.
    pub fn documents(&self, connection: &ConnectionId) -> Result<Vec<StoredDocument>> {
        let rows = self.fetch(
            "kind = ?1 AND parent_id = ?2",
            "ORDER BY sort_ts DESC, id DESC",
            &[&kinds::DOCUMENT, &connection.as_str()],
        )?;
        Ok(rows
            .iter()
            .map(|row| stored_document(self.decode(row)))
            .collect())
    }

    /// Tombstone the draft. `false` when there was no live row.
    pub fn delete_document(&self, id: &DocumentId) -> Result<bool> {
        self.tombstone(kinds::DOCUMENT, id.as_str())
    }
}

fn stored_document(loaded: Loaded<QueryDocument>) -> StoredDocument {
    match loaded {
        Loaded::Ok(doc) => StoredDocument::Ok(doc),
        Loaded::Unreadable { id, reason, .. } => StoredDocument::Unreadable {
            id: DocumentId::from_string(id),
            reason,
        },
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{Error, UnixMillis};

    use super::*;
    use crate::testutil::store;

    fn draft(conn: &ConnectionId, title: &str, content: &str, at: i64) -> QueryDocument {
        QueryDocument {
            id: DocumentId::new(),
            connection_id: conn.clone(),
            title: title.into(),
            content: content.into(),
            saved_path: None,
            dirty: true,
            cursor_offset: Some(4),
            created_at: UnixMillis(at),
            updated_at: UnixMillis(at),
        }
    }

    fn ok(loaded: Option<StoredDocument>) -> Option<QueryDocument> {
        loaded.and_then(|item| match item {
            StoredDocument::Ok(doc) => Some(doc),
            StoredDocument::Unreadable { .. } => None,
        })
    }

    #[test]
    fn roundtrip_update_and_tombstone() {
        let s = store();
        let conn = ConnectionId::new();
        let mut doc = draft(&conn, "q", "SELECT 1", 10);
        assert_eq!(s.document(&doc.id).unwrap(), None);

        s.save_document(&doc).unwrap();
        assert_eq!(ok(s.document(&doc.id).unwrap()).as_ref(), Some(&doc));

        doc.content = "SELECT 2".into();
        doc.dirty = false;
        doc.saved_path = Some("/tmp/q.sql".into());
        doc.updated_at = UnixMillis(20);
        s.save_document(&doc).unwrap();
        assert_eq!(ok(s.document(&doc.id).unwrap()).as_ref(), Some(&doc));

        assert!(s.delete_document(&doc.id).unwrap());
        assert_eq!(s.document(&doc.id).unwrap(), None);
        assert!(!s.delete_document(&doc.id).unwrap());
        let row = s
            .fetch_any(kinds::DOCUMENT, doc.id.as_str())
            .unwrap()
            .expect("tombstone stays");
        assert!(row.deleted_at.is_some());
        assert!(row.enc.is_empty());
    }

    #[test]
    fn newest_first_and_isolated_per_connection() {
        let s = store();
        let a = ConnectionId::new();
        let b = ConnectionId::new();
        let older = draft(&a, "old", "SELECT old", 1);
        let newer = draft(&a, "new", "SELECT new", 5);
        let other = draft(&b, "other", "SELECT other", 9);
        s.save_document(&older).unwrap();
        s.save_document(&newer).unwrap();
        s.save_document(&other).unwrap();

        let listed = s.documents(&a).unwrap();
        assert_eq!(
            listed
                .iter()
                .filter_map(StoredDocument::document)
                .map(|d| d.title.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert_eq!(
            s.documents(&b)
                .unwrap()
                .into_iter()
                .filter_map(|item| item.document().cloned())
                .collect::<Vec<_>>(),
            vec![other]
        );
    }

    #[test]
    fn unreadable_draft_is_surfaced_and_kept() {
        let s = store();
        let conn = ConnectionId::new();
        let live = draft(&conn, "ok", "SELECT 1", 3);
        s.save_document(&live).unwrap();
        s.put_json(
            kinds::DOCUMENT,
            "newer-shape",
            Some(conn.as_str()),
            Some(9),
            &serde_json::json!({"id": "newer-shape", "written_by": "future"}),
        )
        .unwrap();

        let id = DocumentId::from_string("newer-shape");
        match s.document(&id).unwrap() {
            Some(StoredDocument::Unreadable { id: found, reason }) => {
                assert_eq!(found, id);
                assert!(!reason.is_empty());
            }
            other => panic!("unreadable must not look missing: {other:?}"),
        }
        let listed = s.documents(&conn).unwrap();
        assert!(listed.iter().any(|item| item.id() == &id));
        assert!(listed.iter().any(|item| item.document() == Some(&live)));
        assert_eq!(
            s.fetch(
                "kind = ?1 AND id = ?2",
                "",
                &[&kinds::DOCUMENT, &"newer-shape"]
            )
            .unwrap()
            .len(),
            1,
            "an unreadable draft is not deleted or overwritten by a read"
        );
    }

    #[test]
    fn save_refuses_to_overwrite_an_unreadable_document() {
        let s = store();
        let conn = ConnectionId::new();
        let id = "newer-shape";
        s.put_json(
            kinds::DOCUMENT,
            id,
            Some(conn.as_str()),
            Some(9),
            &serde_json::json!({"id": id, "written_by": "future"}),
        )
        .unwrap();
        let before = s
            .fetch("kind = ?1 AND id = ?2", "", &[&kinds::DOCUMENT, &id])
            .unwrap();
        let enc = before[0].enc.clone();
        let mut replacement = draft(&conn, "clobber", "SELECT 1", 10);
        replacement.id = DocumentId::from_string(id);
        let err = s.save_document(&replacement).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err:?}");
        let after = s
            .fetch("kind = ?1 AND id = ?2", "", &[&kinds::DOCUMENT, &id])
            .unwrap();
        assert_eq!(after[0].enc, enc);
        assert!(matches!(
            s.document(&replacement.id).unwrap(),
            Some(StoredDocument::Unreadable { .. })
        ));
    }

    #[test]
    fn deleting_the_connection_removes_its_drafts() {
        let s = store();
        let kept_conn = ConnectionId::new();
        let gone = s
            .save_connection(fg_core::ConnectionConfig::new(
                "db",
                fg_core::DbKind::Sqlite,
                fg_core::ConnectionParams::File {
                    path: "a.db".into(),
                },
            ))
            .unwrap();
        let dropped = draft(&gone.id, "tab", "SELECT 1", 1);
        let kept = draft(&kept_conn, "other", "SELECT 2", 2);
        s.save_document(&dropped).unwrap();
        s.save_document(&kept).unwrap();

        assert!(s.delete_connection(&gone.id).unwrap());
        assert_eq!(s.document(&dropped.id).unwrap(), None);
        assert!(s.documents(&gone.id).unwrap().is_empty());
        assert_eq!(ok(s.document(&kept.id).unwrap()).as_ref(), Some(&kept));
    }
}
