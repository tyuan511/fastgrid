//! Pins for the policy, generation and recording gaps. Each test fails if the corresponding
//! guard is removed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fg_core::{
    Actor, AuditId, ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Environment, Error,
    ExecSettings, Outcome, QuerySource, SafeMode, SettingGroup, UnixMillis, Value,
};
use fg_driver::{Event, ExecOptions};
use fg_service::{
    AuditFilter, KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, RunOptions,
    Service, ServiceOptions,
};
use fg_store::StoredConnection;

const LONG: &str = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000000000) SELECT sum(i) FROM n";

struct Env {
    dir: PathBuf,
    keys: KeychainKeyProvider<MemoryBackend>,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-gap-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            keys: KeychainKeyProvider::with_backend(MemoryBackend::default()),
        }
    }

    fn service(&self) -> Service {
        let mut options = ServiceOptions::new(self.dir.join("store.db"));
        options.max_sessions_per_connection = 1;
        Service::start(options, &self.keys).unwrap()
    }

    fn sqlite(&self, name: &str) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(
            name,
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self
                    .dir
                    .join(format!("{name}.db"))
                    .to_string_lossy()
                    .into_owned(),
            },
        );
        c.options.insert("create".into(), "true".into());
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

async fn exec(conn: &Arc<OpenConnection>, sql: &str) {
    conn.execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
}

fn must_err<T>(result: Result<T, Error>) -> Error {
    match result {
        Err(err) => err,
        Ok(_) => panic!("expected an error"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => panic!("not an int: {other:?}"),
    }
}

async fn cell(conn: &Arc<OpenConnection>, sql: &str) -> i64 {
    let sets = conn
        .execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    int(&sets[0].rows[0][0])
}

async fn open_saved(svc: &Service, cfg: ConnectionConfig) -> Arc<OpenConnection> {
    let saved = svc
        .save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    svc.open(&saved.id).await.unwrap()
}

fn stored(rows: Vec<StoredConnection>, id: &ConnectionId) -> ConnectionConfig {
    rows.into_iter()
        .find_map(|row| match row {
            StoredConnection::Ok(c) if &c.id == id => Some(c),
            _ => None,
        })
        .expect("saved connection")
}

async fn wait_history(
    svc: &Service,
    id: &ConnectionId,
    pred: impl Fn(&[fg_core::QueryHistoryEntry]) -> bool,
) -> Vec<fg_core::QueryHistoryEntry> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let rows = svc.history(id, None, 20).await.unwrap();
        if pred(&rows) || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn execute_cannot_disable_read_only_with_a_pragma() {
    let env = Env::new("pragma");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    drop(conn);
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();

    let err = must_err(
        conn.execute(
            "PRAGMA query_only = OFF; DELETE FROM t",
            &[],
            ExecOptions::default(),
        )
        .await,
    );
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn startup_sql_cannot_write_on_open_or_test() {
    let env = Env::new("startup");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    drop(conn);

    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    cfg.startup_sql = vec!["DELETE FROM t".into()];
    svc.save_connection(cfg.clone(), PasswordUpdate::Keep)
        .await
        .unwrap();
    let err = must_err(svc.open(&id).await);
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    let err = svc.test_connection(cfg, None).await.unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");

    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.startup_sql.clear();
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn startup_pragma_cannot_leave_a_read_only_session_writable() {
    let env = Env::new("reassert");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    drop(conn);
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    cfg.startup_sql = vec!["PRAGMA query_only = OFF".into()];
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let err = must_err(svc.open(&id).await);
    assert!(
        matches!(err, Error::PolicyDenied(_)),
        "query_only must be refused before it can disable the guard: {err:?}"
    );
    let mut plain = stored(svc.connections().await.unwrap(), &id);
    plain.safe_mode = Some(SafeMode::Normal);
    plain.startup_sql.clear();
    svc.save_connection(plain, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn open_after_a_concurrent_save_does_not_publish_the_old_mode() {
    let env = Env::new("race");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    drop(conn);
    svc.close(&id);

    let mut pause = svc._test_pause_next_open();
    let svc2 = svc.clone();
    let id2 = id.clone();
    let opening = tokio::spawn(async move { svc2.open(&id2).await });
    pause.wait().await;

    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    pause.release();

    let conn = opening.await.unwrap().unwrap();
    assert_eq!(conn.safe_mode(), SafeMode::ReadOnly);
    let err = conn
        .run("DELETE FROM t", &[], RunOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn per_call_caps_cannot_loosen_the_stored_ones() {
    let env = Env::new("caps");
    let svc = env.service();
    svc.set_exec_settings(ExecSettings {
        max_rows: Some(2),
        statement_timeout_secs: Some(30),
        ..ExecSettings::default()
    })
    .await
    .unwrap();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE nums (n INTEGER); INSERT INTO nums VALUES (1), (2), (3), (4)",
    )
    .await;

    let sets = conn
        .run(
            "SELECT n FROM nums ORDER BY n",
            &[],
            RunOptions::gui().with_max_rows(Some(1_000_000)),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets[0].rows.len(), 2, "a larger per-call cap must not win");
    assert!(sets[0].truncated);

    let sets = conn
        .run(
            "SELECT n FROM nums ORDER BY n",
            &[],
            RunOptions::gui().with_max_rows(Some(1)),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets[0].rows.len(), 1, "a tighter per-call cap must win");
}

#[tokio::test]
async fn unreadable_exec_settings_fail_closed() {
    let env = Env::new("settings");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    svc._test_poison_store();
    let err = conn
        .run(
            "DELETE FROM t",
            &[],
            RunOptions::gui().with_max_rows(Some(1_000_000)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Storage(_)),
        "a settings read failure must not run the statement: {err:?}"
    );
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn dropping_run_while_the_session_is_busy_does_not_execute() {
    let env = Env::new("abandon");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    let holder = conn
        .execute(LONG, &[], ExecOptions::default())
        .await
        .unwrap();
    let run = conn.run("UPDATE t SET n = 9", &[], RunOptions::default());
    let timed = tokio::time::timeout(Duration::from_millis(400), run).await;
    assert!(
        timed.is_err(),
        "run returned while the only session was busy"
    );
    drop(holder);
    let rows = wait_history(&svc, &id, |rows| {
        rows.iter().any(|e| e.sql.contains("UPDATE"))
    })
    .await;
    let update = rows
        .iter()
        .find(|e| e.sql.contains("UPDATE"))
        .expect("abandoned start must be recorded");
    assert_eq!(update.outcome, Outcome::NotRun);
    let ready = tokio::time::timeout(Duration::from_secs(10), cell(&conn, "SELECT n FROM t")).await;
    assert_eq!(ready.expect("session not reusable"), 1);
}

#[tokio::test]
async fn history_survives_dropping_the_service_before_the_stream() {
    let env = Env::new("runtime");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    let stream = conn
        .run("SELECT 7 AS marker", &[], RunOptions::default())
        .await
        .unwrap();
    drop(svc);
    drop(conn);
    drop(stream);

    // The writer thread still holds the store. A second service can read WAL once the row lands.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while Instant::now() < deadline {
        let mut options = ServiceOptions::new(env.dir.join("store.db"));
        options.max_sessions_per_connection = 1;
        if let Ok(again) = Service::start(options, &env.keys) {
            if let Ok(rows) = again.history(&id, Some("marker"), 5).await
                && rows
                    .iter()
                    .any(|e| e.outcome == Outcome::Ok || e.outcome == Outcome::Cancelled)
            {
                found = true;
                drop(again);
                break;
            }
            drop(again);
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        found,
        "dropping the service first must not drop the history row"
    );
}

#[test]
fn dropping_a_stream_does_not_panic_when_the_runtime_is_shutting_down() {
    let env = Env::new("drop-rt");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut options = ServiceOptions::new(env.dir.join("store.db"));
    options.runtime = Some(runtime.handle().clone());
    options.max_sessions_per_connection = 1;
    let svc = Service::start(options, &env.keys).unwrap();
    let conn = runtime.block_on(open_saved(&svc, env.sqlite("db")));
    let stream = runtime
        .block_on(conn.run("SELECT 1", &[], RunOptions::default()))
        .unwrap();
    drop(svc);
    drop(conn);
    runtime.shutdown_background();
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(stream)));
    assert!(dropped.is_ok(), "RunStream::drop panicked during shutdown");
}

#[tokio::test]
async fn prune_failure_still_writes_the_audit_row() {
    let env = Env::new("prune");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    svc._test_inject_history_prune_failure();
    let err = conn
        .run("SELECT 1", &[], RunOptions::agent("claude"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Storage(_)),
        "the caller must see that recording failed: {err:?}"
    );
    let audit = svc
        .query_audit(AuditFilter {
            connection_id: Some(id),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert!(matches!(&audit[0].actor, Actor::Mcp { client } if client == "claude"));
}

#[tokio::test]
async fn confirmation_cannot_be_replayed_onto_different_sql() {
    let env = Env::new("replay");
    let svc = env.service();
    let mut cfg = env.sqlite("db");
    cfg.safe_mode = Some(SafeMode::Strict);
    let conn = open_saved(&svc, env.sqlite("seed")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    drop(conn);
    let mut saved = stored(svc.connections().await.unwrap(), &id);
    saved.safe_mode = Some(SafeMode::Strict);
    svc.save_connection(saved, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    let _ = cfg;
    let err = conn
        .run(
            "DELETE FROM t",
            &[],
            RunOptions::gui_confirmed("SELECT 1", DbKind::Sqlite),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn strict_requires_confirmation_for_an_ordinary_insert() {
    let env = Env::new("strict-insert");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(&conn, "CREATE TABLE t (n INTEGER)").await;
    let id = conn.id().clone();
    drop(conn);
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::Strict);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    let err = conn
        .run("INSERT INTO t VALUES (1)", &[], RunOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT count(*) FROM t").await, 0);

    let mut alert = env.sqlite("alert");
    alert.safe_mode = Some(SafeMode::Alert);
    alert.environment = Environment::Development;
    let alert = open_saved(&svc, alert).await;
    exec(&alert, "CREATE TABLE t (n INTEGER)").await;
    alert
        .run("INSERT INTO t VALUES (1)", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(cell(&alert, "SELECT count(*) FROM t").await, 1);
}

#[tokio::test]
async fn service_timer_cancels_a_long_query_when_the_engine_cannot() {
    let env = Env::new("timeout");
    let svc = env.service();
    svc.set_exec_settings(ExecSettings {
        statement_timeout_secs: Some(1),
        ..ExecSettings::default()
    })
    .await
    .unwrap();
    let mut cfg = env.sqlite("db");
    cfg.timeouts.statement_secs = Some(30);
    let conn = open_saved(&svc, cfg).await;
    let id = conn.id().clone();
    let started = Instant::now();
    let err = conn
        .run(
            LONG,
            &[],
            RunOptions::gui().with_statement_timeout_secs(Some(10)),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Cancelled),
        "effective timeout is the strictest (1s), not the per-call 10s: {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    let rows = wait_history(&svc, &id, |rows| {
        rows.iter().any(|e| e.outcome == Outcome::Cancelled)
    })
    .await;
    let row = rows
        .iter()
        .find(|e| e.outcome == Outcome::Cancelled)
        .unwrap();
    assert!(
        row.error.as_deref().is_some_and(|e| e.contains("timeout")),
        "{row:?}"
    );
    assert!(row.executed_at.0 + 500 < UnixMillis::now().0 || row.duration_ms.unwrap_or(0) >= 200);
}

#[tokio::test]
async fn executed_at_is_the_start_not_the_completion() {
    let env = Env::new("stamp");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    let before = UnixMillis::now();
    let stream = conn.run(LONG, &[], RunOptions::default()).await.unwrap();
    let after_start = UnixMillis::now();
    tokio::time::sleep(Duration::from_millis(400)).await;
    drop(stream);
    let rows = wait_history(&svc, &id, |rows| !rows.is_empty()).await;
    let row = &rows[0];
    assert!(
        row.executed_at.0 <= after_start.0 + 50,
        "executed_at {} is after the start window {}",
        row.executed_at.0,
        after_start.0
    );
    assert!(row.executed_at.0 + 1 >= before.0);
    assert!(row.duration_ms.unwrap_or(0) >= 200, "{row:?}");
}

#[tokio::test]
async fn stopping_after_result_end_records_ok_not_cancelled() {
    let env = Env::new("eof");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    let mut stream = conn
        .run("SELECT 1", &[], RunOptions::default())
        .await
        .unwrap();
    let mut saw_end = false;
    while let Some(event) = stream.next().await {
        if matches!(event.unwrap(), fg_driver::Event::ResultEnd { .. }) {
            saw_end = true;
            break;
        }
    }
    assert!(saw_end);
    drop(stream);
    let rows = wait_history(&svc, &id, |rows| !rows.is_empty()).await;
    assert_eq!(rows[0].outcome, Outcome::Ok, "{rows:?}");
    assert!(rows[0].error.is_none(), "{rows:?}");
}

#[tokio::test]
async fn dropping_before_the_queued_error_keeps_that_error() {
    let env = Env::new("queued-err");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    let mut stream = conn
        .run(
            "SELECT 1; SELECT * FROM missing_table_xyz",
            &[],
            RunOptions::default(),
        )
        .await
        .unwrap();
    let mut saw_end = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            fg_driver::Event::ResultEnd { .. } if !saw_end => {
                saw_end = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_end);
    drop(stream);
    let rows = wait_history(&svc, &id, |rows| !rows.is_empty()).await;
    assert_eq!(rows[0].outcome, Outcome::Error, "{rows:?}");
    assert!(
        rows[0].error.as_deref().is_some_and(|e| !e.is_empty()),
        "{rows:?}"
    );
}

#[tokio::test]
async fn agent_audit_uses_the_client_id_once() {
    let env = Env::new("actor");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    conn.run("SELECT 1", &[], RunOptions::agent("claude-desktop"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let audit = svc
        .query_audit(AuditFilter {
            connection_id: Some(id.clone()),
            mcp_client: Some("claude-desktop".into()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert!(matches!(&audit[0].actor, Actor::Mcp { client } if client == "claude-desktop"));
    let all = svc
        .query_audit(AuditFilter {
            connection_id: Some(id),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    let _ = (AuditId::new(), QuerySource::Mcp, UnixMillis::now());
}

#[tokio::test]
async fn startup_sql_cannot_change_persistent_header_state() {
    let env = Env::new("header");
    let svc = env.service();
    let mut cfg = env.sqlite("db");
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    cfg.startup_sql = vec!["PRAGMA user_version = 123".into()];
    let saved = svc
        .save_connection(cfg.clone(), PasswordUpdate::Keep)
        .await
        .unwrap();
    let err = must_err(svc.open(&saved.id).await);
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    let err = svc.test_connection(cfg, None).await.unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");

    let mut plain = env.sqlite("db");
    plain.startup_sql.clear();
    let conn = open_saved(&svc, plain).await;
    assert_eq!(cell(&conn, "PRAGMA user_version").await, 0);
}

#[tokio::test]
async fn startup_query_only_cannot_be_disabled() {
    let env = Env::new("qonly");
    let svc = env.service();
    let mut cfg = env.sqlite("db");
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    for sql in [
        "PRAGMA query_only = OFF",
        "PRAGMA query_only=OFF",
        "PRAGMA query_only(0)",
    ] {
        cfg.startup_sql = vec![sql.into()];
        let err = svc.test_connection(cfg.clone(), None).await.unwrap_err();
        assert!(matches!(err, Error::PolicyDenied(_)), "{sql}: {err:?}");
    }
}

#[tokio::test]
async fn allowed_startup_pragma_still_runs_on_a_read_only_connection() {
    let env = Env::new("allow-pragma");
    let svc = env.service();
    let mut cfg = env.sqlite("db");
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    cfg.startup_sql = vec!["PRAGMA foreign_keys = ON".into()];
    let conn = open_saved(&svc, cfg).await;
    assert_eq!(cell(&conn, "PRAGMA foreign_keys").await, 1);
}

#[tokio::test]
async fn read_only_execute_allows_a_pragma_query() {
    let env = Env::new("pragma-read");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(&conn, "CREATE TABLE t (n INTEGER)").await;
    let id = conn.id().clone();
    drop(conn);
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    let sets = conn
        .execute("PRAGMA table_info(t)", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert!(!sets[0].rows.is_empty());
    let err = must_err(
        conn.execute("PRAGMA user_version = 9", &[], ExecOptions::default())
            .await,
    );
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "PRAGMA user_version").await, 0);
}

#[tokio::test]
async fn unreadable_exec_settings_payload_fails_closed() {
    let env = Env::new("bad-exec");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    svc._test_write_unreadable_setting(ExecSettings::KEY, "\"not-an-object\"")
        .await
        .unwrap();
    let err = conn
        .run(
            "DELETE FROM t",
            &[],
            RunOptions::gui().with_max_rows(Some(1_000_000)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Storage(_)),
        "an unreadable exec settings row must not fall open to the default cap: {err:?}"
    );
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);
}

#[tokio::test]
async fn dropping_save_still_invalidates_the_open_connection() {
    let env = Env::new("drop-save");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();
    let mut pause = svc._test_pause_save();
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    let svc2 = svc.clone();
    let saving = tokio::spawn(async move {
        svc2.save_connection(cfg, PasswordUpdate::Set("pw".into()))
            .await
    });
    pause.wait().await;
    saving.abort();
    let _ = saving.await;
    pause.release();
    pause.done().await;
    let err = conn
        .run("SELECT n FROM t", &[], RunOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Connection(_)),
        "the retired connection must not lease after the caller dropped save: {err:?}"
    );
    let stored = stored(svc.connections().await.unwrap(), &id);
    assert_eq!(stored.effective_safe_mode(), SafeMode::ReadOnly);
}

#[tokio::test]
async fn close_after_the_lease_does_not_let_the_statement_run() {
    let env = Env::new("lease-close");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(&conn, "PRAGMA user_version = 1").await;
    let id = conn.id().clone();
    let mut pause = svc._test_pause_before_execute(&id).unwrap();
    let conn2 = Arc::clone(&conn);
    let running = tokio::spawn(async move {
        conn2
            .run("PRAGMA user_version = 9", &[], RunOptions::default())
            .await
    });
    pause.wait().await;
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    pause.release();
    let err = running.await.unwrap().unwrap_err();
    assert!(
        matches!(err, Error::Connection(_) | Error::Cancelled),
        "{err:?}"
    );
    drop(conn);
    let mut plain = stored(svc.connections().await.unwrap(), &id);
    plain.safe_mode = Some(SafeMode::Normal);
    plain.startup_sql.clear();
    svc.save_connection(plain, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    assert_eq!(cell(&conn, "PRAGMA user_version").await, 1);
}

#[tokio::test]
async fn result_start_is_delivered_before_the_slow_statement_finishes() {
    let env = Env::new("stream-now");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let mut stream = conn.run(LONG, &[], RunOptions::default()).await.unwrap();
    let first = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
    let event = first
        .expect("ResultStart must reach the caller before the next event exists")
        .unwrap()
        .unwrap();
    assert!(matches!(event, Event::ResultStart { .. }), "{event:?}");
    drop(stream);
}

#[tokio::test]
async fn a_two_statement_script_delivers_events_in_order() {
    let env = Env::new("stream-order");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let mut stream = conn
        .run("SELECT 1; SELECT 2", &[], RunOptions::default())
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Some(event) = stream.next().await {
        seen.push(match event.unwrap() {
            Event::ResultStart { index, .. } => format!("start{index}"),
            Event::Rows(_) => "rows".into(),
            Event::ResultEnd { .. } => "end".into(),
            Event::Notice(_) => "notice".into(),
            _ => "other".into(),
        });
    }
    assert_eq!(
        seen,
        [
            "start0".to_owned(),
            "rows".to_owned(),
            "end".to_owned(),
            "start1".to_owned(),
            "rows".to_owned(),
            "end".to_owned(),
        ]
    );
}

#[tokio::test]
async fn two_statements_under_the_threshold_are_not_cancelled_as_a_script() {
    let env = Env::new("per-stmt");
    let svc = env.service();
    svc.set_exec_settings(ExecSettings {
        statement_timeout_secs: Some(1),
        ..ExecSettings::default()
    })
    .await
    .unwrap();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let sets = conn
        .run("SELECT 1; SELECT 2", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets.len(), 2);
}

#[tokio::test]
async fn history_failure_still_writes_audit_and_drain_reports_it() {
    let env = Env::new("hist-fail");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let id = conn.id().clone();
    svc._test_fail_next_history();
    let err = conn
        .run("SELECT 1", &[], RunOptions::agent("claude"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err:?}");
    let audit = svc
        .query_audit(AuditFilter {
            connection_id: Some(id),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    let err = svc.drain_records().unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err:?}");
}
