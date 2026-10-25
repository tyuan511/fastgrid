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

#[tokio::test]
async fn an_insert_reads_back_its_generated_key_and_defaults() {
    let env = Env::new("returning");
    let conn = env.open(None).await;
    let out = conn
        .commit_changes(
            &t(),
            &[RowChange::Insert {
                cells: vec![set("name", Value::Text("d".into()))],
            }],
        )
        .await
        .unwrap();
    assert_eq!((out.statements, out.affected_rows), (1, 1));
    assert_eq!(
        out.returned_rows,
        vec![vec![Value::Int(4), Value::Text("d".into()), Value::Null,]],
        "the generated id and the omitted column come back from the engine"
    );
    assert_eq!(names(&conn).await, ["a", "b", "c", "d"]);
}

// ---- property-style: a random changeset, executed, must match the model ----

/// Deterministic xorshift, so a failing seed reproduces the same changeset.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }

    fn bool(&mut self) -> bool {
        self.next().is_multiple_of(2)
    }
}

async fn read_all(conn: &Arc<OpenConnection>) -> Vec<(i64, String, Option<i64>)> {
    let sets = conn
        .execute(
            "SELECT id, name, n FROM t ORDER BY id",
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
        .map(|r| {
            let id = match r[0] {
                Value::Int(i) => i,
                _ => panic!("id is not an int"),
            };
            let name = match &r[1] {
                Value::Text(s) => s.clone(),
                _ => panic!("name is not text"),
            };
            let n = match r[2] {
                Value::Int(i) => Some(i),
                Value::Null => None,
                _ => panic!("n is not int or null"),
            };
            (id, name, n)
        })
        .collect()
}

#[tokio::test]
async fn random_changesets_execute_to_the_modelled_table() {
    for seed in 1..=200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let env = Env::new(&format!("prop-{seed}"));
        let conn = env.open(None).await;
        // The model starts from the table Env creates: ids 1..=3, names a/b/c.
        let mut model: std::collections::BTreeMap<i64, (String, Option<i64>)> = [
            (1, ("a".into(), Some(10))),
            (2, ("b".into(), Some(20))),
            (3, ("c".into(), Some(30))),
        ]
        .into_iter()
        .collect();
        let mut fresh = 0usize;
        let mut fresh_name = move || {
            fresh += 1;
            format!("u{seed}-{fresh}")
        };
        let mut changes = Vec::new();

        // Deletes, then updates, then inserts: the order `Changeset::changes`
        // produces, so the model and the commit apply the same sequence.
        let mut ids: Vec<i64> = model.keys().copied().collect();
        for _ in 0..rng.below(3) {
            if ids.is_empty() {
                break;
            }
            let at = rng.below(ids.len());
            let id = ids.remove(at);
            changes.push(RowChange::Delete { key: key(id) });
            model.remove(&id);
        }
        for _ in 0..rng.below(4) {
            if ids.is_empty() {
                break;
            }
            let id = ids[rng.below(ids.len())];
            let (name, n) = model.get(&id).unwrap().clone();
            let mut cells = Vec::new();
            let mut new_name = name.clone();
            let mut new_n = n;
            if rng.bool() {
                new_name = fresh_name();
                cells.push(set("name", Value::Text(new_name.clone())));
            }
            if rng.bool() {
                new_n = if rng.bool() {
                    Some(rng.below(100) as i64)
                } else {
                    None
                };
                cells.push(set("n", new_n.map(Value::Int).unwrap_or(Value::Null)));
            }
            if cells.is_empty() {
                continue;
            }
            changes.push(RowChange::Update {
                key: key(id),
                cells,
            });
            model.insert(id, (new_name, new_n));
        }
        let mut returned = Vec::new();
        for _ in 0..rng.below(4) {
            let name = fresh_name();
            let n = if rng.bool() {
                Some(rng.below(100) as i64)
            } else {
                None
            };
            let mut cells = vec![set("name", Value::Text(name.clone()))];
            // Sometimes omit `n`, sometimes spell it as a default: both mean
            // the engine picks, which here is NULL.
            let mut written_n = None;
            if rng.bool() {
                cells.push(set("n", n.map(Value::Int).unwrap_or(Value::Null)));
                written_n = n;
            } else if rng.bool() {
                cells.push(("n".into(), CellChange::Default));
            }
            changes.push(RowChange::Insert { cells });
            let id = model.keys().max().copied().unwrap_or(0) + 1;
            model.insert(id, (name.clone(), written_n));
            returned.push(vec![
                Value::Int(id),
                Value::Text(name),
                written_n.map(Value::Int).unwrap_or(Value::Null),
            ]);
        }

        let out = conn
            .commit_changes(&t(), &changes)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: commit failed: {e}"));
        assert_eq!(out.returned_rows, returned, "seed {seed}: readback");
        let expected: Vec<(i64, String, Option<i64>)> = model
            .iter()
            .map(|(id, (name, n))| (*id, name.clone(), *n))
            .collect();
        assert_eq!(read_all(&conn).await, expected, "seed {seed}");
    }
}

#[tokio::test]
async fn a_thousand_mixed_changes_commit_in_one_transaction() {
    let env = Env::new("thousand");
    // A bigger table than Env's default, built the same way.
    {
        let db = env.dir.join("d.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        let mut sql = String::from("DELETE FROM t;");
        for i in 1..=2000 {
            sql.push_str(&format!("INSERT INTO t VALUES ({i},'r{i}',{i});"));
        }
        c.execute_batch(&sql).unwrap();
    }
    let conn = env.open(None).await;
    let mut changes = Vec::new();
    for id in 1..=333 {
        changes.push(RowChange::Delete { key: key(id) });
    }
    for id in 334..=666 {
        changes.push(RowChange::Update {
            key: key(id),
            cells: vec![set("name", Value::Text(format!("u{id}")))],
        });
    }
    for n in 0..334 {
        changes.push(RowChange::Insert {
            cells: vec![set("name", Value::Text(format!("new{n}")))],
        });
    }
    let out = conn.commit_changes(&t(), &changes).await.unwrap();
    assert_eq!((out.statements, out.affected_rows), (1000, 1000));
    assert_eq!(out.returned_rows.len(), 334);
    let rows = read_all(&conn).await;
    assert_eq!(rows.len(), 2001, "2000 - 333 deleted + 334 inserted");
    assert_eq!(rows[0], (334, "u334".into(), Some(334)));
    assert_eq!(rows.last().unwrap().1, "new333");

    // The same shape with one failing change at the end rolls everything back.
    let env = Env::new("thousand-rollback");
    {
        let db = env.dir.join("d.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        let mut sql = String::from("DELETE FROM t;");
        for i in 1..=2000 {
            sql.push_str(&format!("INSERT INTO t VALUES ({i},'r{i}',{i});"));
        }
        c.execute_batch(&sql).unwrap();
    }
    let conn = env.open(None).await;
    let mut changes = Vec::new();
    for id in 1..=333 {
        changes.push(RowChange::Delete { key: key(id) });
    }
    for id in 334..=666 {
        changes.push(RowChange::Update {
            key: key(id),
            cells: vec![set("name", Value::Text(format!("u{id}")))],
        });
    }
    for n in 0..333 {
        changes.push(RowChange::Insert {
            cells: vec![set("name", Value::Text(format!("new{n}")))],
        });
    }
    // `r667` still exists, so this insert violates UNIQUE(name).
    changes.push(RowChange::Insert {
        cells: vec![set("name", Value::Text("r667".into()))],
    });
    let err = conn.commit_changes(&t(), &changes).await.unwrap_err();
    assert!(err.to_string().contains("change 1000"), "{err}");
    let rows = read_all(&conn).await;
    assert_eq!(rows.len(), 2000, "nothing from the failed batch survives");
    assert_eq!(rows[0], (1, "r1".into(), Some(1)));
}
