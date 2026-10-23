//! `OpenConnection::commit_changes` against a real SQLite file: one transaction, exact row
//! counts, and nothing left behind when a change fails.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, Error, ObjectPath, SafeMode, Value};
use fg_driver::ExecOptions;
use fg_service::{
    KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, Service, ServiceOptions,
};
use fg_sql::{CellChange, RowChange};

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-commit-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("d.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, n INTEGER);
             INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'c',30);
             CREATE TABLE nopk(a INTEGER, b INTEGER);
             CREATE TABLE pair(a INTEGER, b INTEGER, v TEXT, PRIMARY KEY(a, b));",
        )
        .unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        Self { dir, service }
    }

    async fn open(&self, safe_mode: Option<SafeMode>) -> Arc<OpenConnection> {
        let mut c = ConnectionConfig::new(
            "c",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self.dir.join("d.db").to_string_lossy().into_owned(),
            },
        );
        c.safe_mode = safe_mode;
        let c = self
            .service
            .save_connection(c, PasswordUpdate::Keep)
            .await
            .unwrap();
        self.service.open(&c.id).await.unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

fn t() -> ObjectPath {
    ObjectPath::new(None, "t")
}

fn set(c: &str, v: Value) -> (String, CellChange) {
    (c.into(), CellChange::Set(v))
}

fn key(n: i64) -> Vec<(String, Value)> {
    vec![("id".into(), Value::Int(n))]
}

async fn names(conn: &Arc<OpenConnection>) -> Vec<String> {
    let sets = conn
        .execute(
            "SELECT name FROM t ORDER BY id",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    sets[0]
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_mixed_batch_commits_together() {
    let env = Env::new("mixed");
    let conn = env.open(None).await;
    let out = conn
        .commit_changes(
            &t(),
            &[
                RowChange::Insert {
                    cells: vec![set("name", Value::Text("d".into())), set("n", Value::Null)],
                },
                RowChange::Update {
                    key: key(1),
                    cells: vec![set("name", Value::Text("a2".into()))],
                },
                RowChange::Delete { key: key(2) },
            ],
        )
        .await
        .unwrap();
    assert_eq!((out.statements, out.affected_rows), (3, 3));
    assert_eq!(names(&conn).await, ["a2", "c", "d"]);
}

#[tokio::test]
async fn one_failing_change_rolls_everything_back() {
    let env = Env::new("rollback");
    let conn = env.open(None).await;
    // The third change violates UNIQUE(name); the first two must not survive.
    let err = conn
        .commit_changes(
            &t(),
            &[
                RowChange::Delete { key: key(1) },
                RowChange::Update {
                    key: key(2),
                    cells: vec![set("name", Value::Text("zzz".into()))],
                },
                RowChange::Insert {
                    cells: vec![set("name", Value::Text("c".into()))],
                },
            ],
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("change 3"), "{err}");
    assert_eq!(names(&conn).await, ["a", "b", "c"]);

    // The session is usable afterwards and no transaction is left open.
    conn.commit_changes(&t(), &[RowChange::Delete { key: key(3) }])
        .await
        .unwrap();
    assert_eq!(names(&conn).await, ["a", "b"]);
}

#[tokio::test]
async fn a_key_that_matches_nothing_rolls_back() {
    let env = Env::new("stale");
    let conn = env.open(None).await;
    let err = conn
        .commit_changes(
            &t(),
            &[
                RowChange::Delete { key: key(1) },
                RowChange::Delete { key: key(99) },
            ],
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("change 2"), "{err}");
    assert_eq!(names(&conn).await, ["a", "b", "c"]);
}

#[tokio::test]
async fn unsafe_targets_are_refused_before_anything_runs() {
    let env = Env::new("refuse");
    let conn = env.open(None).await;

    let nopk = conn
        .commit_changes(
            &ObjectPath::new(None, "nopk"),
            &[RowChange::Delete {
                key: vec![("a".into(), Value::Int(1))],
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(nopk, Error::InvalidInput(_)), "{nopk}");

    // Half of a composite key could match many rows.
    let partial = conn
        .commit_changes(
            &ObjectPath::new(None, "pair"),
            &[RowChange::Update {
                key: vec![("a".into(), Value::Int(1))],
                cells: vec![set("v", Value::Text("x".into()))],
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(partial, Error::InvalidInput(_)), "{partial}");

    // A non-key column is not a key either.
    let wrong = conn
        .commit_changes(
            &t(),
            &[RowChange::Delete {
                key: vec![("name".into(), Value::Text("a".into()))],
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(wrong, Error::InvalidInput(_)), "{wrong}");
    assert_eq!(names(&conn).await, ["a", "b", "c"]);
}

#[tokio::test]
async fn a_read_only_connection_cannot_commit() {
    let env = Env::new("ro");
    let conn = env.open(Some(SafeMode::ReadOnly)).await;
    let err = conn
        .commit_changes(&t(), &[RowChange::Delete { key: key(1) }])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err}");
    assert_eq!(names(&conn).await, ["a", "b", "c"]);
}
