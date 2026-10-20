//! Generic encrypted record storage that every typed API (connections, history...) is built on.

use fg_core::{Error, Result, UnixMillis};
use fg_crypto::{Envelope, seal};
use rusqlite::{OptionalExtension, ToSql, params, params_from_iter};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Store, crypto, storage};

/// A record as stored: plaintext bookkeeping plus the sealed payload.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub kind: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub sort_ts: Option<i64>,
    pub rev: i64,
    pub device_id: String,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    pub key_id: u32,
    pub enc: Vec<u8>,
}

pub(crate) const COLUMNS: &str =
    "kind, id, parent_id, sort_ts, rev, device_id, updated_at, deleted_at, key_id, enc";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        kind: r.get(0)?,
        id: r.get(1)?,
        parent_id: r.get(2)?,
        sort_ts: r.get(3)?,
        rev: r.get(4)?,
        device_id: r.get(5)?,
        updated_at: r.get(6)?,
        deleted_at: r.get(7)?,
        key_id: r.get(8)?,
        enc: r.get(9)?,
    })
}

/// Result of reading a record. A record that cannot be read (written by a newer version, or
/// sealed with a key we do not have) is reported, never dropped, and stays in the store untouched.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded<T> {
    Ok(T),
    Unreadable {
        id: String,
        reason: String,
        /// The decrypted JSON when decryption worked but the shape was not understood, so that
        /// nothing a newer version wrote is ever lost.
        raw: Option<String>,
    },
}

impl<T> Loaded<T> {
    pub fn ok(self) -> Option<T> {
        match self {
            Loaded::Ok(v) => Some(v),
            Loaded::Unreadable { .. } => None,
        }
    }
}

/// Binds a ciphertext to the row it belongs to: copying `enc` to another row fails to open.
pub(crate) fn aad(kind: &str, id: &str) -> Vec<u8> {
    format!("fg-rec|{kind}|{id}").into_bytes()
}

impl Store {
    pub(crate) fn now(&self) -> i64 {
        UnixMillis::now().0
    }

    pub(crate) fn seal_payload(&self, kind: &str, id: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        seal(&self.key, self.key_id, &aad(kind, id), plaintext).map_err(crypto)
    }

    pub(crate) fn open_payload(&self, row: &Row) -> std::result::Result<Vec<u8>, String> {
        if row.key_id != self.key_id {
            return Err(format!(
                "sealed with key generation {}, which is not available",
                row.key_id
            ));
        }
        let env = Envelope::parse(&row.enc).map_err(|e| e.to_string())?;
        if env.key_id != self.key_id {
            return Err(format!(
                "sealed with key generation {}, which is not available",
                env.key_id
            ));
        }
        env.open(&self.key, &aad(&row.kind, &row.id))
            .map_err(|e| e.to_string())
    }

    pub(crate) fn decode<T: DeserializeOwned>(&self, row: &Row) -> Loaded<T> {
        match self.open_payload(row) {
            Err(reason) => Loaded::Unreadable {
                id: row.id.clone(),
                reason,
                raw: None,
            },
            Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
                Ok(v) => Loaded::Ok(v),
                Err(e) => Loaded::Unreadable {
                    id: row.id.clone(),
                    reason: e.to_string(),
                    raw: String::from_utf8(bytes).ok(),
                },
            },
        }
    }

    /// Live (not deleted) rows matching `where_sql`, which may reference `?1..` params.
    pub(crate) fn fetch(
        &self,
        where_sql: &str,
        order_limit: &str,
        params: &[&dyn ToSql],
    ) -> Result<Vec<Row>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM record WHERE deleted_at IS NULL AND ({where_sql}) {order_limit}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(storage)?;
        stmt.query_map(params_from_iter(params.iter().copied()), row_from)
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)
    }

    /// Any row, including tombstones.
    pub(crate) fn fetch_any(&self, kind: &str, id: &str) -> Result<Option<Row>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM record WHERE kind = ?1 AND id = ?2"),
                [kind, id],
                row_from,
            )
            .optional()
            .map_err(storage)
    }

    /// Write a record locally. Bumps `rev` (also past a tombstone, which resurrects the id),
    /// stamps this device, and marks it dirty for sync.
    pub(crate) fn put_json<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        parent_id: Option<&str>,
        sort_ts: Option<i64>,
        value: &T,
    ) -> Result<()> {
        let json = serde_json::to_vec(value)
            .map_err(|e| Error::Storage(format!("cannot serialize {kind}: {e}")))?;
        let enc = self.seal_payload(kind, id, &json)?;
        let tx = self.conn.unchecked_transaction().map_err(storage)?;
        let old: Option<i64> = tx
            .query_row(
                "SELECT rev FROM record WHERE kind = ?1 AND id = ?2",
                [kind, id],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage)?;
        tx.execute(
            "INSERT OR REPLACE INTO record
               (kind, id, parent_id, sort_ts, rev, device_id, updated_at, deleted_at, dirty, key_id, enc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 1, ?8, ?9)",
            params![
                kind,
                id,
                parent_id,
                sort_ts,
                old.map_or(1, |r| r + 1),
                self.device_id,
                self.now(),
                self.key_id,
                enc
            ],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)
    }

    pub(crate) fn get_json<T: DeserializeOwned>(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<Loaded<T>>> {
        Ok(self
            .fetch("kind = ?1 AND id = ?2", "", &[&kind, &id])?
            .first()
            .map(|row| self.decode(row)))
    }

    /// Newest first (by `sort_ts`, then id). `limit` caps how many are decrypted.
    pub(crate) fn list_json<T: DeserializeOwned>(
        &self,
        kind: &str,
        parent_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Loaded<T>>> {
        let limit = i64::try_from(limit.unwrap_or(usize::MAX)).unwrap_or(i64::MAX);
        let rows = self.fetch(
            "kind = ?1 AND (?2 IS NULL OR parent_id = ?2)",
            "ORDER BY sort_ts DESC, id DESC LIMIT ?3",
            &[&kind, &parent_id, &limit],
        )?;
        Ok(rows.iter().map(|r| self.decode(r)).collect())
    }

    /// Delete by tombstone: the payload is wiped, the bookkeeping stays so the deletion can sync.
    pub(crate) fn tombstone(&self, kind: &str, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE record SET rev = rev + 1, device_id = ?3, updated_at = ?4, deleted_at = ?4,
                        dirty = 1, key_id = ?5, enc = x''
                 WHERE kind = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![kind, id, self.device_id, self.now(), self.key_id],
            )
            .map_err(storage)?;
        Ok(n > 0)
    }

    /// Tombstone every live record of the given kinds that hangs off `parent_id`.
    pub(crate) fn tombstone_children(&self, kinds: &[&str], parent_id: &str) -> Result<usize> {
        let mut total = 0;
        for kind in kinds {
            total += self
                .conn
                .execute(
                    "UPDATE record SET rev = rev + 1, device_id = ?3, updated_at = ?4, deleted_at = ?4,
                            dirty = 1, key_id = ?5, enc = x''
                     WHERE kind = ?1 AND parent_id = ?2 AND deleted_at IS NULL",
                    params![kind, parent_id, self.device_id, self.now(), self.key_id],
                )
                .map_err(storage)?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::testutil::store;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Note {
        text: String,
    }

    fn note(t: &str) -> Note {
        Note { text: t.into() }
    }

    #[test]
    fn put_get_roundtrip_bumps_rev_and_marks_dirty() {
        let s = store();
        s.put_json("note", "a", None, None, &note("one")).unwrap();
        s.put_json("note", "a", None, None, &note("two")).unwrap();
        assert_eq!(
            s.get_json::<Note>("note", "a").unwrap(),
            Some(Loaded::Ok(note("two")))
        );
        let row = s.fetch_any("note", "a").unwrap().unwrap();
        assert_eq!((row.rev, row.device_id.as_str()), (2, s.device_id()));
    }

    #[test]
    fn tombstones_hide_the_record_and_resurrecting_outranks_them() {
        let s = store();
        s.put_json("note", "a", None, None, &note("x")).unwrap();
        assert!(s.tombstone("note", "a").unwrap());
        assert!(!s.tombstone("note", "a").unwrap(), "already deleted");
        assert!(s.get_json::<Note>("note", "a").unwrap().is_none());
        let dead = s.fetch_any("note", "a").unwrap().unwrap();
        assert!(dead.deleted_at.is_some() && dead.enc.is_empty());
        assert_eq!(dead.rev, 2);

        s.put_json("note", "a", None, None, &note("back")).unwrap();
        assert_eq!(s.fetch_any("note", "a").unwrap().unwrap().rev, 3);
        assert!(s.get_json::<Note>("note", "a").unwrap().is_some());
    }

    #[test]
    fn listing_is_newest_first_and_scoped_to_the_parent() {
        let s = store();
        for (i, parent) in [(1, "p1"), (2, "p2"), (3, "p1")] {
            s.put_json(
                "note",
                &format!("n{i}"),
                Some(parent),
                Some(i),
                &note(&format!("{i}")),
            )
            .unwrap();
        }
        let texts = |parent| -> Vec<String> {
            s.list_json::<Note>("note", parent, None)
                .unwrap()
                .into_iter()
                .filter_map(Loaded::ok)
                .map(|n| n.text)
                .collect()
        };
        assert_eq!(texts(None), ["3", "2", "1"]);
        assert_eq!(texts(Some("p1")), ["3", "1"]);
        assert_eq!(s.list_json::<Note>("note", None, Some(1)).unwrap().len(), 1);
    }

    #[test]
    fn a_ciphertext_copied_to_another_row_does_not_open() {
        let s = store();
        s.put_json("note", "a", None, None, &note("for a")).unwrap();
        s.put_json("note", "b", None, None, &note("for b")).unwrap();
        s.conn
            .execute(
                "UPDATE record SET enc = (SELECT enc FROM record WHERE id = 'a') WHERE id = 'b'",
                [],
            )
            .unwrap();
        assert!(matches!(
            s.get_json::<Note>("note", "b").unwrap(),
            Some(Loaded::Unreadable { .. })
        ));
        assert_eq!(
            s.get_json::<Note>("note", "a").unwrap(),
            Some(Loaded::Ok(note("for a")))
        );
    }

    #[test]
    fn tampered_payloads_are_reported_not_trusted() {
        let s = store();
        s.put_json("note", "a", None, None, &note("x")).unwrap();
        let mut enc = s.fetch_any("note", "a").unwrap().unwrap().enc;
        let last = enc.len() - 1;
        enc[last] ^= 1;
        s.conn
            .execute("UPDATE record SET enc = ?1 WHERE id = 'a'", [enc])
            .unwrap();
        assert!(matches!(
            s.get_json::<Note>("note", "a").unwrap(),
            Some(Loaded::Unreadable { raw: None, .. })
        ));
    }

    #[test]
    fn an_unexpected_shape_keeps_the_raw_json() {
        let s = store();
        s.put_json(
            "note",
            "a",
            None,
            None,
            &serde_json::json!({"from": "the future"}),
        )
        .unwrap();
        match s.get_json::<Note>("note", "a").unwrap() {
            Some(Loaded::Unreadable { raw: Some(raw), .. }) => assert!(raw.contains("future")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn children_are_tombstoned_by_kind() {
        let s = store();
        s.put_json("a", "1", Some("p"), None, &note("x")).unwrap();
        s.put_json("b", "2", Some("p"), None, &note("x")).unwrap();
        assert_eq!(s.tombstone_children(&["a"], "p").unwrap(), 1);
        assert!(s.get_json::<Note>("a", "1").unwrap().is_none());
        assert!(s.get_json::<Note>("b", "2").unwrap().is_some());
    }
}
