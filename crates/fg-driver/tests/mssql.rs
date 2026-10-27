//! Behavior tests against a real SQL Server, mirroring `mysql.rs` and
//! `postgres.rs`.
//!
//! The server is expected at `FG_TEST_MSSQL_URL` (a
//! `sqlserver://user:pass@host:port/` URL) or at the default local scratch
//! server `sqlserver://sa:Fastgrid!Pass123@127.0.0.1:14330/` when the variable
//! is unset. When the server is unreachable and no URL was explicitly
//! configured, each test prints a skip notice and passes vacuously, so machines
//! without a server still pass the gate; an explicitly configured but
//! unreachable server fails loudly instead.
//!
//! Every test uses its own database so the tests can run in parallel.
//!
//! Skips are a trap: a dead server makes every test pass vacuously. Always
//! run this file with `-- --nocapture` and confirm no `skipping` line appears;
//! a run with skips proves nothing about the driver.

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
    let url = std::env::var("FG_TEST_MSSQL_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let explicit = url.is_some();
    let url = url.unwrap_or_else(|| "sqlserver://sa:Fastgrid!Pass123@127.0.0.1:14330/".to_owned());
    let rest = url
        .strip_prefix("sqlserver://")
        .or_else(|| url.strip_prefix("mssql://"))
        .unwrap_or(&url);
    let (userinfo, hostpath) = rest.split_once('@').unwrap_or(("", rest));
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u.to_owned(), p.to_owned()),
        None => (userinfo.to_owned(), String::new()),
    };
    let (hostport, _) = hostpath.split_once('/').unwrap_or((hostpath, ""));
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(1433)),
        None => (hostport.to_owned(), 1433),
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
        "mssql-test",
        DbKind::MsSql,
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
    match driver_for(DbKind::MsSql)
        .unwrap()
        .connect(&config_for(t, database), password)
        .await
    {
        Ok(s) => Some(s),
        Err(e) => {
            if t.explicit {
                panic!("configured SQL Server unreachable: {e:?}");
            }
            eprintln!("skipping mssql behavior tests: server unreachable ({e:?})");
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
///
/// `DROP DATABASE` cannot run against the database being dropped, so the
/// setup happens on the login's default database (`master`).
async fn fresh_db(t: &Target, name: &str) -> Option<Box<dyn Session>> {
    let admin = try_connect(t, None).await?;
    // `SINGLE_USER` with rollback kicks out leftover sessions from a previous
    // failed run, which would otherwise make `DROP DATABASE` fail.
    let _ = run(
        &*admin,
        &format!("IF DB_ID('{name}') IS NOT NULL DROP DATABASE [{name}]"),
    )
    .await;
    run(&*admin, &format!("CREATE DATABASE [{name}]")).await;
    drop(admin);
    try_connect(t, Some(name)).await
}

async fn seeded(t: &Target, name: &str) -> Option<Box<dyn Session>> {
    let s = fresh_db(t, name).await?;
    run(
        &*s,
        "CREATE TABLE authors (id INT PRIMARY KEY, name NVARCHAR(100) NOT NULL);
         CREATE TABLE books (
             id INT IDENTITY(1,1) PRIMARY KEY,
             title NVARCHAR(100) NOT NULL DEFAULT 'untitled',
             author_id INT REFERENCES authors(id) ON DELETE CASCADE,
             published DATE,
             price DECIMAL(8,2),
             note NVARCHAR(100)
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

#[tokio::test]
async fn ping_and_version() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_ping").await else {
        return;
    };
    s.ping().await.unwrap();
    let version = s.server_version().await.unwrap();
    assert!(
        version.contains("Microsoft SQL Server"),
        "unexpected server version: {version}"
    );
}

#[tokio::test]
async fn multi_statement_yields_one_result_set_per_statement() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_multi").await else {
        return;
    };
    let sets = run(
        &*s,
        "CREATE TABLE t (a INT, b NVARCHAR(50));
         INSERT INTO t VALUES (1, 'x'), (2, 'y');
         SELECT * FROM t ORDER BY a;",
    )
    .await;
    assert_eq!(sets.len(), 3, "one set per statement: {sets:?}");
    assert!(sets[0].columns.is_empty());
    assert_eq!(sets[1].affected_rows, Some(2));
    assert_eq!(sets[2].columns.len(), 2);
    assert_eq!(sets[2].columns[0].name, "a");
    assert_eq!(
        sets[2].columns[0].data_type.category,
        fg_core::TypeCategory::Integer
    );
    assert_eq!(
        sets[2].rows,
        vec![
            vec![Value::Int(1), Value::Text("x".into())],
            vec![Value::Int(2), Value::Text("y".into())],
        ]
    );
}

#[tokio::test]
async fn parameters_bind() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_params").await else {
        return;
    };
    run(
        &*s,
        "CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(50), score DECIMAL(6,2), \
         data VARBINARY(10), note NVARCHAR(50) NULL)",
    )
    .await;
    let sets = s
        .execute(
            "INSERT INTO t VALUES (@P1, @P2, @P3, @P4, @P5)",
            &[
                Value::Int(7),
                Value::Text("seven".into()),
                Value::Decimal("8.25".into()),
                Value::Bytes(vec![1, 2, 3]),
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

    let sets = s
        .execute(
            "SELECT id, name, score, data, note FROM t WHERE id = @P1",
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
            Value::Bytes(vec![1, 2, 3]),
            Value::Null,
        ]]
    );
}

#[tokio::test]
async fn parameters_bind_to_a_single_statement() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_params_one").await else {
        return;
    };
    let err = s
        .execute(
            "SELECT @P1; SELECT @P1",
            &[Value::Int(1)],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
}

#[tokio::test]
async fn streaming_batches() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_stream").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (n INT)").await;
    run(
        &*s,
        "INSERT INTO t (n) VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)",
    )
    .await;
    let mut stream = s
        .execute(
            "SELECT n FROM t ORDER BY n",
            &[],
            ExecOptions {
                batch_size: 4,
                ..ExecOptions::default()
            },
        )
        .await
        .unwrap();
    let mut batch_sizes = Vec::new();
    while let Some(event) = stream.next().await {
        if let Ok(Event::Rows(rows)) = event {
            batch_sizes.push(rows.len());
        }
    }
    assert_eq!(batch_sizes, vec![4, 4, 2]);
}

#[tokio::test]
async fn max_rows_truncates_and_flags_it() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_maxrows").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (n INT)").await;
    run(&*s, "INSERT INTO t (n) VALUES (1),(2),(3),(4),(5)").await;
    let sets = run_opts(
        &*s,
        "SELECT n FROM t ORDER BY n",
        ExecOptions {
            max_rows: Some(3),
            ..ExecOptions::default()
        },
    )
    .await;
    assert_eq!(sets[0].rows.len(), 3);
    assert!(sets[0].truncated);
}

#[tokio::test]
async fn oversized_cells_truncate() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_cells").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (body NVARCHAR(MAX))").await;
    let long = "x".repeat(500);
    run(&*s, &format!("INSERT INTO t (body) VALUES ('{long}')")).await;
    let sets = run_opts(
        &*s,
        "SELECT body FROM t",
        ExecOptions {
            max_cell_bytes: Some(50),
            ..ExecOptions::default()
        },
    )
    .await;
    match &sets[0].rows[0][0] {
        Value::Truncated(t) => {
            assert_eq!(t.total_len, 500);
            assert!(t.preview.len() <= 50);
        }
        other => panic!("expected a truncated value, got {other:?}"),
    }
}

#[tokio::test]
async fn structured_errors_carry_the_server_code() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_errors").await else {
        return;
    };
    let err = s
        .execute("SELECT * FROM no_such_table", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    match err {
        Error::Query(q) => {
            assert_eq!(q.code.as_deref(), Some("208"), "{q:?}");
            assert!(
                q.message.contains("Invalid object name"),
                "unexpected message: {}",
                q.message
            );
        }
        other => panic!("expected a query error, got {other:?}"),
    }
    // The session survives a failed statement.
    let sets = run(&*s, "SELECT 1 AS one").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(1)]]);
}

#[tokio::test]
async fn metadata_and_relations() {
    let t = target();
    let Some(s) = seeded(&t, "fg_mssql_meta").await else {
        return;
    };
    let namespaces = s.list_namespaces().await.unwrap();
    assert!(namespaces.iter().any(|n| n == "dbo"), "{namespaces:?}");

    let objects = s.list_objects(Some("dbo")).await.unwrap();
    let books = objects
        .iter()
        .find(|o| o.path.name == "books")
        .expect("books table");
    assert_eq!(books.kind, ObjectKind::Table);
    assert!(books.row_estimate.unwrap_or(0) >= 2);
    let view = objects
        .iter()
        .find(|o| o.path.name == "book_titles")
        .expect("view");
    assert_eq!(view.kind, ObjectKind::View);

    let detail = s
        .describe(&ObjectPath::new(Some("dbo"), "books"))
        .await
        .unwrap();
    assert_eq!(detail.primary_key(), vec!["id"]);
    let id = detail.column("id").expect("id column");
    assert!(id.auto_increment, "IDENTITY column: {id:?}");
    let title = detail.column("title").expect("title column");
    assert_eq!(title.default.as_deref(), Some("'untitled'"));
    assert!(!title.nullable);
    let price = detail.column("price").expect("price column");
    assert_eq!(price.data_type.category, fg_core::TypeCategory::Decimal);
    assert_eq!(price.data_type.params.precision, Some(8));
    assert_eq!(price.data_type.params.scale, Some(2));

    let index = detail
        .indexes
        .iter()
        .find(|i| i.name == "books_title")
        .expect("unique index");
    assert!(index.unique);
    assert_eq!(index.column_names(), vec!["title"]);

    let fk = detail
        .foreign_keys
        .iter()
        .find(|f| f.columns == vec!["author_id".to_owned()])
        .expect("author_id foreign key");
    assert_eq!(fk.referenced.name, "authors");
    assert_eq!(fk.referenced_columns, vec!["id".to_owned()]);
    assert_eq!(
        fk.on_delete,
        Some(fg_core::ReferentialAction::Cascade),
        "{fk:?}"
    );

    let relations = s.list_relations(Some("dbo")).await.unwrap();
    assert!(
        relations
            .iter()
            .any(|r| r.from.name == "books" && r.key.referenced.name == "authors"),
        "{relations:?}"
    );

    let ddl = s
        .ddl(&ObjectPath::new(Some("dbo"), "books"))
        .await
        .unwrap()
        .expect("ddl");
    assert!(ddl.starts_with("CREATE TABLE [dbo].[books]"), "{ddl}");
    assert!(ddl.contains("IDENTITY(1,1)"), "{ddl}");
    assert!(ddl.contains("books_title"), "{ddl}");
}

#[tokio::test]
async fn ddl_for_a_view_comes_from_the_catalogue() {
    let t = target();
    let Some(s) = seeded(&t, "fg_mssql_viewddl").await else {
        return;
    };
    let ddl = s
        .ddl(&ObjectPath::new(Some("dbo"), "book_titles"))
        .await
        .unwrap()
        .expect("view ddl");
    assert!(ddl.contains("CREATE VIEW"), "{ddl}");
    assert!(ddl.contains("book_titles"), "{ddl}");
}

#[tokio::test]
async fn read_only_session_blocks_writes() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_readonly").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (a INT)").await;
    s.set_read_only(true).await.unwrap();
    // The guard runs before anything is sent, so the refusal may surface either
    // from `execute` or from draining the stream; both are `PolicyDenied`.
    let err = match s
        .execute("INSERT INTO t VALUES (1)", &[], ExecOptions::default())
        .await
    {
        Err(err) => err,
        Ok(stream) => stream.collect().await.unwrap_err(),
    };
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    // Reads still work.
    let sets = run(&*s, "SELECT COUNT(*) AS n FROM t").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(0)]]);
    s.set_read_only(false).await.unwrap();
    let sets = run(&*s, "INSERT INTO t VALUES (1)").await;
    assert_eq!(sets[0].affected_rows, Some(1));
}

#[tokio::test]
async fn cancel_kills_a_sleeping_query() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_cancel").await else {
        return;
    };
    let started = Instant::now();
    let handle = s.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel();
    });
    let result = s
        .execute("WAITFOR DELAY '00:00:20'", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await;
    // SQL Server can only be interrupted with `KILL`, which ends the session, so
    // the stream may report either a cancellation or a dead connection.
    let err = result.expect_err("a cancelled statement must fail");
    assert!(
        matches!(err, Error::Cancelled | Error::Connection(_)),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancel took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn statement_timeout_ends_a_sleeping_query() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_timeout").await else {
        return;
    };
    let started = Instant::now();
    let sets = s
        .execute(
            "WAITFOR DELAY '00:00:20'",
            &[],
            ExecOptions {
                statement_timeout_secs: Some(1),
                ..ExecOptions::default()
            },
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "timeout took {:?}",
        started.elapsed()
    );
    assert!(sets.is_err(), "a timed-out statement must fail");
}

#[tokio::test]
async fn transactions_roll_back_through_the_driver() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_tx").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (a INT)").await;
    run(&*s, "BEGIN TRANSACTION").await;
    run(&*s, "INSERT INTO t VALUES (1),(2)").await;
    run(&*s, "ROLLBACK").await;
    let sets = run(&*s, "SELECT COUNT(*) AS n FROM t").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(0)]]);

    run(&*s, "BEGIN TRANSACTION").await;
    run(&*s, "INSERT INTO t VALUES (3)").await;
    run(&*s, "COMMIT").await;
    let sets = run(&*s, "SELECT COUNT(*) AS n FROM t").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(1)]]);
}

#[tokio::test]
async fn temporary_tables_survive_across_statements() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_temp").await else {
        return;
    };
    run(&*s, "SELECT 1 AS a INTO #scratch").await;
    run(&*s, "INSERT INTO #scratch (a) VALUES (2)").await;
    let sets = run(&*s, "SELECT COUNT(*) AS n FROM #scratch").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(2)]]);
}

#[tokio::test]
async fn last_insert_id_and_affected_rows() {
    let t = target();
    let Some(s) = seeded(&t, "fg_mssql_identity").await else {
        return;
    };
    let sets = run(
        &*s,
        "INSERT INTO books (title, author_id) VALUES ('dispossessed', 2)",
    )
    .await;
    assert_eq!(sets[0].affected_rows, Some(1));
    // The identity readback is a separate statement by contract.
    let dialect = fg_core::dialect_for(DbKind::MsSql).unwrap();
    let sql = dialect.last_insert_id_sql().expect("identity readback");
    let sets = run(&*s, sql).await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(3)]]);
}

#[tokio::test]
async fn type_matrix() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_types").await else {
        return;
    };
    run(
        &*s,
        "CREATE TABLE t (
             flag BIT,
             tiny TINYINT,
             small SMALLINT,
             big BIGINT,
             ratio FLOAT,
             amount DECIMAL(18,4),
             label NVARCHAR(50),
             blob VARBINARY(16),
             day DATE,
             clock TIME(7),
             stamp DATETIME2(7),
             zoned DATETIMEOFFSET(7),
             ident UNIQUEIDENTIFIER
         )",
    )
    .await;
    run(
        &*s,
        "INSERT INTO t VALUES (
             1, 200, 300, 400, 1.5, 1234.5678, 'hello', 0x0102,
             '2024-03-02', '00:07:30', '2024-03-02T00:00:01.2345670',
             '2024-03-02T00:00:01.2345670+08:00',
             '6f9619ff-8b86-d011-b42d-00c04fc964ff'
         )",
    )
    .await;
    let sets = run(&*s, "SELECT * FROM t").await;
    let row = &sets[0].rows[0];
    assert_eq!(row[0], Value::Bool(true));
    assert_eq!(row[1], Value::Int(200));
    assert_eq!(row[2], Value::Int(300));
    assert_eq!(row[3], Value::Int(400));
    assert_eq!(row[4], Value::Float(1.5));
    assert_eq!(row[5], Value::Decimal("1234.5678".into()));
    assert_eq!(row[6], Value::Text("hello".into()));
    assert_eq!(row[7], Value::Bytes(vec![1, 2]));
    assert_eq!(row[8], Value::Date("2024-03-02".into()));
    assert_eq!(row[9], Value::Time("00:07:30".into()));
    assert_eq!(
        row[10],
        Value::Timestamp("2024-03-02T00:00:01.234567".into())
    );
    assert_eq!(
        row[11],
        Value::TimestampTz("2024-03-02T00:00:01.234567+08:00".into())
    );
    assert_eq!(
        row[12],
        Value::Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".into())
    );
}

#[tokio::test]
async fn empty_result_sets_still_report_their_columns() {
    let t = target();
    let Some(s) = seeded(&t, "fg_mssql_empty").await else {
        return;
    };
    let sets = run(&*s, "SELECT id, title FROM books WHERE 1 = 0").await;
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].columns.len(), 2);
    assert!(sets[0].rows.is_empty());
    assert_eq!(sets[0].affected_rows, None);
}

#[tokio::test]
async fn go_separates_batches_without_being_sent() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_go").await else {
        return;
    };
    let sets = run(&*s, "SELECT 1 AS a\nGO\nSELECT 2 AS b\nGO\n").await;
    assert_eq!(sets.len(), 2, "{sets:?}");
    assert_eq!(sets[0].rows, vec![vec![Value::Int(1)]]);
    assert_eq!(sets[1].rows, vec![vec![Value::Int(2)]]);
}

#[tokio::test]
async fn list_databases_includes_the_current_one() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_mssql_databases").await else {
        return;
    };
    let databases = s.list_databases().await.unwrap();
    assert!(
        databases.iter().any(|d| d == "fg_mssql_databases"),
        "{databases:?}"
    );
}
