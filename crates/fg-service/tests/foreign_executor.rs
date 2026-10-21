//! Every public `async` method of this crate must be awaitable from a non-tokio executor: the GUI
//! runs on GPUI's executor, and only the service owns a tokio runtime. Awaiting a record ack used
//! to call `tokio::time::timeout`, which panics with "there is no reactor running" when the
//! caller's thread is not inside a tokio runtime.
//!
//! These tests drive the whole service from a plain `std` thread with a hand-rolled `block_on`, so
//! no reactor exists anywhere on the calling thread. `#[tokio::test]` would hide exactly the bug
//! this pins, which is why the rest of the suite never caught it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fg_core::{
    ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Error, Outcome, SafeMode, Value,
};
use fg_driver::{Event, ExecOptions};
use fg_service::{
    KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, RunOptions, Service,
    ServiceOptions,
};

/// Minimal executor: poll on this thread and park until a waker from any thread unparkes it.
/// `Thread::unpark` leaves a token behind, so a wake that lands before `park` is not lost.
fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

struct Env {
    dir: PathBuf,
    keys: KeychainKeyProvider<MemoryBackend>,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-foreign-{}-{}-{name}",
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
        let mut config = ConnectionConfig::new(
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
        config.options.insert("create".into(), "true".into());
        config
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Windows keeps the store file open until the last handle closes.
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

/// Seed a table and return the connection id, all awaited from the foreign executor.
fn seeded(svc: &Service, env: &Env) -> (Arc<OpenConnection>, ConnectionId) {
    let conn = block_on(async {
        let saved = svc
            .save_connection(env.sqlite("db"), PasswordUpdate::Keep)
            .await
            .unwrap();
        svc.open(&saved.id).await.unwrap()
    });
    block_on(async {
        conn.execute(
            "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
    });
    let id = conn.id().clone();
    (conn, id)
}

/// A refusal, an unconfirmed dangerous statement and a mismatched confirmation all record
/// `NotRun` before returning. Each of those paths awaited the ack and panicked from GPUI.
#[test]
fn refusals_are_recorded_when_awaited_from_a_foreign_executor() {
    let env = Env::new("refused");
    let svc = env.service();
    // Seeded through the foreign executor, then reopened under read-only below.
    let (_seeded, id) = seeded(&svc, &env);

    // Read-only refusal through `run`.
    block_on(async {
        let mut config = match svc.connections().await.unwrap().remove(0) {
            fg_store::StoredConnection::Ok(config) => config,
            other => panic!("{other:?}"),
        };
        config.safe_mode = Some(SafeMode::ReadOnly);
        svc.save_connection(config, PasswordUpdate::Keep)
            .await
            .unwrap()
    });
    let conn = block_on(svc.open(&id)).unwrap();
    let err = block_on(conn.run("DELETE FROM t", &[], RunOptions::gui())).unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");

    // A confirmation that does not match the script: refused before a session is leased.
    let err = block_on(conn.run(
        "DELETE FROM t",
        &[],
        RunOptions::gui_confirmed("SELECT 1", DbKind::Sqlite),
    ))
    .unwrap_err();
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");

    // `execute` refuses the same way through `record_blocked`, which is a different call site.
    let err = match block_on(conn.execute("DELETE FROM t", &[], ExecOptions::default())) {
        Err(err) => err,
        Ok(_) => panic!("a read-only connection must refuse a write"),
    };
    assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");

    let history = block_on(svc.history(&id, None, 10)).unwrap();
    assert_eq!(history.len(), 3, "{history:?}");
    assert!(history.iter().all(|e| e.outcome == Outcome::NotRun));
    assert_eq!(conn.id(), &id);
    drop(conn);
}

/// A run that finishes normally: `next()` and `collect()` both wait for the history row, which is
/// the other half of the panic.
#[test]
fn a_completed_run_is_awaited_from_a_foreign_executor() {
    let env = Env::new("completed");
    let svc = env.service();
    let (conn, id) = seeded(&svc, &env);

    let mut stream = block_on(conn.run("SELECT 1 AS one", &[], RunOptions::gui())).unwrap();
    let mut rows = Vec::new();
    while let Some(event) = block_on(stream.next()) {
        if let Event::Rows(batch) = event.unwrap() {
            rows.extend(batch);
        }
    }
    assert_eq!(rows, vec![vec![Value::Int(1)]]);
    assert_eq!(stream.record_error(), None);
    drop(stream);

    let sets = block_on(async {
        conn.run("SELECT 2 AS two; SELECT 3 AS three", &[], RunOptions::gui())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
    });
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[1].rows[0][0], Value::Int(3));

    let history = block_on(svc.history(&id, None, 10)).unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert!(history.iter().all(|e| e.outcome == Outcome::Ok));
    drop(conn);
}

/// A statement the engine refuses reaches `fail_start`, which records before returning the error.
/// That path awaits the ack too.
#[test]
fn a_failed_run_start_is_recorded_from_a_foreign_executor() {
    let env = Env::new("failed");
    let svc = env.service();
    let (conn, id) = seeded(&svc, &env);

    let mut stream = block_on(conn.run("SELECT * FROM no_such_table", &[], RunOptions::gui()))
        .expect("the stream is handed back before the first event");
    let mut failed = false;
    while let Some(event) = block_on(stream.next()) {
        if event.is_err() {
            failed = true;
        }
    }
    assert!(failed, "a missing table must surface as an error");
    drop(stream);

    let history = block_on(svc.history(&id, None, 10)).unwrap();
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0].outcome, Outcome::Error);
    drop(conn);
}
