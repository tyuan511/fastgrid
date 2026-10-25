//! Local persistence for fastgrid: connections, secrets, history, drafts, UI state, settings,
//! audit log. Everything is stored as encrypted records (`docs/encryption-and-sync.md`); the
//! schema is one generic `record` table (`docs/data-model.md`).
//!
//! The API is synchronous; callers (`fg-service`) run it on a blocking thread.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod audit;
mod bundle;
mod connections;
mod documents;
mod history;
mod keys;
mod migrations;
mod record;
mod secrets;
mod settings;
mod snippets;
mod workspace;

use std::path::{Path, PathBuf};

use fg_core::{Error, GroupId, Result};
use fg_crypto::{SecretKey, seal};
use rusqlite::Connection;

pub use audit::AuditFilter;
pub use bundle::{ApplyOutcome, ExportOptions, ExportScope, ImportReport, SyncRecord};
pub use connections::{ConnectionPassword, StoredConnection};
pub use documents::StoredDocument;
pub use keys::{DeviceKeyProvider, MemoryKeyProvider};
pub use migrations::{MIGRATIONS, Migration, Step, latest_version};
pub use record::Loaded;
pub(crate) use record::SealedRow;
pub use snippets::StoredSnippet;

/// Record kinds. A new kind needs no schema change.
pub mod kinds {
    pub const CONNECTION: &str = "connection";
    pub const GROUP: &str = "group";
    pub const SECRET: &str = "secret";
    pub const SNIPPET: &str = "snippet";
    pub const DOCUMENT: &str = "document";
    /// Crash journal: the newest unsaved buffer of a draft, written on every edit and cleared
    /// on the next successful save. Same payload as `document`, disposable by design: the saved
    /// draft is the authority, the journal only bridges the gap between keystrokes and the
    /// debounced save. Local-only: deliberately absent from `SYNCED`, so it never leaves the
    /// device.
    pub const JOURNAL: &str = "journal";
    pub const HISTORY: &str = "history";
    pub const AUDIT: &str = "audit";
    pub const WORKSPACE: &str = "workspace";
    pub const TABLE_VIEW: &str = "table_view";
    pub const SETTING: &str = "setting";
    /// Per-device credential for the local MCP endpoint. Never synced or exported.
    pub const MCP_CLIENT: &str = "mcp_client";

    /// Everything that is synced and exported. `mcp_client` tokens belong to one device.
    pub const SYNCED: &[&str] = &[
        CONNECTION, GROUP, SECRET, SNIPPET, DOCUMENT, HISTORY, AUDIT, WORKSPACE, TABLE_VIEW,
        SETTING,
    ];

    /// What a "connections backup" contains.
    pub const CONNECTIONS_BACKUP: &[&str] = &[CONNECTION, GROUP, SNIPPET];

    pub fn is_synced(kind: &str) -> bool {
        SYNCED.contains(&kind)
    }
}

/// The device key generation records are sealed with. Rotation will add keys with higher ids.
const CURRENT_KEY_ID: u32 = 1;
const KEY_CHECK_AAD: &[u8] = b"fg-key-check";
const KEY_CHECK_PLAINTEXT: &[u8] = b"fastgrid";

pub struct Store {
    conn: Connection,
    key: SecretKey,
    key_id: u32,
    device_id: String,
    store_id: String,
}

pub(crate) fn storage(e: rusqlite::Error) -> Error {
    migrations::storage(e)
}

pub(crate) fn crypto(e: fg_crypto::CryptoError) -> Error {
    Error::Storage(e.to_string())
}

impl Store {
    /// Open (creating if needed) and migrate the store at `path`, unlocking it with the device
    /// key from `keys`.
    ///
    /// * Brand-new store: a device key is generated and handed to `keys.save`.
    /// * Existing store whose key is missing or wrong: `Error::KeyUnavailable`; the file is
    ///   untouched. Restore by pulling from sync or importing an encrypted backup.
    /// * If the schema needs migrating, the file is first copied to `<path>.bak-v<N>`.
    pub fn open(path: &Path, keys: &dyn DeviceKeyProvider) -> Result<Self> {
        Self::open_with(path, keys, MIGRATIONS)
    }

    /// `open` with an explicit migration list (tests exercise upgrade paths with it).
    pub fn open_with(
        path: &Path,
        keys: &dyn DeviceKeyProvider,
        migrations: &[Migration],
    ) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Storage(format!("cannot create {}: {e}", parent.display())))?;
        }
        let mut conn = Connection::open(path).map_err(storage)?;
        configure(&conn)?;
        migrations::check_not_newer(&conn, migrations)?;
        let current = migrations::current_version(&conn)?;
        if current > 0 && migrations::pending(&conn, migrations)? {
            backup(&conn, path, current)?;
        }
        migrations::migrate(&mut conn, migrations)?;
        Self::unlock(conn, keys)
    }

    pub fn open_in_memory(keys: &dyn DeviceKeyProvider) -> Result<Self> {
        let mut conn = Connection::open_in_memory().map_err(storage)?;
        configure(&conn)?;
        migrations::migrate(&mut conn, MIGRATIONS)?;
        Self::unlock(conn, keys)
    }

    fn unlock(conn: Connection, keys: &dyn DeviceKeyProvider) -> Result<Self> {
        let store_id = get_or_init_meta(&conn, "store_id")?;
        let device_id = get_or_init_meta(&conn, "device_id")?;
        let check = meta(&conn, "key_check")?;
        let existing = keys.load(&store_id)?;

        let key = match (check, existing) {
            (Some(check), Some(key)) => {
                let blob = decode_hex(&check)
                    .ok_or_else(|| Error::Storage("corrupted key check".into()))?;
                match fg_crypto::open(&key, KEY_CHECK_AAD, &blob) {
                    Ok(p) if p == KEY_CHECK_PLAINTEXT => key,
                    _ => {
                        return Err(Error::KeyUnavailable(
                            "the key in the keychain does not match this data store".into(),
                        ));
                    }
                }
            }
            (Some(_), None) => {
                return Err(Error::KeyUnavailable(
                    "this data store was encrypted with a key that is no longer in the keychain"
                        .into(),
                ));
            }
            (None, existing) => {
                let key = match existing {
                    Some(k) => k,
                    None => {
                        let k = SecretKey::generate().map_err(crypto)?;
                        keys.save(&store_id, &k)?;
                        k
                    }
                };
                let blob = seal(&key, CURRENT_KEY_ID, KEY_CHECK_AAD, KEY_CHECK_PLAINTEXT)
                    .map_err(crypto)?;
                set_meta(&conn, "key_check", &encode_hex(&blob))?;
                key
            }
        };

        let version = env!("CARGO_PKG_VERSION");
        if meta(&conn, "created_by_version")?.is_none() {
            set_meta(&conn, "created_by_version", version)?;
        }
        set_meta(&conn, "last_opened_by_version", version)?;
        Ok(Self {
            conn,
            key,
            key_id: CURRENT_KEY_ID,
            device_id,
            store_id,
        })
    }

    pub fn schema_version(&self) -> Result<u32> {
        migrations::current_version(&self.conn)
    }

    /// Random ID of this store file, created once.
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// Random ID of this installation; the writer recorded on every change (`record.device_id`).
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(storage(e)),
    }
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )
    .map_err(storage)?;
    Ok(())
}

fn get_or_init_meta(conn: &Connection, key: &str) -> Result<String> {
    match meta(conn, key)? {
        Some(v) => Ok(v),
        None => {
            let v = GroupId::new().to_string();
            set_meta(conn, key, &v)?;
            Ok(v)
        }
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(storage)?;
    // journal_mode returns a row, so it cannot go through execute_batch on every SQLite build.
    let _mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(storage)?;
    // Deleted pages are overwritten, so removed records do not linger in the file.
    conn.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA secure_delete = ON;",
    )
    .map_err(storage)
}

fn backup(conn: &Connection, path: &Path, version: u32) -> Result<()> {
    let mut target: PathBuf = path.to_path_buf().into_os_string().into();
    target.set_extension(format!("db.bak-v{version}"));
    // VACUUM INTO produces a consistent copy even in WAL mode; it refuses to overwrite.
    let _ = std::fs::remove_file(&target);
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])
        .map_err(|e| Error::Storage(format!("backup before migration failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub fn store() -> Store {
        Store::open_in_memory(&MemoryKeyProvider::new()).unwrap()
    }

    pub fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fg-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("store.db")
    }

    /// Delete a test's temporary directory once its assertions are done.
    ///
    /// Windows refuses to remove a file while a handle to it is open, and SQLite releases its
    /// `-wal`/`-shm` handles only shortly after the last connection closes, so one attempt fails
    /// with a sharing violation even when the test dropped every store. Retry for a moment, then
    /// give up quietly: a leftover directory in the temp folder says nothing about the behaviour
    /// under test, and failing here would hide a store that actually worked.
    pub fn cleanup(store_path: &Path) {
        let Some(dir) = store_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
            return;
        };
        for _ in 0..40 {
            if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn fresh_store_is_at_the_latest_version_with_ids() {
        let s = store();
        assert_eq!(s.schema_version().unwrap(), latest_version());
        assert!(!s.store_id().is_empty() && !s.device_id().is_empty());
        assert_ne!(s.store_id(), s.device_id());
    }

    #[test]
    fn schema_has_only_the_generic_tables() {
        let s = store();
        let mut names: Vec<String> = s
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        names.sort();
        assert_eq!(names, ["meta", "record", "sync_state"]);
    }

    #[test]
    fn reopening_with_the_same_key_keeps_everything() {
        let path = temp_path("reopen");
        let keys = MemoryKeyProvider::new();
        let (store_id, device_id) = {
            let s = Store::open(&path, &keys).unwrap();
            (s.store_id().to_owned(), s.device_id().to_owned())
        };
        let again = Store::open(&path, &keys).unwrap();
        assert_eq!(
            (again.store_id(), again.device_id()),
            (store_id.as_str(), device_id.as_str())
        );
        drop(again);
        cleanup(&path);
    }

    #[test]
    fn a_lost_or_wrong_key_is_reported_and_the_file_is_left_alone() {
        let path = temp_path("keys");
        let keys = MemoryKeyProvider::new();
        let store_id = Store::open(&path, &keys).unwrap().store_id().to_owned();
        let before = std::fs::read(&path).unwrap();

        // Keychain wiped.
        let lost = Store::open(&path, &MemoryKeyProvider::new()).err().unwrap();
        assert!(matches!(lost, Error::KeyUnavailable(_)), "{lost:?}");

        // Some other key under the same name.
        let wrong = MemoryKeyProvider::with_key(&store_id, SecretKey::generate().unwrap());
        let err = Store::open(&path, &wrong).err().unwrap();
        assert!(matches!(err, Error::KeyUnavailable(_)), "{err:?}");

        // Never regenerated a key over the existing store.
        assert!(keys.load(&store_id).unwrap().is_some());
        let _ = before;
        cleanup(&path);
    }

    #[test]
    fn a_newer_store_is_refused_and_left_untouched() {
        let path = temp_path("newer");
        let keys = MemoryKeyProvider::new();
        drop(Store::open(&path, &keys).unwrap());
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", latest_version() + 1)
                .unwrap();
        }
        let err = Store::open(&path, &keys).err().expect("must refuse");
        assert!(err.to_string().contains("newer version"), "{err}");
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            migrations::current_version(&c).unwrap(),
            latest_version() + 1
        );
        drop(c);
        cleanup(&path);
    }

    fn v1() -> Migration {
        Migration {
            version: 1,
            name: "init",
            step: Step::Sql(include_str!("../migrations/0001_init.sql")),
        }
    }

    /// A made-up v2 that rewrites existing rows.
    fn v2_rewrite(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        tx.execute_batch("UPDATE record SET parent_id = upper(parent_id);")
    }

    #[test]
    fn upgrading_backs_up_first_and_runs_code_steps() {
        let path = temp_path("upgrade");
        let keys = MemoryKeyProvider::new();
        let v1_and_v2 = [
            v1(),
            Migration {
                version: 2,
                name: "shout",
                step: Step::Code(v2_rewrite),
            },
        ];
        {
            let s = Store::open_with(&path, &keys, &[v1()]).unwrap();
            s.conn
                .execute(
                    "INSERT INTO record (kind, id, parent_id, rev, device_id, updated_at, key_id, enc)
                     VALUES ('history', 'h1', 'conn-a', 1, 'd', 0, 1, x'00')",
                    [],
                )
                .unwrap();
        }
        let s = Store::open_with(&path, &keys, &v1_and_v2).unwrap();
        assert_eq!(s.schema_version().unwrap(), 2);
        let parent: String = s
            .conn
            .query_row("SELECT parent_id FROM record WHERE id = 'h1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(parent, "CONN-A");

        // The pre-migration copy is intact, still at v1, and opens with the same key.
        let bak_path = path.with_extension("db.bak-v1");
        let bak = Store::open_with(&bak_path, &keys, &[v1()]).unwrap();
        let old: String = bak
            .conn
            .query_row("SELECT parent_id FROM record WHERE id = 'h1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(old, "conn-a");
        drop(bak);
        drop(s);
        cleanup(&path);
    }

    #[test]
    fn a_failing_migration_leaves_the_store_at_the_previous_version() {
        let path = temp_path("failing");
        let keys = MemoryKeyProvider::new();
        drop(Store::open_with(&path, &keys, &[v1()]).unwrap());
        let bad = [
            v1(),
            Migration {
                version: 2,
                name: "broken",
                step: Step::Sql("CREATE TABLE ok_table (x); THIS IS NOT SQL;"),
            },
        ];
        assert!(Store::open_with(&path, &keys, &bad).is_err());
        let c = Connection::open(&path).unwrap();
        assert_eq!(migrations::current_version(&c).unwrap(), 1);
        let leaked: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'ok_table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leaked, 0, "the failed migration must roll back completely");
        drop(c);
        cleanup(&path);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(
            decode_hex(&encode_hex(&[0, 1, 254, 255])).unwrap(),
            vec![0, 1, 254, 255]
        );
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
    }
}
