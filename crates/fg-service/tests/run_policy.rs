//! Policy, history and audit for `OpenConnection::run`. Refusals must return before a session is
//! leased: with the pool size pinned at one and a long query holding that session, a refusal that
//! tried to acquire would block until the holder was dropped.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fg_core::{
    Actor, AuditEntry, AuditId, ConnectionConfig, ConnectionId, ConnectionParams, DangerReason,
    DbKind, Decision, DocumentId, Environment, Error, ExecSettings, Outcome, QueryDocument,
    QuerySource, SafeMode, Snippet, SnippetId, StatementClass, UnixMillis, Value,
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
            "fg-run-{}-{}-{name}",
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
        // Windows keeps the store file open until the last handle closes. Retry instead of
        // ignoring a single failure, and only after the test has dropped Service and Store.
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
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

/// A refusal that leases a session would sit behind `holder` until we drop it.
async fn assert_refused_without_a_session(
    conn: &Arc<OpenConnection>,
    sql: &str,
    options: RunOptions,
) -> Error {
    let holder = conn
        .execute(LONG, &[], ExecOptions::default())
        .await
        .unwrap();
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(3), conn.run(sql, &[], options)).await;
    drop(holder);
    let err = result
        .expect("run blocked on a session lease")
        .expect_err("a refusal must not succeed");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "refusal took {:?}",
        started.elapsed()
    );
    // Dropping the holder cancels it; wait until that session can be leased again so a later
    // assertion cannot hang the suite on a stuck interrupt.
    let ready = tokio::time::timeout(Duration::from_secs(10), cell(conn, "SELECT 1")).await;
    assert_eq!(ready.expect("session not reusable after cancel"), 1);
    err
}

async fn wait_history(
    svc: &Service,
    id: &ConnectionId,
    pred: impl Fn(&[fg_core::QueryHistoryEntry]) -> bool,
) -> Vec<fg_core::QueryHistoryEntry> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rows = svc.history(id, None, 20).await.unwrap();
        if pred(&rows) || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn read_only_update_is_refused_before_a_session_is_leased() {
    let env = Env::new("ro-update");
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
    assert_eq!(conn.safe_mode(), SafeMode::ReadOnly);

    let err =
        assert_refused_without_a_session(&conn, "UPDATE t SET n = 9", RunOptions::default()).await;
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);

    let err = conn
        .run(
            "UPDATE t SET n = 9",
            &[],
            RunOptions::gui_confirmed("UPDATE t SET n = 9", DbKind::Sqlite),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::PolicyDenied(_)),
        "confirmation cannot override read-only: {err:?}"
    );
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);

    let history = svc.history(&id, None, 10).await.unwrap();
    assert!(
        history.iter().all(|e| e.outcome == Outcome::NotRun),
        "{history:?}"
    );
    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn read_only_select_still_runs() {
    let env = Env::new("ro-select");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (4)",
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

    let sets = conn
        .run("SELECT n FROM t", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(int(&sets[0].rows[0][0]), 4);
}

#[tokio::test]
async fn alert_delete_and_drop_need_confirmation_normal_does_not() {
    let env = Env::new("alert");
    let svc = env.service();
    let mut cfg = env.sqlite("prod");
    cfg.environment = Environment::Production;
    let conn = open_saved(&svc, cfg).await;
    assert_eq!(
        conn.safe_mode(),
        SafeMode::Alert,
        "production's default safe mode is alert"
    );
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1);
         CREATE TABLE u (n INTEGER); INSERT INTO u VALUES (1)",
    )
    .await;

    let err = assert_refused_without_a_session(&conn, "DELETE FROM t", RunOptions::default()).await;
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);

    conn.run(
        "DELETE FROM t",
        &[],
        RunOptions::gui_confirmed("DELETE FROM t", DbKind::Sqlite),
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();
    assert_eq!(cell(&conn, "SELECT count(*) FROM t").await, 0);

    let err = assert_refused_without_a_session(&conn, "DROP TABLE u", RunOptions::default()).await;
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM u").await, 1);

    conn.run(
        "DROP TABLE u",
        &[],
        RunOptions::gui_confirmed("DROP TABLE u", DbKind::Sqlite),
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();
    let missing = conn
        .execute("SELECT n FROM u", &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(matches!(missing, Error::Query(_)), "{missing:?}");

    let mut normal = env.sqlite("dev");
    normal.safe_mode = Some(SafeMode::Normal);
    let conn = open_saved(&svc, normal).await;
    assert_eq!(conn.safe_mode(), SafeMode::Normal);
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (3)",
    )
    .await;
    conn.run("DELETE FROM t", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(cell(&conn, "SELECT count(*) FROM t").await, 0);
}

#[tokio::test]
async fn strict_confirms_the_same_dangerous_statements() {
    let env = Env::new("strict");
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
    cfg.safe_mode = Some(SafeMode::Strict);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();

    let err = assert_refused_without_a_session(&conn, "DELETE FROM t", RunOptions::default()).await;
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);

    conn.run(
        "DELETE FROM t",
        &[],
        RunOptions::gui_confirmed("DELETE FROM t", DbKind::Sqlite),
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();
    assert_eq!(cell(&conn, "SELECT count(*) FROM t").await, 0);
}

#[tokio::test]
async fn unknown_safe_mode_behaves_as_read_only() {
    let env = Env::new("unknown");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (2)",
    )
    .await;
    let id = conn.id().clone();
    drop(conn);
    let mut cfg = stored(svc.connections().await.unwrap(), &id);
    cfg.safe_mode = Some(SafeMode::Unknown);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&id).await.unwrap();
    assert_eq!(conn.safe_mode(), SafeMode::ReadOnly);

    let err =
        assert_refused_without_a_session(&conn, "UPDATE t SET n = 8", RunOptions::default()).await;
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 2);
}

#[tokio::test]
async fn parameters_with_multiple_statements_are_invalid_input() {
    let env = Env::new("params");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;

    let holder = conn
        .execute(LONG, &[], ExecOptions::default())
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        conn.run(
            "UPDATE t SET n = 5; SELECT 1",
            &[Value::Int(5)],
            RunOptions::default(),
        ),
    )
    .await;
    drop(holder);
    let err = result.expect("param check leased a session").unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");
    let ready = tokio::time::timeout(Duration::from_secs(10), cell(&conn, "SELECT n FROM t")).await;
    assert_eq!(ready.expect("session not reusable after cancel"), 1);
    assert!(
        svc.history(conn.id(), None, 10).await.unwrap().is_empty(),
        "a malformed call is not a run"
    );

    let sets = conn
        .run("SELECT ?", &[Value::Int(7)], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(int(&sets[0].rows[0][0]), 7);
}

#[tokio::test]
async fn multi_statement_script_streams_results_in_order() {
    let env = Env::new("multi");
    let svc = env.service();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    let sets = conn
        .run("SELECT 1 AS n; SELECT 2 AS n", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!(int(&sets[0].rows[0][0]), 1);
    assert_eq!(int(&sets[1].rows[0][0]), 2);
}

#[tokio::test]
async fn max_rows_is_honoured_and_reported_truncated() {
    let env = Env::new("cap");
    let svc = env.service();
    assert_eq!(svc.exec_settings().await.unwrap().max_rows, Some(100_000));
    svc.set_exec_settings(ExecSettings {
        max_rows: Some(2),
        ..ExecSettings::default()
    })
    .await
    .unwrap();
    let conn = open_saved(&svc, env.sqlite("db")).await;
    exec(
        &conn,
        "CREATE TABLE nums (n INTEGER);
         INSERT INTO nums VALUES (1), (2), (3), (4)",
    )
    .await;

    let mut stream = conn
        .run("SELECT n FROM nums ORDER BY n", &[], RunOptions::default())
        .await
        .unwrap();
    let mut rows = 0usize;
    while let Some(event) = stream.next().await {
        if let Event::Rows(batch) = event.unwrap() {
            rows += batch.len();
        }
    }
    assert_eq!(rows, 2);
    assert!(stream.truncated());

    let history = svc.history(conn.id(), None, 5).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].outcome, Outcome::Ok);
    assert_eq!(history[0].row_count, Some(2));
}

#[tokio::test]
async fn history_records_ok_error_cancelled_and_not_run() {
    let env = Env::new("history");
    let svc = env.service();
    let mut cfg = env.sqlite("db");
    cfg.safe_mode = Some(SafeMode::Alert);
    let conn = open_saved(&svc, cfg).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();

    conn.run("SELECT 42 AS marker", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    let err = conn
        .run(
            "SELECT * FROM missing_table_xyz",
            &[],
            RunOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Query(_)), "{err:?}");

    let stream = conn.run(LONG, &[], RunOptions::default()).await.unwrap();
    drop(stream);

    let err = conn
        .run("DELETE FROM t", &[], RunOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    let n = tokio::time::timeout(Duration::from_secs(10), cell(&conn, "SELECT n FROM t"))
        .await
        .expect("session not reusable after cancel");
    assert_eq!(n, 1);

    let rows = wait_history(&svc, &id, |rows| {
        rows.iter().any(|e| e.outcome == Outcome::Cancelled)
    })
    .await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    let outcomes: Vec<_> = rows.iter().map(|e| e.outcome).collect();
    assert!(outcomes.contains(&Outcome::Ok), "{outcomes:?}");
    assert!(outcomes.contains(&Outcome::Error), "{outcomes:?}");
    assert!(outcomes.contains(&Outcome::Cancelled), "{outcomes:?}");
    assert!(outcomes.contains(&Outcome::NotRun), "{outcomes:?}");
    assert_eq!(
        rows.iter()
            .filter(|e| e.outcome == Outcome::Ok)
            .map(|e| e.row_count)
            .collect::<Vec<_>>(),
        vec![Some(1)]
    );

    let found = svc.history(&id, Some("marker"), 10).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].source, QuerySource::Editor);
    assert_eq!(svc.history(&id, Some("marker"), 0).await.unwrap().len(), 0);
    assert_eq!(svc.history(&id, None, 1).await.unwrap().len(), 1);

    assert_eq!(svc.clear_history(&id).await.unwrap(), 4);
    assert!(svc.history(&id, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn audit_records_mcp_runs_and_refusals_but_not_editor() {
    let env = Env::new("audit");
    let svc = env.service();
    let mut cfg = env.sqlite("db");
    cfg.safe_mode = Some(SafeMode::Alert);
    let conn = open_saved(&svc, cfg).await;
    exec(
        &conn,
        "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    let id = conn.id().clone();

    conn.run("SELECT 1", &[], RunOptions::agent("claude"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    let err = conn
        .run("DELETE FROM t", &[], RunOptions::agent("claude"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
    assert_eq!(cell(&conn, "SELECT n FROM t").await, 1);

    conn.run("SELECT 2", &[], RunOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let editor_refusal = conn
        .run("DROP TABLE t", &[], RunOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(editor_refusal, Error::PolicyDenied(_)));

    let audit = svc
        .query_audit(AuditFilter {
            connection_id: Some(id.clone()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.len(), 2, "{audit:?}");
    let allowed = audit
        .iter()
        .find(|e| e.decision == Decision::Allowed)
        .expect("mcp select");
    assert_eq!(allowed.outcome, Outcome::Ok);
    assert_eq!(allowed.class, StatementClass::Read);
    assert!(matches!(&allowed.actor, fg_core::Actor::Mcp { client } if client == "claude"));
    let denied = audit
        .iter()
        .find(|e| e.decision == Decision::Denied)
        .expect("mcp delete");
    assert_eq!(denied.outcome, Outcome::NotRun);
    assert_eq!(denied.class, StatementClass::Write);
    assert_eq!(denied.danger, Some(DangerReason::DeleteWithoutWhere));
    assert!(audit.iter().all(|e| e.statement != "SELECT 2"));
    assert!(audit.iter().all(|e| e.statement != "DROP TABLE t"));

    svc.record_audit(AuditEntry {
        id: AuditId::new(),
        at: UnixMillis::now(),
        actor: Actor::System,
        connection_id: Some(id.clone()),
        connection_name: "db".into(),
        statement: "manual".into(),
        class: StatementClass::Read,
        danger: None,
        decision: Decision::Rejected,
        reason: Some("confirm timed out".into()),
        outcome: Outcome::NotRun,
        error: None,
        duration_ms: None,
        row_count: None,
    })
    .await
    .unwrap();
    let audit = svc
        .query_audit(AuditFilter {
            connection_id: Some(id),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert!(
        audit
            .iter()
            .any(|e| e.statement == "manual" && e.decision == Decision::Rejected)
    );
}

#[tokio::test]
async fn documents_and_snippets_round_trip_through_the_service() {
    let env = Env::new("docs");
    let svc = env.service();
    let conn = ConnectionId::new();
    let other = ConnectionId::new();

    let mut older = QueryDocument {
        id: DocumentId::new(),
        connection_id: conn.clone(),
        title: "old".into(),
        content: "SELECT 1".into(),
        saved_path: None,
        dirty: true,
        cursor_offset: Some(1),
        created_at: UnixMillis(1),
        updated_at: UnixMillis(1),
    };
    let newer = QueryDocument {
        id: DocumentId::new(),
        connection_id: conn.clone(),
        title: "new".into(),
        content: "SELECT 2".into(),
        saved_path: None,
        dirty: false,
        cursor_offset: None,
        created_at: UnixMillis(5),
        updated_at: UnixMillis(5),
    };
    let foreign = QueryDocument {
        connection_id: other.clone(),
        title: "other".into(),
        ..newer.clone()
    };
    // The clone shares `newer.id`; give the foreign draft its own id.
    let foreign = QueryDocument {
        id: DocumentId::new(),
        ..foreign
    };
    svc.save_document(older.clone()).await.unwrap();
    svc.save_document(newer.clone()).await.unwrap();
    svc.save_document(foreign.clone()).await.unwrap();

    let listed = svc.documents(&conn).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .filter_map(fg_store::StoredDocument::document)
            .map(|d| d.title.as_str())
            .collect::<Vec<_>>(),
        ["new", "old"]
    );
    older.content = "SELECT 9".into();
    older.updated_at = UnixMillis(9);
    svc.save_document(older.clone()).await.unwrap();
    assert_eq!(
        svc.documents(&conn).await.unwrap()[0]
            .document()
            .expect("draft")
            .content,
        "SELECT 9"
    );
    assert!(svc.delete_document(&older.id).await.unwrap());
    assert!(!svc.delete_document(&older.id).await.unwrap());
    assert_eq!(svc.documents(&conn).await.unwrap().len(), 1);
    assert_eq!(svc.documents(&other).await.unwrap().len(), 1);

    let global = Snippet {
        id: SnippetId::new(),
        connection_id: None,
        title: "global".into(),
        sql: "SELECT 1".into(),
        tags: vec!["g".into()],
        sort_order: 1,
        created_at: UnixMillis(1),
        updated_at: UnixMillis(1),
    };
    let own = Snippet {
        id: SnippetId::new(),
        connection_id: Some(conn.clone()),
        title: "own".into(),
        sql: "SELECT 2".into(),
        tags: Vec::new(),
        sort_order: 2,
        created_at: UnixMillis(2),
        updated_at: UnixMillis(2),
    };
    svc.save_snippet(global.clone()).await.unwrap();
    svc.save_snippet(own.clone()).await.unwrap();
    let visible = svc.snippets(Some(&conn)).await.unwrap();
    assert_eq!(
        visible
            .iter()
            .filter_map(fg_store::StoredSnippet::snippet)
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>(),
        ["global", "own"]
    );
    assert_eq!(svc.snippets(None).await.unwrap().len(), 2);
    assert!(svc.delete_snippet(&own.id).await.unwrap());
    assert!(!svc.delete_snippet(&own.id).await.unwrap());
    assert_eq!(
        svc.snippets(Some(&conn))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|item| item.snippet().cloned())
            .collect::<Vec<_>>(),
        vec![global]
    );
}
