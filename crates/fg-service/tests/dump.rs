//! Whole-database SQL dump export and import against real SQLite
//! files: header and engine checks, foreign-key ordering, cycles,
//! gzip files, and a full export-then-import round trip.
//!
//! SQLite runs on local files and never skips. Run with `-- --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, SafeMode};
use fg_service::{
    ExportControl, ExportFormat, ExportOptions, ExportScope, KeychainKeyProvider, MemoryBackend,
    OpenConnection, PasswordUpdate, Service, ServiceOptions, SqlImportOptions,
};

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-dump-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        Self { dir, service }
    }

    fn seed(&self, db: &str, sql: &str) {
        let c = rusqlite::Connection::open(self.dir.join(db)).unwrap();
        c.execute_batch(sql).unwrap();
    }

    async fn open(&self, db: &str, safe_mode: Option<SafeMode>) -> Arc<OpenConnection> {
        let mut c = ConnectionConfig::new(
            format!("c-{db}"),
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self.dir.join(db).to_string_lossy().into_owned(),
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

    fn rows(&self, db: &str, sql: &str) -> Vec<String> {
        let c = rusqlite::Connection::open(self.dir.join(db)).unwrap();
        let mut stmt = c.prepare(sql).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
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

fn sql_options(gzip: bool) -> ExportOptions {
    ExportOptions {
        format: ExportFormat::Sql,
        scope: ExportScope::Both,
        gzip,
        sql_batch: 100,
    }
}

const PARENT_CHILD: &str = "
CREATE TABLE z_parent(id INTEGER PRIMARY KEY, name TEXT);
INSERT INTO z_parent VALUES (1, 'p1');
INSERT INTO z_parent VALUES (2, 'p2');
CREATE TABLE a_child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES z_parent(id), v TEXT);
INSERT INTO a_child VALUES (1, 1, 'c1');
INSERT INTO a_child VALUES (2, 2, 'c2');
CREATE VIEW v_join AS SELECT z_parent.name, a_child.v FROM z_parent JOIN a_child ON a_child.parent_id = z_parent.id;
";

#[tokio::test]
async fn roundtrip_preserves_schema_data_and_view() {
    let env = Env::new("roundtrip");
    env.seed("src.db", PARENT_CHILD);
    let conn = env.open("src.db", None).await;
    let dump = env.dir.join("dump.sql");
    let control = ExportControl::default();
    let report = conn
        .export_database_to_file(Some("main"), &sql_options(false), &control, &dump)
        .await
        .unwrap();
    assert_eq!(report.tables, 2, "two tables, got: {report:?}");
    assert_eq!(report.views, 1, "one view, got: {report:?}");
    assert_eq!(report.rows, 4, "four rows, got: {report:?}");
    assert!(report.warnings.is_empty(), "no warnings, got: {report:?}");

    let text = std::fs::read_to_string(&dump).unwrap();
    assert!(
        text.starts_with("-- fastgrid dump v1\n-- engine: SQLite\n"),
        "header first, got: {:?}",
        &text[..text.len().min(80)]
    );
    // a_child sorts before z_parent alphabetically, so this only
    // passes when the dump orders by foreign keys, parents first.
    // Table-name quoting is dialect detail; compare the row values.
    let parent_at = text.find("(1, 'p1')").unwrap();
    let child_at = text.find("(1, 1, 'c1')").unwrap();
    assert!(
        parent_at < child_at,
        "parent rows before child rows, got:\n{text}"
    );
    assert!(
        text.find("z_parent").unwrap() < text.find("a_child").unwrap(),
        "parent CREATE before child CREATE, got:\n{text}"
    );

    env.seed("dst.db", "");
    let target = env.open("dst.db", None).await;
    let preview = target.preview_sql_dump(&dump).await.unwrap();
    assert_eq!(
        preview.statements, 5,
        "2 CREATEs + 2 INSERTs + 1 view, got: {preview:?}"
    );
    assert!(
        !preview.needs_confirmation,
        "Normal needs none, got: {preview:?}"
    );
    assert!(preview.warnings.is_empty(), "own header, got: {preview:?}");
    let imported = target
        .import_sql_dump(&dump, &SqlImportOptions::default(), false)
        .await
        .unwrap();
    assert_eq!(imported.statements, 5, "all ran, got: {imported:?}");
    assert!(!imported.cancelled);
    assert_eq!(
        env.rows("dst.db", "SELECT name FROM z_parent ORDER BY id"),
        vec!["p1", "p2"]
    );
    assert_eq!(
        env.rows("dst.db", "SELECT v FROM a_child ORDER BY id"),
        vec!["c1", "c2"]
    );
    assert_eq!(
        env.rows(
            "dst.db",
            "SELECT name || ':' || v FROM v_join ORDER BY name"
        ),
        vec!["p1:c1", "p2:c2"]
    );
    env.service.close_all();
}

#[tokio::test]
async fn cycle_tables_dump_with_a_warning_and_still_import() {
    let env = Env::new("cycle");
    env.seed(
        "src.db",
        "CREATE TABLE a(id INTEGER PRIMARY KEY, b_id INTEGER REFERENCES b(id));
         CREATE TABLE b(id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id));",
    );
    let conn = env.open("src.db", None).await;
    let dump = env.dir.join("dump.sql");
    let report = conn
        .export_database_to_file(
            Some("main"),
            &sql_options(false),
            &ExportControl::default(),
            &dump,
        )
        .await
        .unwrap();
    assert_eq!(report.tables, 2);
    assert!(
        report.warnings.iter().any(|w| w.contains("cycle")),
        "cycle warned, got: {report:?}"
    );
    env.seed("dst.db", "");
    let target = env.open("dst.db", None).await;
    let imported = target
        .import_sql_dump(&dump, &SqlImportOptions::default(), false)
        .await
        .unwrap();
    assert_eq!(imported.statements, 2, "two CREATEs, got: {imported:?}");
    assert_eq!(
        env.rows(
            "dst.db",
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"
        ),
        vec!["a", "b"]
    );
    env.service.close_all();
}

#[tokio::test]
async fn header_engine_mismatch_is_refused_and_foreign_dump_warns() {
    let env = Env::new("header");
    env.seed("dst.db", "");
    let target = env.open("dst.db", None).await;

    let wrong = env.dir.join("wrong.sql");
    std::fs::write(
        &wrong,
        "-- fastgrid dump v1\n-- engine: PostgreSQL\nCREATE TABLE t(x);",
    )
    .unwrap();
    let err = target.preview_sql_dump(&wrong).await.unwrap_err();
    assert!(
        err.to_string().contains("PostgreSQL"),
        "mismatch named, got: {err}"
    );

    let foreign = env.dir.join("foreign.sql");
    std::fs::write(
        &foreign,
        "CREATE TABLE t(x INTEGER);\nINSERT INTO t VALUES (7);",
    )
    .unwrap();
    let preview = target.preview_sql_dump(&foreign).await.unwrap();
    assert_eq!(preview.statements, 2);
    assert!(
        preview.warnings.iter().any(|w| w.contains("foreign")),
        "foreign warned, got: {preview:?}"
    );
    target
        .import_sql_dump(&foreign, &SqlImportOptions::default(), false)
        .await
        .unwrap();
    assert_eq!(
        env.rows("dst.db", "SELECT CAST(x AS TEXT) FROM t"),
        vec!["7"]
    );
    env.service.close_all();
}

#[tokio::test]
async fn gzip_dump_roundtrip() {
    let env = Env::new("gzip");
    env.seed("src.db", PARENT_CHILD);
    let conn = env.open("src.db", None).await;
    let dump = env.dir.join("dump.sql.gz");
    let report = conn
        .export_database_to_file(
            Some("main"),
            &sql_options(true),
            &ExportControl::default(),
            &dump,
        )
        .await
        .unwrap();
    assert_eq!(report.rows, 4);
    assert_eq!(&std::fs::read(&dump).unwrap()[..2], &[0x1f, 0x8b]);
    env.seed("dst.db", "");
    let target = env.open("dst.db", None).await;
    let imported = target
        .import_sql_dump(&dump, &SqlImportOptions::default(), false)
        .await
        .unwrap();
    assert_eq!(imported.statements, 5);
    assert_eq!(
        env.rows("dst.db", "SELECT CAST(COUNT(*) AS TEXT) FROM a_child"),
        vec!["2"]
    );
    env.service.close_all();
}

#[tokio::test]
async fn read_only_connection_refuses_dump_import() {
    let env = Env::new("readonly");
    env.seed("src.db", "CREATE TABLE t(x INTEGER);");
    let conn = env.open("src.db", Some(SafeMode::ReadOnly)).await;
    let dump = env.dir.join("dump.sql");
    std::fs::write(&dump, "CREATE TABLE u(y INTEGER);").unwrap();
    let err = conn.preview_sql_dump(&dump).await.unwrap_err();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)), "got: {err}");
    let err = conn
        .import_sql_dump(&dump, &SqlImportOptions::default(), false)
        .await
        .unwrap_err();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)), "got: {err}");
    env.service.close_all();
}
