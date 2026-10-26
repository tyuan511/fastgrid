//! `OpenConnection::preview_schema_changes` / `apply_schema_changes` against
//! real engines: column add/alter/drop roundtrips (including a SQLite table
//! rebuild that must preserve rows and triggers), create/rename/drop table,
//! read-only refusal, and the MySQL no-rollback warning.
//!
//! SQLite runs on a local file and never skips. MySQL and PostgreSQL follow
//! the same discipline as the driver behavior tests: `FG_TEST_MYSQL_URL` /
//! `FG_TEST_POSTGRES_URL` or the default local scratch server; unreachable
//! and unconfigured means skip with a notice, explicitly configured but
//! unreachable means fail. Run with `-- --nocapture` and confirm no
//! `skipping` line appears for the engines under test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use fg_core::{
    ColumnInfo, ConnectionConfig, ConnectionParams, DataType, DbKind, IndexInfo, ObjectKind,
    ObjectPath, SafeMode, TableDetail, TypeCategory, Value,
};
use fg_driver::{ExecOptions, Session, driver_for};
use fg_service::{
    KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, Service, ServiceOptions,
};
use fg_sql::SchemaChange;

// ---------------------------------------------------------------- SQLite

struct SqliteEnv {
    dir: PathBuf,
    service: Service,
}

impl SqliteEnv {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-schema-{}-{}-{name}",
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
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL, n INTEGER);
             INSERT INTO t VALUES (1,'a',10),(2,'b',20);
             CREATE TRIGGER t_audit AFTER INSERT ON t BEGIN SELECT 1; END;",
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

impl Drop for SqliteEnv {
    fn drop(&mut self) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

fn sqlite_path() -> ObjectPath {
    ObjectPath::new(None, "t")
}

fn text_col(name: &str, ordinal: u32) -> ColumnInfo {
    let mut c = ColumnInfo::new(name, ordinal, DataType::new("TEXT", TypeCategory::Text));
    c.nullable = true;
    c
}

async fn query_one(conn: &Arc<OpenConnection>, sql: &str) -> Vec<Value> {
    let sets = conn
        .execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    sets.into_iter()
        .next()
        .unwrap()
        .rows
        .into_iter()
        .next()
        .unwrap()
}

#[tokio::test]
async fn sqlite_column_roundtrip_preserves_rows_and_triggers() {
    let env = SqliteEnv::new("roundtrip");
    let conn = env.open(None).await;
    let path = sqlite_path();

    // Add a column with a default.
    let mut age = text_col("age", 4);
    age.data_type = DataType::new("INTEGER", TypeCategory::Integer);
    age.default = Some("5".into());
    let out = conn
        .apply_schema_changes(&path, &[SchemaChange::AddColumn { column: age }])
        .await
        .unwrap();
    assert_eq!(out.applied, 1);
    assert!(out.failed.is_none());
    let detail = conn.describe(&path).await.unwrap();
    let age = detail.column("age").unwrap();
    assert_eq!(age.default.as_deref(), Some("5"));

    // Alter the type: SQLite cannot do this natively, so the service runs
    // a rebuild. Rows and the trigger must survive it.
    let preview = conn
        .preview_schema_changes(
            &path,
            &[SchemaChange::AlterColumn {
                name: "name".into(),
                new_type: Some("VARCHAR(100)".into()),
                nullable: None,
                default: None,
            }],
        )
        .await
        .unwrap();
    assert!(
        preview.warnings.iter().any(|w| w.contains("rebuild")),
        "expected a rebuild warning, got {:?}",
        preview.warnings
    );
    assert!(
        !preview.warnings.iter().any(|w| w.contains("rolled back")),
        "SQLite DDL is transactional, no rollback warning expected"
    );
    let out = conn
        .apply_schema_changes(
            &path,
            &[SchemaChange::AlterColumn {
                name: "name".into(),
                new_type: Some("VARCHAR(100)".into()),
                nullable: None,
                default: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        out.applied, 5,
        "rebuild is CREATE+INSERT+DROP+RENAME+trigger"
    );
    let names = query_one(&conn, "SELECT name FROM t ORDER BY id").await;
    assert_eq!(names, vec![Value::Text("a".into())]);
    let trigger = query_one(
        &conn,
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='t_audit'",
    )
    .await;
    assert_eq!(trigger, vec![Value::Int(1)]);

    // Drop a column.
    let out = conn
        .apply_schema_changes(&path, &[SchemaChange::DropColumn { name: "n".into() }])
        .await
        .unwrap();
    assert!(out.failed.is_none());
    let detail = conn.describe(&path).await.unwrap();
    assert!(detail.column("n").is_none());
    assert_eq!(detail.columns.len(), 3);
}

#[tokio::test]
async fn sqlite_create_rename_drop_table() {
    let env = SqliteEnv::new("crt");
    let conn = env.open(None).await;
    let mut id = ColumnInfo::new("id", 1, DataType::new("INTEGER", TypeCategory::Integer));
    id.nullable = false;
    id.primary_key_position = Some(1);
    let created = TableDetail {
        path: ObjectPath::new(None, "fresh"),
        kind: ObjectKind::Table,
        comment: None,
        columns: vec![id, text_col("label", 2)],
        indexes: vec![IndexInfo {
            name: "fresh_label".into(),
            columns: vec![fg_core::IndexColumn::column("label")],
            unique: false,
            primary: false,
            method: None,
            predicate: None,
            comment: None,
            extra: Default::default(),
        }],
        foreign_keys: vec![],
        checks: vec![],
        triggers: vec![],
        row_estimate: None,
        size_bytes: None,
        options: Default::default(),
    };
    let path = ObjectPath::new(None, "fresh");
    let out = conn
        .apply_schema_changes(&path, &[SchemaChange::CreateTable { detail: created }])
        .await
        .unwrap();
    assert!(out.failed.is_none());
    let detail = conn.describe(&path).await.unwrap();
    assert_eq!(detail.columns.len(), 2);
    assert_eq!(detail.indexes.len(), 1);

    let renamed = ObjectPath::new(None, "fresh2");
    conn.apply_schema_changes(
        &path,
        &[SchemaChange::RenameTable {
            new_name: "fresh2".into(),
        }],
    )
    .await
    .unwrap();
    assert!(conn.describe(&renamed).await.is_ok());
    assert!(conn.describe(&path).await.is_err());

    let out = conn
        .apply_schema_changes(&renamed, &[SchemaChange::DropTable])
        .await
        .unwrap();
    assert!(out.failed.is_none());
    assert!(conn.describe(&renamed).await.is_err());
}

#[tokio::test]
async fn sqlite_readonly_connection_refuses_schema_changes() {
    let env = SqliteEnv::new("readonly");
    let conn = env.open(Some(SafeMode::ReadOnly)).await;
    let err = conn
        .preview_schema_changes(&sqlite_path(), &[SchemaChange::DropTable])
        .await
        .unwrap_err();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)));
}

// ------------------------------------------------------------------ MySQL

struct NetTarget {
    host: String,
    port: u16,
    user: String,
    password: String,
    explicit: bool,
}

fn mysql_target() -> NetTarget {
    let url = std::env::var("FG_TEST_MYSQL_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let explicit = url.is_some();
    let url = url.unwrap_or_else(|| "mysql://fgtest:fgtest@127.0.0.1:53306/".to_owned());
    parse_net_url(&url, 3306, explicit)
}

fn pg_target() -> NetTarget {
    let url = std::env::var("FG_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let explicit = url.is_some();
    let url = url.unwrap_or_else(|| "postgres://postgres@127.0.0.1:54399/postgres".to_owned());
    parse_net_url(&url, 5432, explicit)
}

fn parse_net_url(url: &str, default_port: u16, explicit: bool) -> NetTarget {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let (userinfo, hostpath) = rest.split_once('@').unwrap_or(("", rest));
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u.to_owned(), p.to_owned()),
        None => (userinfo.to_owned(), String::new()),
    };
    let (hostport, _) = hostpath.split_once('/').unwrap_or((hostpath, ""));
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(default_port)),
        None => (hostport.to_owned(), default_port),
    };
    NetTarget {
        host,
        port,
        user,
        password,
        explicit,
    }
}

async fn net_session(
    kind: DbKind,
    t: &NetTarget,
    database: Option<&str>,
) -> Option<Box<dyn Session>> {
    let mut config = ConnectionConfig::new(
        "net-schema-test",
        kind,
        ConnectionParams::Network {
            host: t.host.clone(),
            port: t.port,
            user: t.user.clone(),
            database: database.map(str::to_owned),
            socket: None,
        },
    );
    config.default_namespace = database.map(str::to_owned);
    let password = if t.password.is_empty() {
        None
    } else {
        Some(t.password.as_str())
    };
    match driver_for(kind).unwrap().connect(&config, password).await {
        Ok(s) => Some(s),
        Err(e) => {
            if t.explicit {
                panic!("explicitly configured server unreachable: {e:?}");
            }
            eprintln!("skipping schema test: server unreachable ({e:?})");
            None
        }
    }
}

async fn run_seed(session: &mut Box<dyn Session>, sql: &str) {
    session
        .execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
}

async fn open_service(
    kind: DbKind,
    t: &NetTarget,
    database: &str,
) -> (Service, Arc<OpenConnection>) {
    let dir = std::env::temp_dir().join(format!(
        "fg-schema-net-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
    let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
    let mut config = ConnectionConfig::new(
        "net",
        kind,
        ConnectionParams::Network {
            host: t.host.clone(),
            port: t.port,
            user: t.user.clone(),
            database: Some(database.to_owned()),
            socket: None,
        },
    );
    config.default_namespace = Some(database.to_owned());
    let saved = service
        .save_connection_with_secrets(
            config,
            PasswordUpdate::Set(t.password.clone()),
            PasswordUpdate::Keep,
        )
        .await
        .unwrap();
    let conn = service.open(&saved.id).await.unwrap();
    (service, conn)
}

#[tokio::test]
async fn mysql_column_roundtrip_and_no_rollback_warning() {
    let t = mysql_target();
    let db = format!("fg_schema_{}", std::process::id());
    let Some(mut admin) = net_session(DbKind::MySql, &t, None).await else {
        return;
    };
    run_seed(&mut admin, &format!("CREATE DATABASE `{db}`")).await;
    let Some(mut seed) = net_session(DbKind::MySql, &t, Some(&db)).await else {
        return;
    };
    run_seed(
        &mut seed,
        "CREATE TABLE t(id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(50) NOT NULL, n INT);
         INSERT INTO t(name, n) VALUES ('a', 10), ('b', 20);",
    )
    .await;
    let (service, conn) = open_service(DbKind::MySql, &t, &db).await;
    let path = ObjectPath::new(Some(&db), "t");

    let preview = conn
        .preview_schema_changes(
            &path,
            &[SchemaChange::AddColumn {
                column: {
                    let mut c = text_col("age", 4);
                    c.data_type = DataType::new("INT", TypeCategory::Integer);
                    c
                },
            }],
        )
        .await
        .unwrap();
    assert!(
        preview.warnings.iter().any(|w| w.contains("rolled back")),
        "MySQL preview must warn DDL cannot be rolled back, got {:?}",
        preview.warnings
    );
    let out = conn
        .apply_schema_changes(
            &path,
            &[SchemaChange::AddColumn {
                column: {
                    let mut c = text_col("age", 4);
                    c.data_type = DataType::new("INT", TypeCategory::Integer);
                    c
                },
            }],
        )
        .await
        .unwrap();
    assert_eq!(out.applied, 1);
    assert!(out.failed.is_none());

    // MODIFY COLUMN roundtrip: widen the type, keep the rows.
    let out = conn
        .apply_schema_changes(
            &path,
            &[SchemaChange::AlterColumn {
                name: "name".into(),
                new_type: Some("VARCHAR(200)".into()),
                nullable: None,
                default: None,
            }],
        )
        .await
        .unwrap();
    assert!(out.failed.is_none());
    let detail = conn.describe(&path).await.unwrap();
    assert_eq!(
        detail.column("name").unwrap().data_type.native,
        "varchar(200)"
    );
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
    assert_eq!(sets[0].rows.len(), 2);

    let out = conn
        .apply_schema_changes(&path, &[SchemaChange::DropColumn { name: "n".into() }])
        .await
        .unwrap();
    assert!(out.failed.is_none());
    let detail = conn.describe(&path).await.unwrap();
    assert!(detail.column("n").is_none());

    // The service registry holds the connection, and its pool holds
    // sessions to this database; close them before the database itself
    // can go away.
    service.close_all();
    drop(conn);
    drop(service);
    drop(seed);
    run_seed(&mut admin, &format!("DROP DATABASE `{db}`")).await;
}

#[tokio::test]
async fn postgres_column_roundtrip_is_transactional() {
    let t = pg_target();
    let db = format!("fg_schema_{}", std::process::id());
    let Some(mut admin) = net_session(DbKind::Postgres, &t, Some("postgres")).await else {
        return;
    };
    run_seed(&mut admin, &format!("CREATE DATABASE \"{db}\"")).await;
    let Some(mut seed) = net_session(DbKind::Postgres, &t, Some(&db)).await else {
        return;
    };
    run_seed(
        &mut seed,
        "CREATE TABLE t(id SERIAL PRIMARY KEY, name TEXT NOT NULL, n INT);
         INSERT INTO t(name, n) VALUES ('a', 10), ('b', 20);",
    )
    .await;
    let (service, conn) = open_service(DbKind::Postgres, &t, &db).await;
    let path = ObjectPath::new(Some("public"), "t");

    let preview = conn
        .preview_schema_changes(
            &path,
            &[SchemaChange::AddColumn {
                column: {
                    let mut c = text_col("age", 4);
                    c.data_type = DataType::new("INTEGER", TypeCategory::Integer);
                    c
                },
            }],
        )
        .await
        .unwrap();
    assert!(
        preview.warnings.is_empty(),
        "PostgreSQL DDL is transactional: no warnings expected, got {:?}",
        preview.warnings
    );
    let out = conn
        .apply_schema_changes(
            &path,
            &[SchemaChange::AlterColumn {
                name: "name".into(),
                new_type: Some("VARCHAR(200)".into()),
                nullable: Some(false),
                default: Some(Some("'anon'".into())),
            }],
        )
        .await
        .unwrap();
    assert!(out.failed.is_none());
    let detail = conn.describe(&path).await.unwrap();
    assert_eq!(
        detail.column("name").unwrap().data_type.native,
        "character varying(200)"
    );
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
    assert_eq!(sets[0].rows.len(), 2);

    service.close_all();
    drop(conn);
    drop(service);
    drop(seed);
    run_seed(&mut admin, &format!("DROP DATABASE \"{db}\"")).await;
}
