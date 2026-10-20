//! End-to-end checks that nothing readable reaches the disk, using the real file-backed store.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use fg_core::{
    Actor, AuditEntry, AuditId, ConnectionConfig, ConnectionGroup, ConnectionParams, DbKind,
    Decision, Environment, HistoryId, Outcome, QueryHistoryEntry, QuerySource, SecretKind,
    StatementClass, UnixMillis,
};
use fg_store::{AuditFilter, ExportOptions, MemoryKeyProvider, Store};

fn all_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn no_plaintext_reaches_any_file_on_disk() {
    let dir = std::env::temp_dir().join(format!("fg-enc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("store.db");
    let keys = MemoryKeyProvider::new();

    let needles = [
        "db-host.internal.example",
        "Super Secret Production",
        "p@ssw0rd-needle",
        "SELECT needle_column FROM needle_table",
        "Needle Group",
        "ssh-bastion.example",
    ];

    {
        let store = Store::open(&path, &keys).unwrap();
        let group = store
            .save_group(ConnectionGroup::new("Needle Group"))
            .unwrap();
        let mut c = ConnectionConfig::new(
            "Super Secret Production",
            DbKind::Postgres,
            ConnectionParams::Network {
                host: "db-host.internal.example".into(),
                port: 5432,
                user: "admin".into(),
                database: Some("prod".into()),
                socket: None,
            },
        );
        c.environment = Environment::Production;
        c.group_id = Some(group.id);
        c.notes = Some("ssh-bastion.example".into());
        let c = store.save_connection(c).unwrap();
        store
            .set_secret(&c.id, SecretKind::DatabasePassword, "p@ssw0rd-needle")
            .unwrap();
        store
            .append_history(&QueryHistoryEntry {
                id: HistoryId::new(),
                connection_id: c.id.clone(),
                namespace: None,
                sql: "SELECT needle_column FROM needle_table".into(),
                executed_at: UnixMillis::now(),
                duration_ms: Some(1),
                outcome: Outcome::Ok,
                row_count: Some(1),
                error: None,
                source: QuerySource::Editor,
            })
            .unwrap();
        store
            .append_audit(&AuditEntry {
                id: AuditId::new(),
                at: UnixMillis::now(),
                actor: Actor::User,
                connection_id: Some(c.id.clone()),
                connection_name: "Super Secret Production".into(),
                statement: "SELECT needle_column FROM needle_table".into(),
                class: StatementClass::Read,
                danger: None,
                decision: Decision::Allowed,
                reason: None,
                outcome: Outcome::Ok,
                error: None,
                duration_ms: None,
                row_count: None,
            })
            .unwrap();
        // The WAL file may hold the newest writes: check it while the store is still open too.
        for (name, bytes) in all_bytes(&dir) {
            for n in needles {
                assert!(!contains(&bytes, n), "{n:?} found in {name} (store open)");
            }
        }
    }

    for (name, bytes) in all_bytes(&dir) {
        for n in needles {
            assert!(!contains(&bytes, n), "{n:?} found in {name}");
        }
    }

    // And it is all still there for the right key.
    let store = Store::open(&path, &keys).unwrap();
    assert_eq!(store.load_connections().unwrap().len(), 1);
    assert_eq!(store.query_audit(&AuditFilter::default()).unwrap().len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn exported_backup_works_end_to_end_with_the_recommended_kdf() {
    let a = Store::open_in_memory(&MemoryKeyProvider::new()).unwrap();
    let c = a
        .save_connection(ConnectionConfig::new(
            "x",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: "a.db".into(),
            },
        ))
        .unwrap();
    a.set_secret(&c.id, SecretKind::DatabasePassword, "pw")
        .unwrap();
    // Integration tests are built without cfg(test), so this uses the real Argon2id cost.
    let file = a
        .export_bundle(&ExportOptions::default(), "correct horse")
        .unwrap();
    assert!(!contains(&file, "\"pw\""));
    let b = Store::open_in_memory(&MemoryKeyProvider::new()).unwrap();
    assert_eq!(b.import_bundle(&file, "correct horse").unwrap().applied, 2);
    assert!(b.import_bundle(&file, "battery staple").is_err());
}
