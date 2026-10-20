#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use fg_core::{
    ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Error, ObjectKind, ObjectPath,
    TypeCategory, Value,
};
use fg_driver::{Event, ExecOptions, Session, driver_for};

fn config(path: &str) -> ConnectionConfig {
    ConnectionConfig {
        id: ConnectionId("t".into()),
        name: "test".into(),
        kind: DbKind::Sqlite,
        params: ConnectionParams::File { path: path.into() },
        options: BTreeMap::new(),
        is_production: false,
    }
}

async fn memory() -> Box<dyn Session> {
    driver_for(DbKind::Sqlite)
        .unwrap()
        .connect(&config(":memory:"), None)
        .await
        .unwrap()
}

async fn run(s: &dyn Session, sql: &str) -> Vec<fg_driver::ResultSet> {
    s.execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
}

async fn seeded() -> Box<dyn Session> {
    let s = memory().await;
    run(
        &*s,
        "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE books (
             id INTEGER PRIMARY KEY,
             title VARCHAR(100) NOT NULL DEFAULT 'untitled',
             author_id INTEGER REFERENCES authors(id) ON DELETE CASCADE,
             published DATE,
             price DECIMAL(8,2)
         );
         CREATE UNIQUE INDEX books_title ON books(title);
         CREATE VIEW book_titles AS SELECT title FROM books;
         INSERT INTO authors VALUES (1, 'Ann'), (2, 'Bob');",
    )
    .await;
    s
}

#[tokio::test]
async fn ping_and_version() {
    let s = memory().await;
    s.ping().await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("SQLite 3"));
}

#[tokio::test]
async fn multi_statement_yields_one_result_set_per_statement() {
    let s = memory().await;
    let sets = run(
        &*s,
        "CREATE TABLE t (a INTEGER, b TEXT); INSERT INTO t VALUES (1, 'x'), (2, 'y'); SELECT * FROM t ORDER BY a;",
    )
    .await;
    assert_eq!(sets.len(), 3);
    assert!(sets[0].columns.is_empty());
    assert_eq!(sets[1].affected_rows, Some(2));
    assert_eq!(sets[2].columns[0].name, "a");
    assert_eq!(sets[2].columns[0].data_type.category, TypeCategory::Integer);
    assert_eq!(
        sets[2].rows,
        vec![
            vec![Value::Int(1), Value::Text("x".into())],
            vec![Value::Int(2), Value::Text("y".into())]
        ]
    );
}

#[tokio::test]
async fn parameters_are_bound() {
    let s = memory().await;
    run(&*s, "CREATE TABLE t (a INTEGER, b TEXT, c BLOB)").await;
    let params = [
        Value::Int(7),
        Value::Text("it's".into()),
        Value::Bytes(vec![1, 2, 3]),
    ];
    s.execute(
        "INSERT INTO t VALUES (?, ?, ?)",
        &params,
        ExecOptions::default(),
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();
    let sets = run(&*s, "SELECT a, b, c FROM t").await;
    assert_eq!(sets[0].rows, vec![params.to_vec()]);
}

#[tokio::test]
async fn rows_stream_in_batches() {
    let s = memory().await;
    let mut stream = s
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10) SELECT i FROM n",
            &[],
            ExecOptions {
                batch_size: 4,
                max_rows: None,
            },
        )
        .await
        .unwrap();
    let mut batch_sizes = Vec::new();
    while let Some(ev) = stream.next().await {
        if let Event::Rows(rows) = ev.unwrap() {
            batch_sizes.push(rows.len());
        }
    }
    assert_eq!(batch_sizes, vec![4, 4, 2]);
}

#[tokio::test]
async fn max_rows_truncates_and_flags_it() {
    let s = memory().await;
    let q = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10) SELECT i FROM n";
    let opts = |max| ExecOptions {
        batch_size: 3,
        max_rows: Some(max),
    };
    let cut = s
        .execute(q, &[], opts(5))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(cut[0].rows.len(), 5);
    assert!(cut[0].truncated);

    let exact = s
        .execute(q, &[], opts(10))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(exact[0].rows.len(), 10);
    assert!(!exact[0].truncated);
}

#[tokio::test]
async fn sql_errors_are_structured() {
    let s = memory().await;
    let err = s
        .execute("SELEC 1", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    match err {
        Error::Query(q) => {
            assert!(q.message.contains("syntax error"), "{}", q.message);
            assert!(q.code.is_some());
            assert_eq!(q.position, Some(0));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn metadata_roundtrip() {
    let s = seeded().await;
    assert_eq!(s.list_namespaces().await.unwrap(), vec!["main"]);

    let objects = s.list_objects(None).await.unwrap();
    let names: Vec<_> = objects
        .iter()
        .map(|o| (o.path.name.as_str(), o.kind))
        .collect();
    assert_eq!(
        names,
        vec![
            ("authors", ObjectKind::Table),
            ("book_titles", ObjectKind::View),
            ("books", ObjectKind::Table),
        ]
    );

    let detail = s.describe(&ObjectPath::new(None, "books")).await.unwrap();
    assert_eq!(detail.primary_key(), vec!["id"]);
    let title = detail.columns.iter().find(|c| c.name == "title").unwrap();
    assert!(!title.nullable);
    assert_eq!(title.default.as_deref(), Some("'untitled'"));
    let cat = |n: &str| {
        detail
            .columns
            .iter()
            .find(|c| c.name == n)
            .unwrap()
            .data_type
            .category
    };
    assert_eq!(cat("published"), TypeCategory::Date);
    assert_eq!(cat("price"), TypeCategory::Decimal);
    assert_eq!(cat("author_id"), TypeCategory::Integer);

    assert!(
        detail
            .indexes
            .iter()
            .any(|i| i.name == "books_title" && i.unique)
    );
    let fk = &detail.foreign_keys[0];
    assert_eq!(fk.columns, vec!["author_id"]);
    assert_eq!(fk.referenced.name, "authors");
    assert_eq!(fk.referenced_columns, vec!["id"]);
    assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));

    let ddl = s
        .ddl(&ObjectPath::new(None, "authors"))
        .await
        .unwrap()
        .unwrap();
    assert!(ddl.starts_with("CREATE TABLE authors"));

    assert!(s.describe(&ObjectPath::new(None, "nope")).await.is_err());
}

#[tokio::test]
async fn read_only_session_blocks_writes() {
    let s = seeded().await;
    s.set_read_only(true).await.unwrap();
    let err = s
        .execute(
            "INSERT INTO authors VALUES (3, 'Cy')",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Query(_)), "{err:?}");
    assert_eq!(
        run(&*s, "SELECT count(*) FROM authors").await[0].rows[0][0],
        Value::Int(2)
    );

    s.set_read_only(false).await.unwrap();
    run(&*s, "INSERT INTO authors VALUES (3, 'Cy')").await;
}

#[tokio::test]
async fn cancel_interrupts_a_long_query() {
    let s = memory().await;
    let stream = s
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000000000) SELECT sum(i) FROM n",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let cancel = s.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let result = tokio::time::timeout(Duration::from_secs(10), stream.collect())
        .await
        .expect("query was not cancelled in time");
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");

    // The session stays usable afterwards.
    s.ping().await.unwrap();
}

#[tokio::test]
async fn cancel_before_the_statement_starts_is_not_lost() {
    let s = memory().await;
    let stream = s
        .execute("SELECT 1", &[], ExecOptions::default())
        .await
        .unwrap();
    stream.cancel();
    // Either the statement already finished or it was cancelled; it must never hang.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.collect())
        .await
        .expect("hung");
}

#[tokio::test]
async fn missing_file_is_not_created_implicitly() {
    let dir = std::env::temp_dir().join(format!("fg-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nope.db");
    let driver = driver_for(DbKind::Sqlite).unwrap();

    assert!(
        driver
            .connect(&config(path.to_str().unwrap()), None)
            .await
            .is_err()
    );
    assert!(!path.exists());

    let mut cfg = config(path.to_str().unwrap());
    cfg.options.insert("create".into(), "true".into());
    driver
        .connect(&cfg, None)
        .await
        .unwrap()
        .ping()
        .await
        .unwrap();
    assert!(path.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn other_engines_are_not_available_yet() {
    assert!(matches!(
        driver_for(DbKind::Postgres),
        Err(Error::Unsupported(_))
    ));
}
