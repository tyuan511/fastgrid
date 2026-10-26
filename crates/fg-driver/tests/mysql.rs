//! Behavior tests against a real MySQL / MariaDB, mirroring `postgres.rs`.
//!
//! The server is expected at `FG_TEST_MYSQL_URL` (a `mysql://user:pass@host:port/`
//! URL) or at the default local scratch server
//! `mysql://fgtest:fgtest@127.0.0.1:53306/` when the variable is unset. When
//! the server is unreachable and no URL was explicitly configured, each test
//! prints a skip notice and passes vacuously, so machines without a server
//! still pass the gate; an explicitly configured but unreachable server fails
//! loudly instead.
//!
//! Every test uses its own database so the tests can run in parallel.
//!
//! Skips are a trap: a dead server makes every test pass vacuously. Always
//! run this file with `-- --nocapture` and confirm no `skipping` line appears;
//! a run with skips proves nothing about the driver.
//!
//! MariaDB vs MySQL: the local server is MariaDB 10.11, which has no real
//! `JSON` type (`JSON` is an alias for `LONGTEXT`). Tests that depend on a
//! MySQL-only feature skip on MariaDB with a notice instead of asserting
//! MariaDB's aliasing behavior as if it were the feature.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, Error, ObjectKind, ObjectPath, Value};
use fg_driver::{Event, ExecOptions, Session, driver_for};

struct Target {
    host: String,
    port: u16,
    user: String,
    password: String,
    explicit: bool,
}

fn target() -> Target {
    let url = std::env::var("FG_TEST_MYSQL_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let explicit = url.is_some();
    let url = url.unwrap_or_else(|| "mysql://fgtest:fgtest@127.0.0.1:53306/".to_owned());
    let rest = url.strip_prefix("mysql://").unwrap_or(&url);
    let (userinfo, hostpath) = rest.split_once('@').unwrap_or(("", rest));
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u.to_owned(), p.to_owned()),
        None => (userinfo.to_owned(), String::new()),
    };
    let (hostport, _) = hostpath.split_once('/').unwrap_or((hostpath, ""));
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(3306)),
        None => (hostport.to_owned(), 3306),
    };
    Target {
        host,
        port,
        user,
        password,
        explicit,
    }
}

fn config_for(t: &Target, database: Option<&str>) -> ConnectionConfig {
    ConnectionConfig::new(
        "mysql-test",
        DbKind::MySql,
        ConnectionParams::Network {
            host: t.host.clone(),
            port: t.port,
            user: t.user.clone(),
            database: database.map(str::to_owned),
            socket: None,
        },
    )
}

/// Connect to `database`, or return `None` when the server is absent and the
/// caller should skip. Panics when the server was explicitly configured.
async fn try_connect(t: &Target, database: Option<&str>) -> Option<Box<dyn Session>> {
    let password = if t.password.is_empty() {
        None
    } else {
        Some(t.password.as_str())
    };
    match driver_for(DbKind::MySql)
        .unwrap()
        .connect(&config_for(t, database), password)
        .await
    {
        Ok(s) => Some(s),
        Err(e) => {
            if t.explicit {
                panic!("configured MySQL server unreachable: {e:?}");
            }
            eprintln!("skipping mysql behavior tests: server unreachable ({e:?})");
            None
        }
    }
}

async fn run(s: &dyn Session, sql: &str) -> Vec<fg_driver::ResultSet> {
    s.execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
}

async fn run_opts(s: &dyn Session, sql: &str, options: ExecOptions) -> Vec<fg_driver::ResultSet> {
    s.execute(sql, &[], options)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
}

/// A fresh database for one test, or `None` when the server is absent.
async fn fresh_db(t: &Target, name: &str) -> Option<Box<dyn Session>> {
    let admin = try_connect(t, None).await?;
    run(&*admin, &format!("DROP DATABASE IF EXISTS `{name}`")).await;
    run(&*admin, &format!("CREATE DATABASE `{name}`")).await;
    drop(admin);
    try_connect(t, Some(name)).await
}

async fn seeded(t: &Target, name: &str) -> Option<Box<dyn Session>> {
    let s = fresh_db(t, name).await?;
    run(
        &*s,
        "CREATE TABLE authors (id INT PRIMARY KEY, name VARCHAR(100) NOT NULL);
         CREATE TABLE books (
             id INT PRIMARY KEY AUTO_INCREMENT,
             title VARCHAR(100) NOT NULL DEFAULT 'untitled',
             author_id INT REFERENCES authors(id) ON DELETE CASCADE,
             published DATE,
             price DECIMAL(8,2),
             note VARCHAR(100) COMMENT 'a note'
         );
         CREATE UNIQUE INDEX books_title ON books(title);
         CREATE VIEW book_titles AS SELECT title FROM books;
         INSERT INTO authors (id, name) VALUES (1, 'octavia'), (2, 'ursula');
         INSERT INTO books (title, author_id, published, price)
             VALUES ('kindred', 1, '1979-06-01', 12.50),
                    ('left hand', 2, '1969-01-01', 9.99);",
    )
    .await;
    Some(s)
}

fn is_mariadb(version: &str) -> bool {
    version.to_lowercase().contains("mariadb")
}

#[tokio::test]
async fn ping_and_version() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_ping").await else {
        return;
    };
    s.ping().await.unwrap();
    let version = s.server_version().await.unwrap();
    assert!(!version.is_empty(), "server version must not be empty");
}

#[tokio::test]
async fn multi_statement_and_results() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_multi").await else {
        return;
    };
    let sets = run(&*s, "SELECT 1; SELECT 2; SELECT 3").await;
    assert_eq!(sets.len(), 3);
    assert_eq!(sets[0].rows, vec![vec![Value::Int(1)]]);
    assert_eq!(sets[1].rows, vec![vec![Value::Int(2)]]);
    assert_eq!(sets[2].rows, vec![vec![Value::Int(3)]]);
    for set in &sets {
        assert_eq!(set.affected_rows, None);
    }
}

#[tokio::test]
async fn parameters_bind() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_params").await else {
        return;
    };
    run(
        &*s,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50), score DECIMAL(6,2), note TEXT)",
    )
    .await;
    let sets = s
        .execute(
            "INSERT INTO t VALUES (?, ?, ?, ?)",
            &[
                Value::Int(7),
                Value::Text("seven".into()),
                Value::Decimal("8.25".into()),
                Value::Null,
            ],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets[0].affected_rows, Some(1));
    assert_eq!(sets[0].last_insert_id, None);
    let sets = s
        .execute(
            "SELECT id, name, score, note FROM t WHERE id = ?",
            &[Value::Int(7)],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(
        sets[0].rows,
        vec![vec![
            Value::Int(7),
            Value::Text("seven".into()),
            Value::Decimal("8.25".into()),
            Value::Null,
        ]]
    );
}

#[tokio::test]
async fn streaming_batches() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_stream").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (n INT)").await;
    let mut insert = String::from("INSERT INTO t VALUES ");
    for i in 0..250 {
        if i > 0 {
            insert.push(',');
        }
        insert.push_str(&format!("({i})"));
    }
    run(&*s, &insert).await;
    let mut stream = s
        .execute(
            "SELECT n FROM t ORDER BY n",
            &[],
            ExecOptions {
                batch_size: 100,
                ..ExecOptions::default()
            },
        )
        .await
        .unwrap();
    let mut batches = 0;
    let mut rows = 0;
    while let Some(event) = stream.next().await {
        if let Event::Rows(batch) = event.unwrap() {
            batches += 1;
            rows += batch.len();
        }
    }
    assert_eq!(rows, 250);
    assert_eq!(batches, 3, "250 rows at batch 100 arrive as 100/100/50");
}

#[tokio::test]
async fn max_rows_truncates() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_maxrows").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (n INT)").await;
    let mut insert = String::from("INSERT INTO t VALUES ");
    for i in 0..250 {
        if i > 0 {
            insert.push(',');
        }
        insert.push_str(&format!("({i})"));
    }
    run(&*s, &insert).await;
    let sets = run_opts(
        &*s,
        "SELECT n FROM t ORDER BY n",
        ExecOptions {
            max_rows: Some(10),
            ..ExecOptions::default()
        },
    )
    .await;
    assert_eq!(sets[0].rows.len(), 10);
    assert!(sets[0].truncated);
    // An exact fit is not truncation.
    let sets = run_opts(
        &*s,
        "SELECT n FROM t ORDER BY n LIMIT 10",
        ExecOptions {
            max_rows: Some(10),
            ..ExecOptions::default()
        },
    )
    .await;
    assert_eq!(sets[0].rows.len(), 10);
    assert!(!sets[0].truncated);
}

#[tokio::test]
async fn structured_errors() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_errors").await else {
        return;
    };
    let err = s
        .execute(
            "SELECT * FROM missing_table_xyz",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    match err {
        Error::Query(q) => {
            assert_eq!(q.code.as_deref(), Some("42S02"));
            assert!(q.message.contains("missing_table_xyz"));
        }
        other => panic!("expected a query error, got {other:?}"),
    }
    let err = s
        .execute("SELCT 1", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    match err {
        Error::Query(q) => assert_eq!(q.code.as_deref(), Some("42000")),
        other => panic!("expected a query error, got {other:?}"),
    }
}

#[tokio::test]
async fn metadata_and_relations() {
    let t = target();
    let Some(s) = seeded(&t, "mysql_meta").await else {
        return;
    };
    let namespaces = s.list_namespaces().await.unwrap();
    assert!(namespaces.contains(&"mysql_meta".to_string()));
    // Databases are namespaces, not a separate list.
    assert!(s.list_databases().await.unwrap().is_empty());

    let objects = s.list_objects(Some("mysql_meta")).await.unwrap();
    let books = objects
        .iter()
        .find(|o| o.path.name == "books")
        .expect("books table listed");
    assert_eq!(books.kind, ObjectKind::Table);
    assert!(books.row_estimate.is_some());
    let view = objects
        .iter()
        .find(|o| o.path.name == "book_titles")
        .expect("view listed");
    assert_eq!(view.kind, ObjectKind::View);

    let detail = s
        .describe(&ObjectPath::new(Some("mysql_meta"), "books"))
        .await
        .unwrap();
    assert_eq!(detail.primary_key(), vec!["id"]);
    let title = detail.column("title").expect("title column");
    assert_eq!(title.default.as_deref(), Some("'untitled'"));
    assert!(!title.nullable);
    let id = detail.column("id").expect("id column");
    assert!(id.auto_increment);
    let note = detail.column("note").expect("note column");
    assert_eq!(note.comment.as_deref(), Some("a note"));
    let unique = detail
        .indexes
        .iter()
        .find(|i| i.name == "books_title")
        .expect("unique index");
    assert!(unique.unique);
    assert!(!unique.primary);
    let fk = detail
        .foreign_keys
        .iter()
        .find(|f| f.columns == vec!["author_id".to_string()])
        .expect("fk to authors");
    assert_eq!(fk.referenced.name, "authors");
    assert_eq!(fk.referenced_columns, vec!["id".to_string()]);
    assert_eq!(fk.on_delete, Some(fg_core::ReferentialAction::Cascade));

    let relations = s.list_relations(Some("mysql_meta")).await.unwrap();
    assert!(
        relations
            .iter()
            .any(|r| r.from.name == "books" && r.key.referenced.name == "authors"),
        "books -> authors relation listed"
    );

    let ddl = s
        .ddl(&ObjectPath::new(Some("mysql_meta"), "books"))
        .await
        .unwrap()
        .expect("show create table");
    assert!(ddl.contains("CREATE TABLE"));
    assert!(ddl.contains("books"));
    let view_ddl = s
        .ddl(&ObjectPath::new(Some("mysql_meta"), "book_titles"))
        .await
        .unwrap()
        .expect("show create view");
    assert!(view_ddl.contains("CREATE"));
    assert!(view_ddl.contains("book_titles"));

    let missing = s
        .describe(&ObjectPath::new(Some("mysql_meta"), "no_such_table"))
        .await;
    assert!(missing.is_err(), "describing a missing table errors");
}

#[tokio::test]
async fn column_origins() {
    let t = target();
    let Some(s) = seeded(&t, "mysql_origins").await else {
        return;
    };
    let sets = run(&*s, "SELECT id, title FROM books ORDER BY id").await;
    let id = &sets[0].columns[0];
    assert_eq!(id.origin.as_ref().map(|o| o.table.as_str()), Some("books"));
    assert_eq!(id.origin.as_ref().map(|o| o.column.as_str()), Some("id"));
    assert_eq!(
        id.origin.as_ref().and_then(|o| o.namespace.as_deref()),
        Some("mysql_origins")
    );
    let sets = run(&*s, "SELECT 1 + 1").await;
    assert!(sets[0].columns[0].origin.is_none());
}

#[tokio::test]
async fn oversized_cells_truncate() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_cells").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (body TEXT)").await;
    run(&*s, "INSERT INTO t VALUES ('abcdefghijklmnopqrstuvwxyz')").await;
    let sets = run_opts(
        &*s,
        "SELECT body FROM t",
        ExecOptions {
            max_cell_bytes: Some(5),
            ..ExecOptions::default()
        },
    )
    .await;
    assert!(matches!(sets[0].rows[0][0], Value::Truncated(_)));
}

#[tokio::test]
async fn read_only_session_blocks_writes() {
    let t = target();
    let Some(s) = seeded(&t, "mysql_readonly").await else {
        return;
    };
    s.set_read_only(true).await.unwrap();
    let err = s
        .execute(
            "INSERT INTO authors (id, name) VALUES (99, 'nope')",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Query(_)),
        "read-only insert must fail, got {err:?}"
    );
    let sets = run(&*s, "SELECT COUNT(*) FROM authors").await;
    assert_eq!(sets[0].rows[0][0], Value::Int(2));
    s.set_read_only(false).await.unwrap();
    run(&*s, "INSERT INTO authors (id, name) VALUES (99, 'yes')").await;
    let sets = run(&*s, "SELECT COUNT(*) FROM authors").await;
    assert_eq!(sets[0].rows[0][0], Value::Int(3));
}

#[tokio::test]
async fn cancel_kills_a_sleeping_query() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_cancel").await else {
        return;
    };
    let handle = s.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel();
    });
    let started = Instant::now();
    let err = s
        .execute("SELECT SLEEP(30)", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Cancelled),
        "killed query must report Cancelled, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "cancel must land well before SLEEP(30) finishes"
    );
}

#[tokio::test]
async fn statement_timeout_kills_a_sleeping_query() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_timeout").await else {
        return;
    };
    let started = Instant::now();
    let err = s
        .execute(
            "SELECT SLEEP(30)",
            &[],
            ExecOptions {
                statement_timeout_secs: Some(1),
                ..ExecOptions::default()
            },
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Cancelled),
        "timed-out query must report Cancelled, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "timeout must land well before SLEEP(30) finishes"
    );
}

#[tokio::test]
async fn last_insert_id_and_affected_rows() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_ids").await else {
        return;
    };
    run(
        &*s,
        "CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(50))",
    )
    .await;
    let sets = run(&*s, "INSERT INTO t (name) VALUES ('a'), ('b')").await;
    assert_eq!(sets[0].affected_rows, Some(2));
    assert_eq!(sets[0].last_insert_id, Some(1));
    let sets = run(&*s, "UPDATE t SET name = 'c' WHERE id = 2").await;
    assert_eq!(sets[0].affected_rows, Some(1));
    assert_eq!(sets[0].last_insert_id, None);
    let sets = run(&*s, "SELECT id FROM t ORDER BY id").await;
    assert_eq!(sets[0].affected_rows, None);
    assert_eq!(sets[0].last_insert_id, None);
}

#[tokio::test]
async fn type_matrix() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_types").await else {
        return;
    };
    // Non-strict session: zero dates are rejected under the default
    // strict mode, and this test owns its session.
    run(&*s, "SET SESSION sql_mode = ''").await;
    run(&*s, "SET time_zone = '+00:00'").await;
    run(
        &*s,
        "CREATE TABLE types (
             flag TINYINT(1), small TINYINT, usmall TINYINT UNSIGNED,
             si SMALLINT, mi MEDIUMINT, i INT, ui INT UNSIGNED,
             bi BIGINT, ubi BIGINT UNSIGNED,
             f FLOAT, d DOUBLE, decv DECIMAL(10,3),
             e ENUM('a','b'), st SET('x','y'),
             bitv BIT(8), blobv BLOB, vb VARBINARY(4),
             dt DATE, zdt DATE, dtm DATETIME, dtm6 DATETIME(6),
             ts TIMESTAMP, tm TIME, tm2 TIME, yr YEAR,
             txt TEXT, vc VARCHAR(20)
         ) CHARACTER SET utf8mb4",
    )
    .await;
    run(
        &*s,
        "INSERT INTO types VALUES (
             1, -5, 200, -300, -70000, -2000000000, 4000000000,
             -9223372036854775807, 18446744073709551615,
             1.5, 2.25, 123.450,
             'b', 'x,y',
             b'10100101', x'00FF', x'0102',
             '2026-10-26', '0000-00-00', '2026-10-26 13:14:15', '2026-10-26 13:14:15.123456',
             '2026-10-26 13:14:15', '13:14:15', '26:03:04', 2026,
             '🦀', 'abc'
         )",
    )
    .await;
    let sets = run(&*s, "SELECT * FROM types").await;
    let row = &sets[0].rows[0];
    let col = |name: &str| {
        sets[0]
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing column {name}"))
    };
    assert_eq!(row[col("flag")], Value::Bool(true));
    assert_eq!(row[col("small")], Value::Int(-5));
    assert_eq!(row[col("usmall")], Value::UInt(200));
    assert_eq!(row[col("si")], Value::Int(-300));
    assert_eq!(row[col("mi")], Value::Int(-70000));
    assert_eq!(row[col("i")], Value::Int(-2_000_000_000));
    assert_eq!(row[col("ui")], Value::UInt(4_000_000_000));
    assert_eq!(row[col("bi")], Value::Int(-9_223_372_036_854_775_807));
    assert_eq!(row[col("ubi")], Value::UInt(18_446_744_073_709_551_615));
    assert!(
        matches!(row[col("f")], Value::Float(v) if (v - 1.5).abs() < 0.001),
        "float value: {:?}",
        row[col("f")]
    );
    assert_eq!(row[col("d")], Value::Float(2.25));
    assert_eq!(row[col("decv")], Value::Decimal("123.450".into()));
    assert_eq!(row[col("e")], Value::Text("b".into()));
    assert_eq!(row[col("st")], Value::Text("x,y".into()));
    assert_eq!(row[col("bitv")], Value::Bytes(vec![0xA5]));
    assert_eq!(row[col("blobv")], Value::Bytes(vec![0x00, 0xFF]));
    assert_eq!(row[col("vb")], Value::Bytes(vec![0x01, 0x02]));
    assert_eq!(row[col("dt")], Value::Date("2026-10-26".into()));
    assert_eq!(row[col("zdt")], Value::Date("0000-00-00".into()));
    assert_eq!(
        row[col("dtm")],
        Value::Timestamp("2026-10-26T13:14:15".into())
    );
    assert_eq!(
        row[col("dtm6")],
        Value::Timestamp("2026-10-26T13:14:15.123456".into())
    );
    assert_eq!(
        row[col("ts")],
        Value::TimestampTz("2026-10-26T13:14:15".into())
    );
    assert_eq!(row[col("tm")], Value::Time("13:14:15".into()));
    assert_eq!(row[col("tm2")], Value::Time("26:03:04".into()));
    assert_eq!(row[col("yr")], Value::Int(2026));
    assert_eq!(row[col("txt")], Value::Text("🦀".into()));
    assert_eq!(row[col("vc")], Value::Text("abc".into()));

    // Type categories on the result columns.
    let cat = |name: &str| sets[0].columns[col(name)].data_type.category;
    assert_eq!(cat("flag"), fg_core::TypeCategory::Bool);
    assert_eq!(cat("decv"), fg_core::TypeCategory::Decimal);
    assert_eq!(cat("e"), fg_core::TypeCategory::Enum);
    assert_eq!(cat("st"), fg_core::TypeCategory::Set);
    assert_eq!(cat("bitv"), fg_core::TypeCategory::Bit);
    assert_eq!(cat("blobv"), fg_core::TypeCategory::Binary);
    assert_eq!(cat("dt"), fg_core::TypeCategory::Date);
    assert_eq!(cat("ts"), fg_core::TypeCategory::TimestampTz);
}

#[tokio::test]
async fn json_type_on_mysql_only() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_json").await else {
        return;
    };
    if is_mariadb(&s.server_version().await.unwrap()) {
        eprintln!("skipping json type test: MariaDB aliases JSON to LONGTEXT");
        return;
    }
    run(&*s, "CREATE TABLE t (j JSON)").await;
    run(&*s, "INSERT INTO t VALUES ('{\"b\": 2, \"a\": 1}')").await;
    let sets = run(&*s, "SELECT j FROM t").await;
    assert_eq!(
        sets[0].columns[0].data_type.category,
        fg_core::TypeCategory::Json
    );
    assert_eq!(
        sets[0].rows[0][0],
        Value::Json("{\"a\": 1, \"b\": 2}".into())
    );
}

#[tokio::test]
async fn call_returns_multiple_result_sets() {
    let t = target();
    let Some(s) = fresh_db(&t, "mysql_call").await else {
        return;
    };
    // One execute call carries the whole compound body: the splitter must
    // not cut it at the body's inner `;`.
    run(&*s, "CREATE PROCEDURE two() BEGIN SELECT 1; SELECT 2; END").await;
    // One CALL statement, two result sets: this is the first test to drive
    // the driver's multi-set path from a single statement.
    let sets = run(&*s, "CALL two()").await;
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].rows, vec![vec![Value::Int(1)]]);
    assert_eq!(sets[1].rows, vec![vec![Value::Int(2)]]);

    // A DELIMITER script: the directives never reach the server, the
    // procedure body stays whole, and the trailing statement still runs.
    let sets = run(
        &*s,
        "DELIMITER $$\nCREATE PROCEDURE three() BEGIN SELECT 3; SELECT 4; END$$\nDELIMITER ;\nSELECT 5",
    )
    .await;
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[1].rows, vec![vec![Value::Int(5)]]);
    let sets = run(&*s, "CALL three()").await;
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].rows, vec![vec![Value::Int(3)]]);
    assert_eq!(sets[1].rows, vec![vec![Value::Int(4)]]);
}
