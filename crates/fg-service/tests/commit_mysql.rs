//! `OpenConnection::commit_changes` against a real MySQL / MariaDB: the
//! readback path for dialects without `RETURNING` (re-select by primary key,
//! generated keys via `LAST_INSERT_ID()`).
//!
//! Same server discipline as `fg-driver/tests/mysql.rs`: `FG_TEST_MYSQL_URL`
//! or the default local scratch server; unreachable and unconfigured means
//! skip with a notice, explicitly configured but unreachable means fail.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, ObjectPath, Value};
use fg_driver::{ExecOptions, Session, driver_for};
use fg_service::{
    KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, Service, ServiceOptions,
};
use fg_sql::{CellChange, RowChange};

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
        "mysql-commit-test",
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
            eprintln!("skipping mysql commit test: server unreachable ({e:?})");
            None
        }
    }
}

async fn run(s: &dyn Session, sql: &str) {
    s.execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
}

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-commit-mysql-{}-{}-{name}",
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

    async fn open(&self, t: &Target, database: &str) -> Arc<OpenConnection> {
        let c = self
            .service
            .save_connection_with_secrets(
                config_for(t, Some(database)),
                PasswordUpdate::Set(t.password.clone()),
                PasswordUpdate::Keep,
            )
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

fn set(c: &str, v: Value) -> (String, CellChange) {
    (c.into(), CellChange::Set(v))
}

#[tokio::test]
async fn inserts_read_back_without_returning() {
    let t = target();
    let Some(admin) = try_connect(&t, None).await else {
        return;
    };
    run(&*admin, "DROP DATABASE IF EXISTS `mysql_commit`").await;
    run(&*admin, "CREATE DATABASE `mysql_commit`").await;
    drop(admin);
    let Some(admin) = try_connect(&t, Some("mysql_commit")).await else {
        return;
    };
    run(
        &*admin,
        "CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(50) NOT NULL, n INT DEFAULT 7)",
    )
    .await;
    drop(admin);

    let env = Env::new("readback");
    let conn = env.open(&t, "mysql_commit").await;
    let path = ObjectPath::new(Some("mysql_commit"), "t");
    let out = conn
        .commit_changes(
            &path,
            &[
                // Generated key, default column omitted: readback must fill
                // both in from the server.
                RowChange::Insert {
                    cells: vec![set("name", Value::Text("a".into()))],
                },
                // Explicit key: readback locates the row by that key.
                RowChange::Insert {
                    cells: vec![
                        set("id", Value::Int(42)),
                        set("name", Value::Text("b".into())),
                        set("n", Value::Int(3)),
                    ],
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(out.statements, 2);
    assert_eq!(out.affected_rows, 2);
    assert_eq!(
        out.returned_rows,
        vec![
            vec![Value::Int(1), Value::Text("a".into()), Value::Int(7),],
            vec![Value::Int(42), Value::Text("b".into()), Value::Int(3),],
        ],
        "readback rows carry generated keys and defaults"
    );
}
