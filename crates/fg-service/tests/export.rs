//! `OpenConnection::export_table` / `export_sql` against a real SQLite
//! file: CSV escaping, JSON typing, SQL batching and literals, structure
//! scope, write refusal, and a gzip file roundtrip.
//!
//! SQLite runs on a local file and never skips. Run with `-- --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, ObjectPath, SafeMode, TableQuery};
use fg_service::{
    ExportControl, ExportFormat, ExportOptions, ExportScope, KeychainKeyProvider, MemoryBackend,
    OpenConnection, PasswordUpdate, Service, ServiceOptions,
};

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-export-{}-{}-{name}",
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
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, n REAL, b BLOB);
             INSERT INTO t VALUES (1,'plain',1.5,NULL);
             INSERT INTO t VALUES (2,'with,comma',NULL,X'00ff');
             INSERT INTO t VALUES (3,'with\"quote',-2.25,NULL);
             INSERT INTO t VALUES (4,'with
newline',NULL,NULL);
             INSERT INTO t VALUES (5,NULL,NULL,NULL);",
        )
        .unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        Self { dir, service }
    }

    async fn open(&self) -> Arc<OpenConnection> {
        let c = ConnectionConfig::new(
            "c",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self.dir.join("d.db").to_string_lossy().into_owned(),
            },
        );
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

fn query() -> TableQuery {
    TableQuery::new(ObjectPath::new(None, "t"), 100)
}

async fn export_csv(conn: &Arc<OpenConnection>) -> String {
    let control = ExportControl::default();
    let options = ExportOptions {
        format: ExportFormat::Csv,
        ..ExportOptions::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    conn.export_table(&query(), &options, &control, &mut buf)
        .await
        .unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn csv_escapes_commas_quotes_newlines_and_nulls() {
    let env = Env::new("csv");
    let conn = env.open().await;
    let csv = export_csv(&conn).await;
    assert!(
        csv.starts_with("id,name,n,b\n"),
        "header first, got: {csv:?}"
    );
    assert!(csv.contains("1,plain,1.5,\n"), "plain row, got: {csv:?}");
    assert!(
        csv.contains("2,\"with,comma\",,00ff\n"),
        "comma quoted, got: {csv:?}"
    );
    assert!(
        csv.contains("3,\"with\"\"quote\",-2.25,\n"),
        "quote doubled, got: {csv:?}"
    );
    assert!(
        csv.contains("4,\"with\nnewline\",,\n"),
        "newline quoted, got: {csv:?}"
    );
    assert!(csv.contains("5,,,\n"), "NULLs are empty, got: {csv:?}");
    env.service.close_all();
}

#[tokio::test]
async fn json_keeps_numbers_and_nulls() {
    let env = Env::new("json");
    let conn = env.open().await;
    let control = ExportControl::default();
    let options = ExportOptions {
        format: ExportFormat::Json,
        ..ExportOptions::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    conn.export_sql(
        "SELECT id, name FROM t ORDER BY id",
        &options,
        &control,
        &mut buf,
    )
    .await
    .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&buf).unwrap();
    assert_eq!(parsed[0]["id"], 1);
    assert_eq!(parsed[0]["name"], "plain");
    assert_eq!(parsed[4]["name"], serde_json::Value::Null);
    assert_eq!(parsed.as_array().unwrap().len(), 5);
    env.service.close_all();
}

#[tokio::test]
async fn sql_batches_and_quotes_literals() {
    let env = Env::new("sql");
    let conn = env.open().await;
    let control = ExportControl::default();
    let options = ExportOptions {
        format: ExportFormat::Sql,
        sql_batch: 2,
        ..ExportOptions::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    let report = conn
        .export_table(&query(), &options, &control, &mut buf)
        .await
        .unwrap();
    assert_eq!(report.rows, 5);
    let sql = String::from_utf8(buf).unwrap();
    // 5 rows in batches of 2 -> 3 INSERT statements.
    assert_eq!(sql.matches("INSERT INTO").count(), 3, "got: {sql:?}");
    assert!(
        sql.contains(
            "INSERT INTO \"t\" (\"id\", \"name\", \"n\", \"b\") VALUES (1, 'plain', 1.5, NULL)"
        ),
        "first batch literal, got: {sql:?}"
    );
    assert!(
        sql.contains("'with\"quote'"),
        "double quote needs no SQL escape, got: {sql:?}"
    );
    assert!(sql.contains("X'00FF'"), "blob literal, got: {sql:?}");
    env.service.close_all();
}

#[tokio::test]
async fn structure_scope_writes_ddl_first() {
    let env = Env::new("ddl");
    let conn = env.open().await;
    let control = ExportControl::default();
    let options = ExportOptions {
        format: ExportFormat::Sql,
        scope: ExportScope::Both,
        ..ExportOptions::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    let report = conn
        .export_table(&query(), &options, &control, &mut buf)
        .await
        .unwrap();
    let sql = String::from_utf8(buf).unwrap();
    assert!(sql.starts_with("CREATE TABLE"), "got: {sql:?}");
    assert!(sql.contains("INSERT INTO"), "got: {sql:?}");
    assert_eq!(report.rows, 5);
    // Structure alone writes no rows.
    let options = ExportOptions {
        format: ExportFormat::Sql,
        scope: ExportScope::Structure,
        ..ExportOptions::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    let report = conn
        .export_table(&query(), &options, &control, &mut buf)
        .await
        .unwrap();
    assert_eq!(report.rows, 0);
    let sql = String::from_utf8(buf).unwrap();
    assert!(sql.starts_with("CREATE TABLE"), "got: {sql:?}");
    assert!(!sql.contains("INSERT INTO"), "got: {sql:?}");
    env.service.close_all();
}

#[tokio::test]
async fn export_sql_refuses_writes_multi_statements_and_sql_format() {
    let env = Env::new("refuse");
    let conn = env.open().await;
    let control = ExportControl::default();
    let csv = ExportOptions {
        format: ExportFormat::Csv,
        ..ExportOptions::default()
    };
    let mut buf: Vec<u8> = Vec::new();
    assert!(
        conn.export_sql("DELETE FROM t", &csv, &control, &mut buf)
            .await
            .is_err(),
        "writes are refused"
    );
    assert!(
        conn.export_sql("SELECT 1; SELECT 2", &csv, &control, &mut buf)
            .await
            .is_err(),
        "multi-statement is refused"
    );
    let sql = ExportOptions {
        format: ExportFormat::Sql,
        ..ExportOptions::default()
    };
    assert!(
        conn.export_sql("SELECT * FROM t", &sql, &control, &mut buf)
            .await
            .is_err(),
        "SQL output needs a table"
    );
    env.service.close_all();
}

#[tokio::test]
async fn gzip_file_roundtrips() {
    let env = Env::new("gzip");
    let conn = env.open().await;
    let control = ExportControl::default();
    let options = ExportOptions {
        format: ExportFormat::Csv,
        gzip: true,
        ..ExportOptions::default()
    };
    let path = env.dir.join("out.csv.gz");
    let report = conn
        .export_table_to_file(&query(), &options, &control, &path)
        .await
        .unwrap();
    assert_eq!(report.rows, 5);
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[..2], b"\x1f\x8b", "gzip magic");
    let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
    let mut text = String::new();
    decoder.read_to_string(&mut text).unwrap();
    assert!(text.starts_with("id,name,n,b\n"), "got: {text:?}");
    assert!(text.contains("2,\"with,comma\""), "got: {text:?}");
    env.service.close_all();
}

#[tokio::test]
async fn read_only_connection_can_still_export() {
    let env = Env::new("readonly");
    let mut c = ConnectionConfig::new(
        "c",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: env.dir.join("d.db").to_string_lossy().into_owned(),
        },
    );
    c.safe_mode = Some(SafeMode::ReadOnly);
    let c = env
        .service
        .save_connection(c, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = env.service.open(&c.id).await.unwrap();
    // A read-only session still exports: export is a read.
    let csv = export_csv(&conn).await;
    assert!(csv.starts_with("id,name,n,b\n"), "got: {csv:?}");
    env.service.close_all();
}
