//! The agent write gate in `OpenConnection::run`: writes need
//! `mcp.allow_write` plus a person resolving the pending request.
//! Read-only still refuses before any request is filed, and a service
//! with no event listener denies at once (headless: no GUI attached).
//!
//! SQLite runs on a local file and never skips. Run with
//! `-- --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, SafeMode};
use fg_service::{
    KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, RunOptions, Service,
    ServiceEvent, ServiceOptions,
};

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-mcp-policy-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("d.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        Self { dir, service }
    }

    async fn open(&self, allow_write: bool, safe_mode: Option<SafeMode>) -> Arc<OpenConnection> {
        let mut c = ConnectionConfig::new(
            "c",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self.dir.join("d.db").to_string_lossy().into_owned(),
            },
        );
        c.mcp.allow_write = allow_write;
        c.safe_mode = safe_mode;
        let c = self
            .service
            .save_connection(c, PasswordUpdate::Keep)
            .await
            .unwrap();
        self.service.open(&c.id).await.unwrap()
    }

    fn count(&self) -> i64 {
        let c = rusqlite::Connection::open(self.dir.join("d.db")).unwrap();
        c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
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

async fn agent_run(conn: &Arc<OpenConnection>, sql: &str) -> Result<(), fg_core::Error> {
    let stream = conn.run(sql, &[], RunOptions::agent("test-client")).await?;
    stream.collect().await?;
    Ok(())
}

/// Resolve the first pending write the service broadcasts. The
/// subscription is created by the caller before the run starts, so the
/// run's no-listener check always sees a listener.
async fn resolve_first(
    mut events: tokio::sync::broadcast::Receiver<ServiceEvent>,
    service: Service,
    approved: bool,
) {
    loop {
        match events.recv().await {
            Ok(ServiceEvent::McpWriteRequested { request }) => {
                assert!(service.resolve_mcp_write(request.id, approved));
                return;
            }
            Ok(_) => continue,
            Err(_) => panic!("event channel closed before a write request arrived"),
        }
    }
}

#[tokio::test]
async fn agent_write_denied_without_allow_write() {
    let env = Env::new("no-allow");
    let conn = env.open(false, None).await;
    let err = agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')")
        .await
        .unwrap_err();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)), "{err:?}");
    assert!(err.to_string().contains("not enabled"), "{err}");
    assert_eq!(env.count(), 0);
    assert!(env.service.pending_mcp_writes().is_empty());
    env.service.close_all();
}

#[tokio::test]
async fn agent_write_denied_without_listener() {
    let env = Env::new("no-listener");
    let conn = env.open(true, None).await;
    // No subscription: nothing can approve, so the denial is immediate
    // instead of waiting out the approval timeout.
    let err = agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')")
        .await
        .unwrap_err();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)), "{err:?}");
    assert!(err.to_string().contains("no GUI"), "{err}");
    assert_eq!(env.count(), 0);
    env.service.close_all();
}

#[tokio::test]
async fn agent_write_approved_by_gui() {
    let env = Env::new("approved");
    let conn = env.open(true, None).await;
    let service = env.service.clone();
    let events = service.subscribe();
    let resolver = tokio::spawn(resolve_first(events, service, true));
    agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')")
        .await
        .unwrap();
    resolver.await.unwrap();
    assert_eq!(env.count(), 1);
    assert!(env.service.pending_mcp_writes().is_empty());
    env.service.close_all();
}

#[tokio::test]
async fn agent_write_denied_by_gui() {
    let env = Env::new("denied");
    let conn = env.open(true, None).await;
    let service = env.service.clone();
    let events = service.subscribe();
    let resolver = tokio::spawn(resolve_first(events, service, false));
    let err = agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')")
        .await
        .unwrap_err();
    resolver.await.unwrap();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)), "{err:?}");
    assert!(err.to_string().contains("denied"), "{err}");
    assert_eq!(env.count(), 0);
    env.service.close_all();
}

#[tokio::test]
async fn read_only_refuses_before_any_request_is_filed() {
    let env = Env::new("readonly");
    let conn = env.open(true, Some(SafeMode::ReadOnly)).await;
    // A listener exists, but read-only refuses before the gate, so no
    // request is ever filed for the GUI to answer.
    let _events = env.service.subscribe();
    let err = agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')")
        .await
        .unwrap_err();
    assert!(matches!(err, fg_core::Error::PolicyDenied(_)), "{err:?}");
    assert!(err.to_string().contains("read-only"), "{err}");
    assert_eq!(env.count(), 0);
    assert!(env.service.pending_mcp_writes().is_empty());
    env.service.close_all();
}

#[tokio::test]
async fn agent_run_merges_connection_mcp_row_cap() {
    let env = Env::new("cap");
    let mut c = ConnectionConfig::new(
        "c",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: env.dir.join("d.db").to_string_lossy().into_owned(),
        },
    );
    c.mcp.max_rows = Some(1);
    let c = env
        .service
        .save_connection(c, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = env.service.open(&c.id).await.unwrap();
    {
        let db = rusqlite::Connection::open(env.dir.join("d.db")).unwrap();
        db.execute_batch(
            "INSERT INTO t VALUES (1, 'a'); INSERT INTO t VALUES (2, 'b'); INSERT INTO t VALUES (3, 'c');",
        )
        .unwrap();
    }
    // The call asks for 100 rows; the connection's MCP cap of 1 wins.
    let options = RunOptions::agent("test-client").with_max_rows(Some(100));
    let sets = conn
        .run("SELECT * FROM t ORDER BY id", &[], options)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].rows.len(), 1);
    assert!(sets[0].truncated);
    env.service.close_all();
}

#[tokio::test]
async fn resolving_an_unknown_write_changes_nothing() {
    let env = Env::new("unknown");
    assert!(!env.service.resolve_mcp_write(999_999, true));
    // Resolving twice: the second answer finds nothing live.
    let conn = env.open(true, None).await;
    let service = env.service.clone();
    let events = service.subscribe();
    let resolver = tokio::spawn(async move {
        let mut events = events;
        let request = loop {
            match events.recv().await {
                Ok(ServiceEvent::McpWriteRequested { request }) => break request,
                Ok(_) => continue,
                Err(_) => panic!("event channel closed"),
            }
        };
        assert!(service.resolve_mcp_write(request.id, true));
        assert!(!service.resolve_mcp_write(request.id, true));
    });
    agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')")
        .await
        .unwrap();
    resolver.await.unwrap();
    assert_eq!(env.count(), 1);
    env.service.close_all();
}

#[tokio::test]
async fn pending_list_shows_an_unresolved_write() {
    let env = Env::new("pending");
    let conn = env.open(true, None).await;
    // Hold a listener so the run waits instead of denying at once, and
    // check the pending list while it waits.
    let _events = env.service.subscribe();
    let run = tokio::spawn({
        let conn = Arc::clone(&conn);
        async move { agent_run(&conn, "INSERT INTO t VALUES (1, 'amy')").await }
    });
    // The request must appear promptly; poll briefly rather than
    // sleeping a fixed time.
    let mut found = false;
    for _ in 0..100 {
        if !env.service.pending_mcp_writes().is_empty() {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(found, "the pending write never appeared");
    let pending = env.service.pending_mcp_writes();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].connection_name, "c");
    assert!(pending[0].sql.contains("INSERT INTO t"));
    assert!(env.service.resolve_mcp_write(pending[0].id, true));
    run.await.unwrap().unwrap();
    assert_eq!(env.count(), 1);
    env.service.close_all();
}
