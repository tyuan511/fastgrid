//! SQLite driver on top of `rusqlite` (bundled SQLite).
//!
//! rusqlite is synchronous, so each session owns one connection behind a mutex and runs
//! work on tokio's blocking pool. Queries stream through a bounded channel; a slow consumer
//! blocks the blocking thread instead of buffering the result set.

mod meta;
mod run;
mod types;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use fg_core::{
    Capabilities, ConnectionConfig, ConnectionParams, DbKind, Dialect, Error, ObjectInfo,
    ObjectPath, Relation, Result, SqliteDialect, TableDetail, Value,
};
use rusqlite::{Connection, InterruptHandle, OpenFlags};

use crate::{CancelHandle, Driver, ExecOptions, ExecutionStream, Session};

pub struct SqliteDriver;

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            multiple_databases: false,
            namespaces: true,
            multi_statement: true,
            transactions: true,
            transactional_ddl: true,
            cancel: true,
            statement_timeout: false,
            read_only_session: true,
            returning: true,
            show_ddl: true,
            row_estimates: false,
            column_origin: true,
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &SqliteDialect
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        _password: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let config = config.clone();
        let conn = tokio::task::spawn_blocking(move || open(&config))
            .await
            .map_err(|e| Error::Other(format!("blocking task failed: {e}")))??;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(Box::new(SqliteSession {
            conn: Arc::new(Mutex::new(conn)),
            cancel: Cancel::new(interrupt),
        }))
    }
}

/// Opening never creates a file implicitly (a typo'd path should fail, not silently create
/// an empty database); set option `create=true` for that. `readonly=true` opens read-only.
fn open(config: &ConnectionConfig) -> Result<Connection> {
    let ConnectionParams::File { path } = &config.params else {
        return Err(Error::Connection("SQLite needs a file path".into()));
    };
    let conn = if path == ":memory:" {
        Connection::open_in_memory()
    } else {
        let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if config.option_enabled("readonly") {
            flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
        } else {
            flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
            if config.option_enabled("create") {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            }
        }
        Connection::open_with_flags(path, flags)
    }
    .map_err(|e| Error::Connection(e.to_string()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| Error::Connection(e.to_string()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| Error::Connection(e.to_string()))?;
    Ok(conn)
}

pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    cancel: Cancel,
}

/// Delivers a cancel to a session's SQLite connection.
///
/// `sqlite3_interrupt()` is only seen by a statement that is *already* stepping: an interrupt set
/// before the first `sqlite3_step()` is cleared when that step begins, so it is lost (measured on
/// rusqlite 0.40 / bundled SQLite: 25 of 25 pre-step interrupts were ignored, 10 of 10 delivered
/// mid-step aborted the query). A UI "stop" pressed the moment a query starts therefore lands in
/// that window now and then, and the query keeps burning a core with nobody able to cancel it.
///
/// Two mechanisms cover the whole span:
/// * `Cancel::cancelled` is our own flag, never cleared by SQLite, and consulted at every
///   checkpoint in `run::run` - it catches a cancel that arrives before a statement starts.
/// * While a statement is in flight, the cancel is re-delivered every few milliseconds, so an
///   interrupt that arrived too early is handed to SQLite at a moment when it can observe it.
///
/// Both flag writes are `SeqCst`, and the guard is published before the first checkpoint is read,
/// so a cancel racing with a starting statement always either sees the statement in flight (and
/// retries) or has its flag visible to that checkpoint.
#[derive(Clone)]
struct Cancel {
    interrupt: Arc<InterruptHandle>,
    cancelled: Arc<AtomicBool>,
    /// Set for as long as one of this session's statements may be inside SQLite.
    in_flight: Arc<AtomicBool>,
}

impl Cancel {
    fn new(interrupt: Arc<InterruptHandle>) -> Self {
        Self {
            interrupt,
            cancelled: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Forget the previous statement's cancel; call before starting a new one.
    fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    /// The flag `run::run` reads at its checkpoints.
    fn flag(&self) -> &AtomicBool {
        &self.cancelled
    }

    /// Publish that a statement is about to run. The returned guard un-publishes on drop, panic
    /// included, so the retry loop below cannot outlive the statement.
    fn statement(&self) -> InFlight {
        self.in_flight.store(true, Ordering::SeqCst);
        InFlight(self.in_flight.clone())
    }

    fn handle(&self) -> CancelHandle {
        let interrupt = self.interrupt.clone();
        let cancelled = self.cancelled.clone();
        let in_flight = self.in_flight.clone();
        CancelHandle::new(move || {
            cancelled.store(true, Ordering::SeqCst);
            interrupt.interrupt();
            if !in_flight.load(Ordering::SeqCst) {
                // Nothing is stepping yet; the flag alone is enough, the statement will read it
                // at its first checkpoint.
                return;
            }
            let (interrupt, cancelled, in_flight) =
                (interrupt.clone(), cancelled.clone(), in_flight.clone());
            std::thread::spawn(move || {
                // 60s of poking is far beyond any statement we expect to be cancellable; after
                // that the loop gives up rather than outliving its purpose.
                for _ in 0..12_000 {
                    if !in_flight.load(Ordering::SeqCst) || !cancelled.load(Ordering::SeqCst) {
                        return;
                    }
                    interrupt.interrupt();
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        })
    }
}

struct InFlight(Arc<AtomicBool>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn lock(conn: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    // A panic while holding the lock cannot leave SQLite itself inconsistent.
    conn.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SqliteSession {
    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&lock(&conn)))
            .await
            .map_err(|e| Error::Other(format!("blocking task failed: {e}")))?
    }
}

#[async_trait]
impl Session for SqliteSession {
    async fn ping(&self) -> Result<()> {
        self.blocking(|c| {
            c.query_row("SELECT 1", [], |_| Ok(()))
                .map_err(run::map_err)
        })
        .await
    }

    async fn server_version(&self) -> Result<String> {
        self.blocking(|c| {
            c.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))
                .map(|v| format!("SQLite {v}"))
                .map_err(run::map_err)
        })
        .await
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        self.blocking(meta::list_namespaces).await
    }

    async fn list_objects(&self, namespace: Option<&str>) -> Result<Vec<ObjectInfo>> {
        let ns = namespace.unwrap_or("main").to_owned();
        self.blocking(move |c| meta::list_objects(c, &ns)).await
    }

    async fn describe(&self, path: &ObjectPath) -> Result<TableDetail> {
        let path = path.clone();
        self.blocking(move |c| meta::describe(c, &path)).await
    }

    async fn list_relations(&self, namespace: Option<&str>) -> Result<Vec<Relation>> {
        let ns = namespace.unwrap_or("main").to_owned();
        self.blocking(move |c| meta::list_relations(c, &ns)).await
    }

    async fn ddl(&self, path: &ObjectPath) -> Result<Option<String>> {
        let path = path.clone();
        self.blocking(move |c| meta::ddl(c, &path)).await
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        options: ExecOptions,
    ) -> Result<ExecutionStream> {
        let (tx, stream) = ExecutionStream::channel(self.cancel.handle());
        // Reset before spawning so a cancel issued right after this call is not lost.
        self.cancel.reset();
        let conn = self.conn.clone();
        let cancel = self.cancel.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        tokio::task::spawn_blocking(move || {
            // Publish before touching SQLite: from here on a cancel has to re-deliver its
            // interrupt, because the one it sends first can still land before the step starts.
            let _statement = cancel.statement();
            let guard = lock(&conn);
            if let Err(run::RunError::Db(e)) =
                run::run(&guard, &sql, &params, options, &tx, cancel.flag())
            {
                let _ = tx.blocking_send(Err(e));
            }
        });
        Ok(stream)
    }

    async fn set_read_only(&self, read_only: bool) -> Result<()> {
        let pragma = if read_only {
            "PRAGMA query_only = ON;"
        } else {
            "PRAGMA query_only = OFF;"
        };
        self.blocking(move |c| c.execute_batch(pragma).map_err(run::map_err))
            .await
    }

    fn cancel_handle(&self) -> CancelHandle {
        self.cancel.handle()
    }
}

pub(crate) fn quote(ident: &str) -> String {
    SqliteDialect.quote_ident(ident)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// About a second of pure computation that yields no row until it ends, so nothing but an
    /// interrupt can stop it - and it stops by itself eventually, which keeps a regression from
    /// hanging the suite.
    const BUSY: &str = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40000000) SELECT sum(i) FROM n";

    #[test]
    fn a_cancel_that_lands_before_the_first_step_still_stops_the_statement() {
        let conn = Connection::open_in_memory().unwrap();
        let cancel = Cancel::new(Arc::new(conn.get_interrupt_handle()));

        // The statement is published but has not reached SQLite yet: this is the window where
        // sqlite3_interrupt() is forgotten (verified: 25/25 pre-step interrupts ignored, 10/10
        // delivered mid-step abort). The cancel must therefore be re-delivered, not just sent.
        let statement = cancel.statement();
        cancel.handle().cancel();
        std::thread::sleep(Duration::from_millis(2));

        let interrupted = {
            let mut stmt = conn.prepare(BUSY).unwrap();
            let mut rows = stmt.raw_query();
            rows.next().is_err()
        };
        drop(statement);

        assert!(
            interrupted,
            "the retried interrupt must abort the statement, not let it run to completion"
        );
    }

    #[test]
    fn the_retry_stops_poking_once_the_statement_is_over() {
        let conn = Connection::open_in_memory().unwrap();
        let cancel = Cancel::new(Arc::new(conn.get_interrupt_handle()));
        let statement = cancel.statement();
        cancel.handle().cancel();
        drop(statement);
        // The guard is gone, so the retry thread must exit rather than interrupt this connection
        // forever (a later statement of the session would be killed by a stale cancel).
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT 1", [], |r| r.get(0))
                .unwrap(),
            1
        );
    }
}
