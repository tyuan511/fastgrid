//! Sync cursors: how far this store has consumed each remote device's segments.
//!
//! One row per (sync target, remote device) in the `sync_state` table created by the
//! initial schema; this module only adds the accessors, no migration. A cursor is the
//! sequence number of the last segment applied from that device, so a later pull only
//! downloads newer segments. See `docs/encryption-and-sync.md`.

use fg_core::Result;
use rusqlite::params;

use crate::{Store, storage};

impl Store {
    /// Last segment sequence consumed from `device_id` on sync target `remote_id`.
    /// Zero when nothing has been consumed yet.
    pub fn sync_cursor(&self, remote_id: &str, device_id: &str) -> Result<i64> {
        let row: Option<i64> = self
            .conn
            .query_row(
                "SELECT last_seq FROM sync_state WHERE remote_id = ?1 AND device_id = ?2",
                params![remote_id, device_id],
                |r| r.get(0),
            )
            .ok();
        Ok(row.unwrap_or(0))
    }

    /// Advance the cursor to `seq`. Only moves forward: a stale or duplicate write is
    /// ignored, so segments completing out of order cannot rewind consumption and
    /// cause already-applied segments to be downloaded again.
    pub fn set_sync_cursor(&self, remote_id: &str, device_id: &str, seq: i64) -> Result<()> {
        if seq <= self.sync_cursor(remote_id, device_id)? {
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO sync_state (remote_id, device_id, last_seq, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(remote_id, device_id) DO UPDATE SET
                   last_seq = excluded.last_seq, updated_at = excluded.updated_at",
                params![remote_id, device_id, seq, self.now()],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// All cursors for one sync target, as (device_id, last_seq) pairs. Snapshot
    /// cleanup uses this to decide which segments every known device has consumed.
    pub fn sync_cursors(&self, remote_id: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT device_id, last_seq FROM sync_state WHERE remote_id = ?1
                 ORDER BY device_id",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map([remote_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::store;

    #[test]
    fn unknown_cursor_is_zero() {
        let s = store();
        assert_eq!(s.sync_cursor("vault", "dev-a").unwrap(), 0);
    }

    #[test]
    fn cursor_advances_forward_only() {
        let s = store();
        s.set_sync_cursor("vault", "dev-a", 3).unwrap();
        assert_eq!(s.sync_cursor("vault", "dev-a").unwrap(), 3);
        // Stale and duplicate writes are ignored.
        s.set_sync_cursor("vault", "dev-a", 2).unwrap();
        s.set_sync_cursor("vault", "dev-a", 3).unwrap();
        assert_eq!(s.sync_cursor("vault", "dev-a").unwrap(), 3);
        s.set_sync_cursor("vault", "dev-a", 4).unwrap();
        assert_eq!(s.sync_cursor("vault", "dev-a").unwrap(), 4);
    }

    #[test]
    fn cursors_are_scoped_by_target_and_device() {
        let s = store();
        s.set_sync_cursor("vault", "dev-a", 2).unwrap();
        s.set_sync_cursor("vault", "dev-b", 5).unwrap();
        s.set_sync_cursor("other", "dev-a", 9).unwrap();
        assert_eq!(
            s.sync_cursors("vault").unwrap(),
            vec![("dev-a".to_string(), 2), ("dev-b".to_string(), 5)]
        );
        assert_eq!(s.sync_cursor("other", "dev-a").unwrap(), 9);
        assert_eq!(s.sync_cursor("vault", "dev-c").unwrap(), 0);
    }
}
