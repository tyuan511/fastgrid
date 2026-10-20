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
    ObjectPath, Result, TableDetail, Value, dialect_for,
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
            read_only_session: true,
            returning: true,
            show_ddl: true,
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        dialect_for(DbKind::Sqlite)
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
            interrupt,
            cancelled: Arc::new(AtomicBool::new(false)),
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
    interrupt: Arc<InterruptHandle>,
    /// Set by `cancel`, so a cancel that arrives before the statement starts still works.
    cancelled: Arc<AtomicBool>,
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
        let (tx, stream) = ExecutionStream::channel(self.cancel_handle());
        // Reset before spawning so a cancel issued right after this call is not lost.
        self.cancelled.store(false, Ordering::SeqCst);
        let conn = self.conn.clone();
        let cancelled = self.cancelled.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        tokio::task::spawn_blocking(move || {
            let guard = lock(&conn);
            if let Err(run::RunError::Db(e)) =
                run::run(&guard, &sql, &params, options, &tx, &cancelled)
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
        let interrupt = self.interrupt.clone();
        let cancelled = self.cancelled.clone();
        CancelHandle::new(move || {
            cancelled.store(true, Ordering::SeqCst);
            interrupt.interrupt();
        })
    }
}

pub(crate) fn quote(ident: &str) -> String {
    dialect_for(DbKind::Sqlite).quote_ident(ident)
}
