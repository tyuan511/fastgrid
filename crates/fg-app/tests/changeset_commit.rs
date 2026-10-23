//! The staged-edit model feeding the real commit path: a `Changeset` built from loaded rows is
//! reduced to changes and committed through the service into a real SQLite file.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fg_app::model::Changeset;
use fg_core::{ConnectionConfig, ConnectionParams, DbKind, ObjectPath, Value};
use fg_driver::ExecOptions;
use fg_service::{KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service, ServiceOptions};
use fg_sql::CellChange;

#[tokio::test]
async fn staged_edits_reach_the_database_in_one_commit() {
    let dir = std::env::temp_dir().join(format!("fg-cs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
    let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
    let mut config = ConnectionConfig::new(
        "c",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: dir.join("d.db").to_string_lossy().into_owned(),
        },
    );
    config.options.insert("create".into(), "true".into());
    let config = service
        .save_connection(config, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = service.open(&config.id).await.unwrap();
    conn.execute(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT); \
         INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')",
        &[],
        ExecOptions::default(),
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();

    let path = ObjectPath::new(None, "t");
    let detail = conn.describe(&path).await.unwrap();
    let columns: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
    let mut cs = Changeset::new(columns, &detail.primary_key());

    let row = |n: i64, s: &str| vec![Value::Int(n), Value::Text(s.into())];
    let first = cs.row_id(&row(1, "a")).unwrap();
    let second = cs.row_id(&row(2, "b")).unwrap();
    cs.edit_cell(
        &first,
        &row(1, "a"),
        1,
        CellChange::Set(Value::Text("A".into())),
    )
    .unwrap();
    cs.delete_row(&second).unwrap();
    let new = cs.insert_row().unwrap();
    cs.edit_cell(&new, &[], 1, CellChange::Set(Value::Text("d".into())))
        .unwrap();

    let outcome = conn.commit_changes(&path, &cs.changes()).await.unwrap();
    assert_eq!(outcome.statements, 3);

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
    let names: Vec<_> = sets[0].rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        names,
        [
            Value::Text("A".into()),
            Value::Text("c".into()),
            Value::Text("d".into())
        ]
    );
    drop((conn, service));
    for _ in 0..40 {
        if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
