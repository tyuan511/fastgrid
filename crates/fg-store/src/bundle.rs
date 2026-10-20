//! Moving records between stores: encrypted backup files today, sync targets later. Both use
//! the same plaintext-record form ([`SyncRecord`]); the transport (a password-protected file, or
//! the sync key) is what differs. See `docs/encryption-and-sync.md`.

use fg_core::{Error, Result};
use fg_crypto::{CryptoError, KdfParams, password_open, password_seal_with};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::record::{COLUMNS, Row};
use crate::{Store, crypto, kinds, storage};

/// One record in transport form: bookkeeping plus the decrypted JSON payload. `data` is `None`
/// for a tombstone. This never touches disk unencrypted - it only exists in memory between a
/// store and a password/sync-key seal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRecord {
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub sort_ts: Option<i64>,
    pub rev: i64,
    pub device_id: String,
    pub updated_at: i64,
    #[serde(default)]
    pub deleted_at: Option<i64>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    /// The local copy wins (or is identical).
    Skipped,
}

/// A bundle written by a future version is refused rather than half-read.
const BUNDLE_FORMAT: u32 = 1;
const EXPORT_CONTEXT: &[u8] = b"fastgrid-export-v1";

#[derive(Serialize, Deserialize)]
struct Bundle {
    format_version: u32,
    exported_at: i64,
    source_store_id: String,
    records: Vec<SyncRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportScope {
    /// Connections, groups and snippets (plus their secrets if requested): a portable backup.
    Connections,
    /// Everything that syncs: also history, audit log, drafts, UI state and settings.
    Everything,
}

#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub scope: ExportScope,
    /// Include passwords and passphrases. They stay inside the password-encrypted file.
    pub include_secrets: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            scope: ExportScope::Connections,
            include_secrets: true,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub applied: usize,
    /// Skipped because this store already has a newer version of the record.
    pub skipped_newer_local: usize,
    /// Kinds this version does not know (written by a newer one); not imported.
    pub ignored_unknown_kind: usize,
}

fn export_kdf() -> Result<KdfParams> {
    // Unit tests use a cheap KDF; shipped builds always use the recommended cost.
    if cfg!(test) {
        KdfParams::with_cost(8, 1, 1)
    } else {
        KdfParams::recommended()
    }
    .map_err(crypto)
}

/// Ordering used to resolve a conflict between two versions of one record: the higher revision
/// wins; ties break on the later write time, then on the device id (so every device agrees).
fn newer(a: (i64, i64, &str), b: (i64, i64, &str)) -> bool {
    a > b
}

impl Store {
    fn sync_record(&self, row: &Row) -> Result<SyncRecord> {
        let data = if row.deleted_at.is_some() {
            None
        } else {
            let bytes = self.open_payload(row).map_err(|reason| {
                Error::Storage(format!("cannot read {} {}: {reason}", row.kind, row.id))
            })?;
            Some(serde_json::from_slice(&bytes).map_err(|e| {
                Error::Storage(format!("{} {} is not valid JSON: {e}", row.kind, row.id))
            })?)
        };
        Ok(SyncRecord {
            kind: row.kind.clone(),
            id: row.id.clone(),
            parent_id: row.parent_id.clone(),
            sort_ts: row.sort_ts,
            rev: row.rev,
            device_id: row.device_id.clone(),
            updated_at: row.updated_at,
            deleted_at: row.deleted_at,
            data,
        })
    }

    /// Insert or replace a record with explicit bookkeeping.
    fn write_record(&self, r: &SyncRecord, dirty: bool) -> Result<()> {
        let enc = match (&r.data, r.deleted_at) {
            (Some(data), None) => {
                let json = serde_json::to_vec(data)
                    .map_err(|e| Error::Storage(format!("cannot serialize {}: {e}", r.kind)))?;
                self.seal_payload(&r.kind, &r.id, &json)?
            }
            (None, Some(_)) => Vec::new(),
            _ => {
                return Err(Error::Storage(format!(
                    "record {} {} must be either live with data or a tombstone",
                    r.kind, r.id
                )));
            }
        };
        self.conn
            .execute(
                "INSERT OR REPLACE INTO record
                   (kind, id, parent_id, sort_ts, rev, device_id, updated_at, deleted_at, dirty, key_id, enc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    r.kind,
                    r.id,
                    r.parent_id,
                    r.sort_ts,
                    r.rev,
                    r.device_id,
                    r.updated_at,
                    r.deleted_at,
                    i64::from(dirty),
                    self.key_id,
                    enc
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Records changed here since they were last pushed, oldest first. Local-only kinds
    /// (`mcp_client`) are never included.
    pub fn dirty_records(&self, limit: usize) -> Result<Vec<SyncRecord>> {
        let kinds_list = kinds::SYNCED
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {COLUMNS} FROM record WHERE dirty = 1 AND kind IN ({kinds_list})
             ORDER BY updated_at, kind, id LIMIT ?1"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(storage)?;
        let rows = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
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
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        rows.iter().map(|r| self.sync_record(r)).collect()
    }

    /// Mark pushed records clean. A record edited again after it was read keeps its dirty flag
    /// (its `rev` no longer matches), so no change is ever lost.
    pub fn mark_clean(&self, pushed: &[(String, String, i64)]) -> Result<()> {
        for (kind, id, rev) in pushed {
            self.conn
                .execute(
                    "UPDATE record SET dirty = 0 WHERE kind = ?1 AND id = ?2 AND rev = ?3",
                    params![kind, id, rev],
                )
                .map_err(storage)?;
        }
        Ok(())
    }

    /// Apply a record received from a sync target. Conflicts resolve by [`newer`]. Applied
    /// records are stored clean (they came from the remote, nothing to push back).
    pub fn apply_remote(&self, r: &SyncRecord) -> Result<ApplyOutcome> {
        if !kinds::is_synced(&r.kind) {
            return Err(Error::Storage(format!(
                "record kind {:?} is not synced",
                r.kind
            )));
        }
        if let Some(local) = self.fetch_any(&r.kind, &r.id)? {
            let remote_wins = newer(
                (r.rev, r.updated_at, r.device_id.as_str()),
                (local.rev, local.updated_at, local.device_id.as_str()),
            );
            if !remote_wins {
                return Ok(ApplyOutcome::Skipped);
            }
        }
        self.write_record(r, false)?;
        Ok(ApplyOutcome::Applied)
    }

    /// Produce a password-protected backup. Decrypted from the device key and re-encrypted under
    /// a key derived from `password`, so it opens on any device that knows the password.
    pub fn export_bundle(&self, options: &ExportOptions, password: &str) -> Result<Vec<u8>> {
        if password.is_empty() {
            return Err(Error::Other("an export password is required".into()));
        }
        let mut wanted: Vec<&str> = match options.scope {
            ExportScope::Connections => kinds::CONNECTIONS_BACKUP.to_vec(),
            ExportScope::Everything => kinds::SYNCED
                .iter()
                .copied()
                .filter(|k| *k != kinds::SECRET)
                .collect(),
        };
        if options.include_secrets {
            wanted.push(kinds::SECRET);
        }
        let mut records = Vec::new();
        for kind in wanted {
            for row in self.fetch("kind = ?1", "ORDER BY id", &[&kind])? {
                records.push(self.sync_record(&row)?);
            }
        }
        let bundle = Bundle {
            format_version: BUNDLE_FORMAT,
            exported_at: self.now(),
            source_store_id: self.store_id.clone(),
            records,
        };
        let json = serde_json::to_vec(&bundle)
            .map_err(|e| Error::Storage(format!("cannot serialize backup: {e}")))?;
        password_seal_with(&export_kdf()?, password, EXPORT_CONTEXT, &json).map_err(crypto)
    }

    /// Merge a backup into this store. Explicit user action, so it is generous: a record is
    /// skipped only if this store already has a *newer live* version; otherwise it is written
    /// (even over a deletion) with a fresh revision, so it also wins when synced.
    pub fn import_bundle(&self, bytes: &[u8], password: &str) -> Result<ImportReport> {
        let json = password_open(password, EXPORT_CONTEXT, bytes).map_err(|e| match e {
            CryptoError::Decrypt => Error::Other("wrong password, or the file is damaged".into()),
            CryptoError::Format(_) => Error::Other("not a fastgrid backup file".into()),
            CryptoError::UnsupportedVersion(_) => {
                Error::Unsupported("this backup was written by a newer version of fastgrid".into())
            }
            other => crypto(other),
        })?;
        let bundle: Bundle = serde_json::from_slice(&json)
            .map_err(|e| Error::Other(format!("backup is corrupted: {e}")))?;
        if bundle.format_version > BUNDLE_FORMAT {
            return Err(Error::Unsupported(
                "this backup was written by a newer version of fastgrid".into(),
            ));
        }

        let mut report = ImportReport::default();
        for rec in bundle.records {
            if !kinds::is_synced(&rec.kind) {
                report.ignored_unknown_kind += 1;
                continue;
            }
            if rec.deleted_at.is_some() || rec.data.is_none() {
                continue;
            }
            let local = self.fetch_any(&rec.kind, &rec.id)?;
            if let Some(l) = &local
                && l.deleted_at.is_none()
                && l.rev > rec.rev
            {
                report.skipped_newer_local += 1;
                continue;
            }
            let rev = local.map_or(rec.rev, |l| l.rev.max(rec.rev)) + 1;
            self.write_record(
                &SyncRecord {
                    rev,
                    device_id: self.device_id.clone(),
                    updated_at: self.now(),
                    deleted_at: None,
                    ..rec
                },
                true,
            )?;
            report.applied += 1;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{ConnectionConfig, ConnectionParams, DbKind, QueryHistoryEntry, SecretKind};

    use super::*;
    use crate::StoredConnection;
    use crate::testutil::store;

    fn conn(name: &str) -> ConnectionConfig {
        ConnectionConfig::new(
            name,
            DbKind::Sqlite,
            ConnectionParams::File {
                path: "a.db".into(),
            },
        )
    }

    fn rec(kind: &str, id: &str, rev: i64, at: i64, dev: &str) -> SyncRecord {
        SyncRecord {
            kind: kind.into(),
            id: id.into(),
            parent_id: None,
            sort_ts: None,
            rev,
            device_id: dev.into(),
            updated_at: at,
            deleted_at: None,
            data: Some(serde_json::json!({"v": rev})),
        }
    }

    #[test]
    fn dirty_tracking_survives_concurrent_edits() {
        let s = store();
        let c = s.save_connection(conn("a")).unwrap();
        let dirty = s.dirty_records(100).unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].data.as_ref().unwrap()["name"], "a");

        // Edited again while the push was in flight: cleaning the old rev must not hide it.
        let pushed = (dirty[0].kind.clone(), dirty[0].id.clone(), dirty[0].rev);
        let mut changed = c.clone();
        changed.name = "b".into();
        s.save_connection(changed).unwrap();
        s.mark_clean(&[pushed]).unwrap();
        assert_eq!(s.dirty_records(100).unwrap().len(), 1);

        let now = &s.dirty_records(100).unwrap()[0];
        s.mark_clean(&[(now.kind.clone(), now.id.clone(), now.rev)])
            .unwrap();
        assert!(s.dirty_records(100).unwrap().is_empty());
    }

    #[test]
    fn deletions_are_pushed_as_tombstones() {
        let s = store();
        let c = s.save_connection(conn("a")).unwrap();
        let d = s.dirty_records(10).unwrap();
        s.mark_clean(&[(d[0].kind.clone(), d[0].id.clone(), d[0].rev)])
            .unwrap();
        s.delete_connection(&c.id).unwrap();
        let d = s.dirty_records(10).unwrap();
        let tomb = d.iter().find(|r| r.kind == kinds::CONNECTION).unwrap();
        assert!(tomb.deleted_at.is_some() && tomb.data.is_none());
    }

    #[test]
    fn device_local_kinds_are_never_pushed() {
        let s = store();
        s.put_json(
            kinds::MCP_CLIENT,
            "m",
            None,
            None,
            &serde_json::json!({"t": 1}),
        )
        .unwrap();
        assert!(s.dirty_records(10).unwrap().is_empty());
        assert!(
            s.apply_remote(&rec(kinds::MCP_CLIENT, "m", 9, 9, "x"))
                .is_err()
        );
    }

    #[test]
    fn conflicts_resolve_by_rev_then_time_then_device() {
        let s = store();
        let get = |id: &str| s.fetch_any(kinds::SNIPPET, id).unwrap().unwrap();

        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 2, 10, "dev-b"))
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert!(!get("a").enc.is_empty());
        assert_eq!(get("a").rev, 2);

        // Older rev loses, identical is a no-op, higher rev wins.
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 1, 99, "dev-z"))
                .unwrap(),
            ApplyOutcome::Skipped
        );
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 2, 10, "dev-b"))
                .unwrap(),
            ApplyOutcome::Skipped
        );
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 3, 1, "dev-a"))
                .unwrap(),
            ApplyOutcome::Applied
        );

        // Same rev: later write wins, then the larger device id (deterministic everywhere).
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 3, 5, "dev-a"))
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 3, 5, "dev-b"))
                .unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 3, 5, "dev-a"))
                .unwrap(),
            ApplyOutcome::Skipped
        );

        // Applied records are clean: nothing to push back.
        assert!(s.dirty_records(10).unwrap().is_empty());
    }

    #[test]
    fn remote_tombstones_delete_and_stale_resurrections_lose() {
        let s = store();
        s.apply_remote(&rec(kinds::SNIPPET, "a", 1, 1, "d1"))
            .unwrap();
        let mut tomb = rec(kinds::SNIPPET, "a", 2, 2, "d2");
        tomb.data = None;
        tomb.deleted_at = Some(2);
        assert_eq!(s.apply_remote(&tomb).unwrap(), ApplyOutcome::Applied);
        assert!(
            s.get_json::<serde_json::Value>(kinds::SNIPPET, "a")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            s.apply_remote(&rec(kinds::SNIPPET, "a", 1, 1, "d1"))
                .unwrap(),
            ApplyOutcome::Skipped
        );
    }

    #[test]
    fn invalid_records_are_refused() {
        let s = store();
        let mut both = rec(kinds::SNIPPET, "a", 1, 1, "d");
        both.deleted_at = Some(1);
        assert!(s.apply_remote(&both).is_err());
        let mut neither = rec(kinds::SNIPPET, "a", 1, 1, "d");
        neither.data = None;
        assert!(s.apply_remote(&neither).is_err());
    }

    #[test]
    fn export_import_roundtrip_between_stores_with_different_keys() {
        let a = store();
        let c = a.save_connection(conn("prod-db")).unwrap();
        a.set_secret(&c.id, SecretKind::DatabasePassword, "s3cret")
            .unwrap();
        let file = a.export_bundle(&ExportOptions::default(), "pw-1").unwrap();

        let b = store();
        assert_ne!(a.store_id(), b.store_id());
        let report = b.import_bundle(&file, "pw-1").unwrap();
        assert_eq!(report.applied, 2);
        assert!(
            matches!(b.get_connection(&c.id).unwrap(), Some(StoredConnection::Ok(x)) if x.name == "prod-db")
        );
        assert_eq!(
            b.get_secret(&c.id, SecretKind::DatabasePassword)
                .unwrap()
                .as_deref(),
            Some("s3cret")
        );
        // Imported data is queued for sync.
        assert_eq!(b.dirty_records(10).unwrap().len(), 2);
    }

    #[test]
    fn wrong_password_and_foreign_files_are_rejected_clearly() {
        let a = store();
        a.save_connection(conn("x")).unwrap();
        let file = a.export_bundle(&ExportOptions::default(), "right").unwrap();
        let err = store()
            .import_bundle(&file, "wrong")
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong password"), "{err}");
        let err = store()
            .import_bundle(b"hello world, not a backup", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a fastgrid backup"), "{err}");
        assert!(a.export_bundle(&ExportOptions::default(), "").is_err());
    }

    #[test]
    fn exports_are_ciphertext_and_options_are_honoured() {
        let a = store();
        let c = a.save_connection(conn("recognisable-name")).unwrap();
        a.set_secret(&c.id, SecretKind::DatabasePassword, "recognisable-secret")
            .unwrap();
        a.append_history(&QueryHistoryEntry {
            id: fg_core::HistoryId::new(),
            connection_id: c.id.clone(),
            namespace: None,
            sql: "SELECT recognisable_sql".into(),
            executed_at: fg_core::UnixMillis(1),
            duration_ms: None,
            outcome: fg_core::Outcome::Ok,
            row_count: None,
            error: None,
            source: fg_core::QuerySource::Editor,
        })
        .unwrap();

        let file = a.export_bundle(&ExportOptions::default(), "pw").unwrap();
        for needle in [&b"recognisable"[..], b"SELECT"] {
            assert!(!file.windows(needle.len()).any(|w| w == needle));
        }

        let count = |opts: ExportOptions| {
            let f = a.export_bundle(&opts, "pw").unwrap();
            let b = store();
            let report = b.import_bundle(&f, "pw").unwrap();
            (
                report.applied,
                b.get_secret(&c.id, SecretKind::DatabasePassword)
                    .unwrap()
                    .is_some(),
            )
        };
        assert_eq!(count(ExportOptions::default()), (2, true));
        assert_eq!(
            count(ExportOptions {
                include_secrets: false,
                ..ExportOptions::default()
            }),
            (1, false)
        );
        assert_eq!(
            count(ExportOptions {
                scope: ExportScope::Everything,
                include_secrets: true
            }),
            (3, true)
        );
    }

    #[test]
    fn import_keeps_newer_local_data_but_restores_deleted_records() {
        let a = store();
        let c = a.save_connection(conn("v1")).unwrap();
        let file = a.export_bundle(&ExportOptions::default(), "pw").unwrap();

        // Local edits made after the backup win.
        let mut edited = c.clone();
        edited.name = "v2".into();
        a.save_connection(edited.clone()).unwrap();
        a.save_connection(edited).unwrap();
        let report = a.import_bundle(&file, "pw").unwrap();
        assert_eq!((report.applied, report.skipped_newer_local), (0, 1));
        assert!(
            matches!(a.get_connection(&c.id).unwrap(), Some(StoredConnection::Ok(x)) if x.name == "v2")
        );

        // A deleted connection comes back, and with a revision that beats the tombstone on sync.
        a.delete_connection(&c.id).unwrap();
        let tomb_rev = a
            .fetch_any(kinds::CONNECTION, c.id.as_str())
            .unwrap()
            .unwrap()
            .rev;
        assert_eq!(a.import_bundle(&file, "pw").unwrap().applied, 1);
        assert!(
            matches!(a.get_connection(&c.id).unwrap(), Some(StoredConnection::Ok(x)) if x.name == "v1")
        );
        assert!(
            a.fetch_any(kinds::CONNECTION, c.id.as_str())
                .unwrap()
                .unwrap()
                .rev
                > tomb_rev
        );
    }
}
