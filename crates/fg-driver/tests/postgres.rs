//! Behavior tests against a real PostgreSQL, mirroring `sqlite.rs`.
//!
//! The server is expected at `FG_TEST_POSTGRES_URL` (a `postgres://` URL), or
//! at the default local scratch server `postgres://postgres@127.0.0.1:54399`
//! when the variable is unset. When the server is unreachable and no URL was
//! explicitly configured, each test prints a skip notice and passes vacuously,
//! so machines without a server still pass the gate; an explicitly configured
//! but unreachable server fails loudly instead.
//!
//! Every test uses its own database so the tests can run in parallel.
//!
//! Skips are a trap: a dead server makes every test pass vacuously. Always
//! run this file with `-- --nocapture` and confirm no `skipping` line appears;
//! a run with skips proves nothing about the driver.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use fg_core::{
    ConnectionConfig, ConnectionParams, DbKind, Error, ObjectKind, ObjectPath, ReferentialAction,
    TlsConfig, TlsMode, TypeCategory, Value,
};
use fg_driver::{Event, ExecOptions, Session, driver_for};

struct Target {
    host: String,
    port: u16,
    user: String,
    explicit: bool,
}

fn target() -> Target {
    let url = std::env::var("FG_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let explicit = url.is_some();
    let url = url.unwrap_or_else(|| "postgres://postgres@127.0.0.1:54399/postgres".to_owned());
    let rest = url.strip_prefix("postgres://").unwrap_or(&url);
    let rest = rest.strip_prefix("postgresql://").unwrap_or(rest);
    let (userinfo, hostpath) = rest.split_once('@').unwrap_or(("", rest));
    let user = userinfo.split(':').next().unwrap_or("postgres").to_owned();
    let (hostport, _) = hostpath.split_once('/').unwrap_or((hostpath, ""));
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(5432)),
        None => (hostport.to_owned(), 5432),
    };
    Target {
        host,
        port,
        user,
        explicit,
    }
}

fn config_for(t: &Target, database: &str) -> ConnectionConfig {
    ConnectionConfig::new(
        "pg-test",
        DbKind::Postgres,
        ConnectionParams::Network {
            host: t.host.clone(),
            port: t.port,
            user: t.user.clone(),
            database: Some(database.to_owned()),
            socket: None,
        },
    )
}

/// Connect to `database`, or return `None` when the server is absent and the
/// caller should skip. Panics when the server was explicitly configured.
async fn try_connect(t: &Target, database: &str) -> Option<Box<dyn Session>> {
    match driver_for(DbKind::Postgres)
        .unwrap()
        .connect(&config_for(t, database), None)
        .await
    {
        Ok(s) => Some(s),
        Err(e) => {
            if t.explicit {
                panic!("configured PostgreSQL server unreachable: {e:?}");
            }
            eprintln!("skipping postgres behavior tests: server unreachable ({e:?})");
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

/// A fresh database for one test, or `None` when the server is absent.
async fn fresh_db(t: &Target, name: &str) -> Option<Box<dyn Session>> {
    let admin = try_connect(t, "postgres").await?;
    run(&*admin, &format!("DROP DATABASE IF EXISTS {name}")).await;
    run(&*admin, &format!("CREATE DATABASE {name}")).await;
    drop(admin);
    try_connect(t, name).await
}

async fn seeded(t: &Target, name: &str) -> Option<Box<dyn Session>> {
    let s = fresh_db(t, name).await?;
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
    Some(s)
}

#[tokio::test]
async fn ping_and_version() {
    let t = target();
    let Some(s) = try_connect(&t, "postgres").await else {
        return;
    };
    s.ping().await.unwrap();
    // server_version() is the bare number from current_setting, e.g. "16.2".
    let version = s.server_version().await.unwrap();
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "unexpected version string: {version}"
    );
}

#[tokio::test]
async fn multi_statement_yields_one_result_set_per_statement() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_multi").await else {
        return;
    };
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
    let t = target();
    let Some(s) = fresh_db(&t, "fg_params").await else {
        return;
    };
    run(&*s, "CREATE TABLE t (a INTEGER, b TEXT, c BYTEA)").await;
    let params = [
        Value::Int(7),
        Value::Text("it's".into()),
        Value::Bytes(vec![1, 2, 3]),
    ];
    s.execute(
        "INSERT INTO t VALUES ($1, $2, $3)",
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
    let t = target();
    let Some(s) = fresh_db(&t, "fg_batches").await else {
        return;
    };
    let mut stream = s
        .execute(
            "SELECT i FROM generate_series(1, 10) AS i",
            &[],
            ExecOptions {
                batch_size: 4,
                ..ExecOptions::default()
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
async fn max_rows_truncates_and_flags_it_and_the_session_stays_usable() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_maxrows").await else {
        return;
    };
    let q = "SELECT i FROM generate_series(1, 10) AS i";
    let opts = |max| ExecOptions {
        batch_size: 3,
        max_rows: Some(max),
        ..ExecOptions::default()
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

    // Dropping the row stream mid-result must not poison the connection: the
    // next statement on the same session still runs.
    let sets = run(&*s, "SELECT 42").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(42)]]);
}

#[tokio::test]
async fn sql_errors_are_structured() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_sqlerr").await else {
        return;
    };
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
            assert_eq!(q.code.as_deref(), Some("42601"));
            assert_eq!(q.position, Some(1));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn metadata_roundtrip() {
    let t = target();
    let Some(s) = seeded(&t, "fg_meta").await else {
        return;
    };
    let namespaces = s.list_namespaces().await.unwrap();
    assert!(namespaces.iter().any(|n| n == "public"));

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
    assert!(
        title
            .default
            .as_deref()
            .is_some_and(|d| d.contains("untitled")),
        "default: {:?}",
        title.default
    );
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
    assert_eq!(fk.on_delete, Some(ReferentialAction::Cascade));

    let ddl = s
        .ddl(&ObjectPath::new(None, "authors"))
        .await
        .unwrap()
        .unwrap();
    assert!(ddl.contains("CREATE TABLE"), "{ddl}");
    assert!(ddl.contains("authors"), "{ddl}");

    assert!(s.describe(&ObjectPath::new(None, "nope")).await.is_err());
}

#[tokio::test]
async fn relations_are_listed_in_one_call() {
    let t = target();
    let Some(s) = seeded(&t, "fg_relations").await else {
        return;
    };
    let rels = s.list_relations(None).await.unwrap();
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].from.name, "books");
    assert_eq!(rels[0].key.referenced.name, "authors");
}

#[tokio::test]
async fn result_columns_know_their_origin() {
    let t = target();
    let Some(s) = seeded(&t, "fg_origin").await else {
        return;
    };
    let sets = run(
        &*s,
        "SELECT b.title AS t, a.name, upper(b.title) AS shout FROM books b JOIN authors a ON a.id = b.author_id",
    )
    .await;
    let cols = &sets[0].columns;
    let o = |i: usize| {
        cols[i]
            .origin
            .as_ref()
            .map(|o| (o.table.as_str(), o.column.as_str()))
    };
    assert_eq!(o(0), Some(("books", "title")), "aliases keep their origin");
    assert_eq!(o(1), Some(("authors", "name")));
    assert_eq!(o(2), None, "expressions have no origin");
}

#[tokio::test]
async fn oversized_cells_arrive_truncated() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_trunc").await else {
        return;
    };
    let sets = s
        .execute(
            "SELECT 'abcdefghij' AS big, 'ab' AS small",
            &[],
            ExecOptions {
                max_cell_bytes: Some(4),
                ..ExecOptions::default()
            },
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    match &sets[0].rows[0][0] {
        Value::Truncated(tr) => {
            assert_eq!(tr.total_len, 10);
            assert_eq!(tr.preview, b"abcd");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(sets[0].rows[0][1], Value::Text("ab".into()));
}

#[tokio::test]
async fn statements_report_index_and_timing() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_timing").await else {
        return;
    };
    let mut stream = s
        .execute("SELECT 1; SELECT 2", &[], ExecOptions::default())
        .await
        .unwrap();
    let mut starts = Vec::new();
    while let Some(ev) = stream.next().await {
        if let Event::ResultStart { index, .. } = ev.unwrap() {
            starts.push(index);
        }
    }
    assert_eq!(starts, vec![0, 1]);
}

#[tokio::test]
async fn read_only_session_blocks_writes() {
    let t = target();
    let Some(s) = seeded(&t, "fg_readonly").await else {
        return;
    };
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
    let t = target();
    let Some(s) = fresh_db(&t, "fg_cancel").await else {
        return;
    };
    for delay_ms in [0u64, 1, 5, 20, 60, 100] {
        let stream = s
            .execute("SELECT pg_sleep(60)", &[], ExecOptions::default())
            .await
            .unwrap();
        let cancel = s.cancel_handle();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            cancel.cancel();
        });
        // The acceptance bar is a pg_sleep(60) cancelled within 1 s: measure
        // from execute to the cancelled result, which includes the delay.
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .unwrap_or_else(|_| panic!("delay {delay_ms}: query was not cancelled in time"));
        assert!(
            matches!(result, Err(Error::Cancelled)),
            "delay {delay_ms}: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "delay {delay_ms}: cancel took {:?}",
            started.elapsed()
        );
    }

    // The session stays usable afterwards.
    s.ping().await.unwrap();
}

#[tokio::test]
async fn cancel_before_the_statement_starts_is_not_lost() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_cancel_early").await else {
        return;
    };
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
async fn analysed_tables_report_a_row_estimate() {
    let t = target();
    let Some(s) = seeded(&t, "fg_estimate").await else {
        return;
    };
    let estimate = |objects: &[fg_core::ObjectInfo], name: &str| {
        objects
            .iter()
            .find(|o| o.path.name == name)
            .unwrap()
            .row_estimate
    };
    let before = s.list_objects(None).await.unwrap();
    assert_eq!(
        estimate(&before, "authors"),
        None,
        "never analysed: no cheap count"
    );

    run(&*s, "ANALYZE authors").await;
    let after = s.list_objects(None).await.unwrap();
    assert_eq!(estimate(&after, "authors"), Some(2));
}

#[tokio::test]
async fn type_matrix_reads() {
    let t = target();
    let Some(s) = fresh_db(&t, "fg_types").await else {
        return;
    };
    run(
        &*s,
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
         CREATE TABLE m (
             small SMALLINT, i INTEGER, big BIGINT,
             r REAL, d DOUBLE PRECISION, n NUMERIC(10,3),
             txt TEXT, vc VARCHAR(20), ch CHAR(3), bin BYTEA, b BOOLEAN,
             dt DATE, tm TIME, ts TIMESTAMP, tstz TIMESTAMPTZ, iv INTERVAL,
             u UUID, j JSON, jb JSONB,
             arr INTEGER[], en mood, rg INT4RANGE, inet INET, bits BIT(4), pt POINT
         );
         INSERT INTO m VALUES (
             12, 34, 56,
             1.5, 2.5, 123.456,
             'hello', 'world', 'ab ', '\\x0102', true,
             DATE '2026-01-02', TIME '03:04:05', TIMESTAMP '2026-01-02 03:04:05',
             TIMESTAMPTZ '2026-01-02 03:04:05+00', INTERVAL '1 day 02:03:04',
             '123e4567-e89b-12d3-a456-426614174000', '{\"a\":1}', '{\"b\":2}',
             ARRAY[1, 2, 3], 'happy', '[1,5)', '127.0.0.1', B'1010', point(1, 2)
         );",
    )
    .await;
    let sets = run(&*s, "SELECT * FROM m").await;
    let row = &sets[0].rows[0];
    let expect = [
        Value::Int(12),
        Value::Int(34),
        Value::Int(56),
        Value::Float(1.5),
        Value::Float(2.5),
        Value::Decimal("123.456".into()),
        Value::Text("hello".into()),
        Value::Text("world".into()),
        Value::Text("ab ".into()),
        Value::Bytes(vec![1, 2]),
        Value::Bool(true),
        Value::Date("2026-01-02".into()),
        Value::Time("03:04:05".into()),
        Value::Timestamp("2026-01-02T03:04:05".into()),
        Value::TimestampTz("2026-01-02T03:04:05Z".into()),
        Value::Interval("1 day 02:03:04".into()),
        Value::Uuid("123e4567-e89b-12d3-a456-426614174000".into()),
        Value::Json("{\"a\":1}".into()),
        // jsonb is re-serialized by the server, with a space after the colon.
        Value::Json("{\"b\": 2}".into()),
        Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        Value::Text("happy".into()),
        Value::Other {
            type_name: "int4range".into(),
            text: "[1,5)".into(),
        },
        Value::Text("127.0.0.1".into()),
        Value::Text("1010".into()),
        Value::Other {
            type_name: "point".into(),
            text: "(1,2)".into(),
        },
    ];
    assert_eq!(row.len(), expect.len(), "{row:?}");
    for (i, (got, want)) in row.iter().zip(expect.iter()).enumerate() {
        assert_eq!(got, want, "column {i}: {got:?} != {want:?}");
    }
}

#[tokio::test]
async fn unix_socket_connect() {
    // Only meaningful against the default local scratch server, whose socket
    // directory is FG_TEST_PG_SOCKET_DIR (default /home/pgtest). The socket
    // file must exist: without it there is no local socket server to test
    // against, and a connect error would be a setup gap, not a driver result.
    let t = target();
    if t.explicit {
        eprintln!("skipping unix-socket test: explicit server configured");
        return;
    }
    let dir = std::env::var("FG_TEST_PG_SOCKET_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/home/pgtest".to_owned());
    let socket_file = format!("{dir}/.s.PGSQL.{}", t.port);
    if !std::path::Path::new(&socket_file).exists() {
        eprintln!("skipping unix-socket test: no socket at {socket_file}");
        return;
    }
    let mut cfg = ConnectionConfig::new(
        "pg-socket-test",
        DbKind::Postgres,
        ConnectionParams::Network {
            host: dir.clone(),
            port: t.port,
            user: "postgres".to_owned(),
            database: Some("postgres".to_owned()),
            socket: Some(dir),
        },
    );
    // Unix sockets cannot do TLS, so the driver requires an explicit Disable;
    // without it the connect is refused before any socket is opened.
    cfg.tls = Some(fg_core::TlsConfig {
        mode: fg_core::TlsMode::Disable,
        ..Default::default()
    });
    let s = driver_for(DbKind::Postgres)
        .unwrap()
        .connect(&cfg, None)
        .await
        .unwrap();
    s.ping().await.unwrap();
    let sets = run(&*s, "SELECT 1").await;
    assert_eq!(sets[0].rows, vec![vec![Value::Int(1)]]);
}

// --- TLS ------------------------------------------------------------------
//
// Against the TLS test server (system PostgreSQL 16 with ssl=on, port 54400
// by default, certificate for DNS:localhost / IP:127.0.0.1 signed by the CA
// at FG_TEST_PG_TLS_CA, default /home/pgtest/certs/ca.crt). All TLS tests skip
// when the CA file is absent, and when an explicit server is configured (its
// certificate would not match this CA).

fn tls_port() -> u16 {
    std::env::var("FG_TEST_PG_TLS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(54400)
}

fn tls_ca(name: &str, default: &str) -> Option<String> {
    let path = std::env::var(name)
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| default.to_owned());
    std::path::Path::new(&path).exists().then_some(path)
}

async fn tls_connect(
    mode: TlsMode,
    ca_path: Option<String>,
    server_name: Option<String>,
) -> Result<Box<dyn Session>, Error> {
    let mut cfg = ConnectionConfig::new(
        "pg-tls-test",
        DbKind::Postgres,
        ConnectionParams::Network {
            host: "127.0.0.1".to_owned(),
            port: tls_port(),
            user: "postgres".to_owned(),
            database: Some("postgres".to_owned()),
            socket: None,
        },
    );
    cfg.tls = Some(TlsConfig {
        mode,
        ca_cert_path: ca_path,
        client_cert_path: None,
        client_key_path: None,
        server_name,
    });
    driver_for(DbKind::Postgres)
        .unwrap()
        .connect(&cfg, None)
        .await
}

#[tokio::test]
async fn tls_verify_full_connects_and_browses() {
    let t = target();
    if t.explicit {
        eprintln!("skipping TLS tests: explicit server configured");
        return;
    }
    let Some(ca) = tls_ca("FG_TEST_PG_TLS_CA", "/home/pgtest/certs/ca.crt") else {
        eprintln!("skipping TLS tests: no CA file");
        return;
    };
    let s = tls_connect(TlsMode::VerifyFull, Some(ca), None)
        .await
        .unwrap();
    s.ping().await.unwrap();
    let namespaces = s.list_namespaces().await.unwrap();
    assert!(namespaces.iter().any(|n| n == "public"));
}

#[tokio::test]
async fn tls_verify_ca_connects() {
    let t = target();
    if t.explicit {
        eprintln!("skipping TLS tests: explicit server configured");
        return;
    }
    let Some(ca) = tls_ca("FG_TEST_PG_TLS_CA", "/home/pgtest/certs/ca.crt") else {
        eprintln!("skipping TLS tests: no CA file");
        return;
    };
    let s = tls_connect(TlsMode::VerifyCa, Some(ca), None)
        .await
        .unwrap();
    s.ping().await.unwrap();
}

#[tokio::test]
async fn tls_verify_ca_skips_the_name_check() {
    // The server certificate is for localhost/127.0.0.1; verify-ca must not
    // care that the checked name is unrelated.
    let t = target();
    if t.explicit {
        eprintln!("skipping TLS tests: explicit server configured");
        return;
    }
    let Some(ca) = tls_ca("FG_TEST_PG_TLS_CA", "/home/pgtest/certs/ca.crt") else {
        eprintln!("skipping TLS tests: no CA file");
        return;
    };
    let s = tls_connect(
        TlsMode::VerifyCa,
        Some(ca),
        Some("elsewhere.invalid".to_owned()),
    )
    .await
    .unwrap();
    s.ping().await.unwrap();
}

#[tokio::test]
async fn tls_require_connects_without_verification() {
    let t = target();
    if t.explicit {
        eprintln!("skipping TLS tests: explicit server configured");
        return;
    }
    let s = tls_connect(TlsMode::Require, None, None).await.unwrap();
    s.ping().await.unwrap();
}

#[tokio::test]
async fn tls_verify_full_rejects_a_wrong_ca() {
    let t = target();
    if t.explicit {
        eprintln!("skipping TLS tests: explicit server configured");
        return;
    }
    let Some(wrong_ca) = tls_ca("FG_TEST_PG_TLS_WRONG_CA", "/home/pgtest/certs/ca2.crt") else {
        eprintln!("skipping TLS tests: no wrong-CA file");
        return;
    };
    let err = match tls_connect(TlsMode::VerifyFull, Some(wrong_ca), None).await {
        Ok(_) => panic!("connect with a wrong CA must fail"),
        Err(e) => e,
    };
    let text = format!("{err:?}");
    assert!(
        text.to_lowercase().contains("certificate"),
        "expected a certificate error, got: {text}"
    );
}

#[tokio::test]
async fn tls_verify_full_rejects_a_wrong_name() {
    let t = target();
    if t.explicit {
        eprintln!("skipping TLS tests: explicit server configured");
        return;
    }
    let Some(ca) = tls_ca("FG_TEST_PG_TLS_CA", "/home/pgtest/certs/ca.crt") else {
        eprintln!("skipping TLS tests: no CA file");
        return;
    };
    let err = match tls_connect(
        TlsMode::VerifyFull,
        Some(ca),
        Some("elsewhere.invalid".to_owned()),
    )
    .await
    {
        Ok(_) => panic!("connect with a wrong name must fail"),
        Err(e) => e,
    };
    let text = format!("{err:?}");
    assert!(
        text.to_lowercase().contains("certificate"),
        "expected a certificate error, got: {text}"
    );
}
