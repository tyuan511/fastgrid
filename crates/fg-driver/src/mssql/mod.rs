//! Microsoft SQL Server driver over `tiberius` (TDS) with rustls.
//!
//! Three properties of the engine shape this module:
//!
//! * **No session-level read-only switch.** SQL Server has nothing like
//!   `SET TRANSACTION READ ONLY`, so `set_read_only` cannot delegate to the
//!   server. The driver enforces it itself: while a session is read-only, every
//!   statement is classified with [`fg_sql::classify`] and anything that may
//!   write is refused before it reaches the wire. That keeps the same
//!   two-layer guard as the other engines (classification plus engine
//!   enforcement) even though the enforcement lives in the driver.
//! * **Transactions must be plain batch text.** `BEGIN TRANSACTION` sent through
//!   the RPC path (`sp_executesql`) fails with "mismatching number of BEGIN and
//!   COMMIT statements" (error 266), so the runner sends statements without
//!   parameters as batches.
//! * **Cancellation is a `KILL`.** tiberius' Attention-based
//!   `Client::cancel_query` leaves the connection desynchronised (measured: the
//!   next statement silently returns no rows), so the canceller opens a second
//!   connection and kills the session by `@@SPID`. A refused `KILL` poisons the
//!   session so the pool discards it instead of reusing a half-driven wire.

mod meta;
mod run;
mod tls;
mod types;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use fg_core::{
    Capabilities, ConnectionConfig, ConnectionParams, DbKind, Dialect, Error, ObjectInfo,
    ObjectPath, Relation, Result, TableDetail,
};
use tiberius::{AuthMethod, Client, Config};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use run::Canceller;
use tls::install_provider;

/// Session settings SQL Server needs for filtered indexes, indexed views and
/// computed columns to be created and used. A client that leaves these at the
/// server defaults gets `Msg 1934 ... SET options have incorrect settings`
/// when it tries to create a filtered index, so they are set on every connect.
const SESSION_SETUP: &str = "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON; \
     SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; \
     SET CONCAT_NULL_YIELDS_NULL ON; SET NUMERIC_ROUNDABORT OFF;";

/// Stateless factory for SQL Server sessions.
pub struct MsSqlDriver;

#[async_trait]
impl crate::Driver for MsSqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::MsSql
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `list_databases` reads `sys.databases`; switching reconnects
            // because a login record carries the database.
            multiple_databases: true,
            databases_are_namespaces: false,
            // Schemas (`dbo`, custom ones) group objects inside a database.
            namespaces: true,
            multi_statement: true,
            transactions: true,
            // SQL Server rolls back DDL with the surrounding transaction.
            transactional_ddl: true,
            cancel: true,
            statement_timeout: true,
            // Enforced by this driver's statement classification, not by the
            // engine: see the module docs. Declared honestly as false because
            // the capability means "the engine can be switched to read-only",
            // and SQL Server cannot - `set_read_only` only sets a driver-side
            // flag. Callers must not read this as "writes are allowed".
            read_only_session: false,
            // `OUTPUT INSERTED.*` goes before `VALUES`, which the DML compiler
            // cannot express by appending a clause.
            returning: false,
            show_ddl: true,
            row_estimates: true,
            // Result metadata carries no table or column origin.
            column_origin: false,
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &fg_core::MsSqlDialect
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Box<dyn crate::Session>> {
        let ConnectionParams::Network {
            host,
            port,
            user,
            database,
            socket,
        } = &config.params
        else {
            return Err(Error::InvalidInput(
                "SQL Server connections need a host, not a file".into(),
            ));
        };
        if socket.is_some() {
            return Err(Error::Unsupported(
                "SQL Server does not listen on a Unix socket; use a TCP host".into(),
            ));
        }
        install_provider();
        let password = password
            .ok_or_else(|| Error::InvalidInput("SQL Server needs a password to log in".into()))?;
        let mut tiberius = Config::new();
        tiberius.host(host);
        tiberius.port(*port);
        tiberius.application_name("fastgrid");
        if let Some(database) = database
            && !database.is_empty()
        {
            tiberius.database(database);
        }
        tiberius.authentication(AuthMethod::sql_server(user, password));
        tls::apply(&mut tiberius, config.tls.as_ref())?;
        // `command_timeout` is a per-round-trip deadline that tiberius resets on
        // every delivered token, so a long query that produces no rows for
        // longer than the deadline would be aborted mid-stream and the
        // connection left unusable. Statement timeouts are the service's job
        // (and are enforced here through the canceller), so the driver-level
        // deadline is disabled rather than guessed.
        tiberius.command_timeout(None);
        let connect_timeout = Duration::from_secs(u64::from(config.timeouts.connect_secs));
        tiberius.handshake_timeout(Some(connect_timeout));

        let addr = tiberius.get_addr();
        let tcp = tokio::time::timeout(connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| Error::Connection(format!("connecting to {addr} timed out")))?
            .map_err(|e| Error::Connection(format!("connecting to {addr} failed: {e}")))?;
        let _ = tcp.set_nodelay(true);
        let mut client = tokio::time::timeout(
            connect_timeout,
            Client::connect(tiberius.clone(), tcp.compat_write()),
        )
        .await
        .map_err(|_| Error::Connection(format!("login to {addr} timed out")))?
        .map_err(run::map_err)?;

        // One batch, one round trip: these must be in effect before anything
        // else runs.
        let mut stream = client
            .simple_query(SESSION_SETUP)
            .await
            .map_err(run::map_err)?;
        drain(&mut stream).await?;
        drop(stream);
        let spid = session_id(&mut client).await?;

        Ok(Box::new(MsSqlSession {
            conn: Arc::new(Mutex::new(client)),
            canceller: Arc::new(Canceller {
                flag: AtomicBool::new(false),
                spid,
                config: tiberius,
                poisoned: Arc::new(AtomicBool::new(false)),
                cancels: Default::default(),
            }),
            default_namespace: config.default_namespace.clone(),
            read_only: AtomicBool::new(false),
        }))
    }
}

/// Read a stream to its end so the connection is left clean.
async fn drain(stream: &mut tiberius::QueryStream<'_>) -> Result<()> {
    use futures::TryStreamExt;
    while stream.try_next().await.map_err(run::map_err)?.is_some() {}
    Ok(())
}

/// `@@SPID`, needed by the canceller's `KILL`.
async fn session_id(client: &mut Client<tokio_util::compat::Compat<TcpStream>>) -> Result<i32> {
    let row = client
        .simple_query("SELECT @@SPID AS spid")
        .await
        .map_err(run::map_err)?
        .into_row()
        .await
        .map_err(run::map_err)?;
    let Some(row) = row else {
        return Err(Error::Connection(
            "the server did not report a session id".into(),
        ));
    };
    row.try_get::<i32, _>(0)
        .map_err(run::map_err)?
        .ok_or_else(|| Error::Connection("the server reported no session id".into()))
}

pub struct MsSqlSession {
    conn: Arc<Mutex<run::Conn>>,
    canceller: Arc<Canceller>,
    default_namespace: Option<String>,
    /// Set by `set_read_only`; consulted by `execute` before anything is sent.
    read_only: AtomicBool,
}

impl MsSqlSession {
    fn namespace<'a>(&'a self, namespace: Option<&'a str>) -> Option<&'a str> {
        namespace.or(self.default_namespace.as_deref())
    }
}

#[async_trait]
impl crate::Session for MsSqlSession {
    async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let mut stream = conn.simple_query("SELECT 1").await.map_err(run::map_err)?;
        drain(&mut stream).await
    }

    async fn server_version(&self) -> Result<String> {
        let mut conn = self.conn.lock().await;
        meta::server_version(&mut conn).await
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().await;
        meta::list_databases(&mut conn).await
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().await;
        meta::list_namespaces(&mut conn).await
    }

    async fn list_objects(&self, namespace: Option<&str>) -> Result<Vec<ObjectInfo>> {
        let mut conn = self.conn.lock().await;
        meta::list_objects(&mut conn, self.namespace(namespace)).await
    }

    async fn describe(&self, path: &ObjectPath) -> Result<TableDetail> {
        let mut conn = self.conn.lock().await;
        meta::describe(&mut conn, path).await
    }

    async fn list_relations(&self, namespace: Option<&str>) -> Result<Vec<Relation>> {
        let mut conn = self.conn.lock().await;
        meta::list_relations(&mut conn, self.namespace(namespace)).await
    }

    async fn ddl(&self, path: &ObjectPath) -> Result<Option<String>> {
        let mut conn = self.conn.lock().await;
        meta::ddl(&mut conn, path).await
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[fg_core::Value],
        options: crate::ExecOptions,
    ) -> Result<crate::ExecutionStream> {
        // SQL Server has no session-level read-only mode, so the guard is here:
        // a statement that may write never reaches the server on a read-only
        // session. The service classifies too; this is the engine-side layer.
        if self.read_only.load(Ordering::SeqCst) {
            for span in fg_sql::split_statements(sql, DbKind::MsSql) {
                let text = span.sql(sql);
                let info = fg_sql::classify(text, DbKind::MsSql);
                if info.class.may_write() {
                    return Err(Error::PolicyDenied(format!(
                        "`{}` is refused: this session is read-only",
                        first_words(text)
                    )));
                }
            }
        }
        let cancel = crate::CancelHandle::new({
            let canceller = Arc::clone(&self.canceller);
            move || canceller.cancel()
        });
        let (tx, stream) = crate::ExecutionStream::channel(cancel);
        let conn = Arc::clone(&self.conn);
        let canceller = Arc::clone(&self.canceller);
        let sql = sql.to_owned();
        let params = params.to_vec();
        tokio::spawn(async move {
            run::execute(conn, canceller, &sql, &params, options, tx).await;
        });
        Ok(stream)
    }

    async fn set_read_only(&self, read_only: bool) -> Result<()> {
        // Nothing to send: the guard lives in `execute`. Clearing the flag while
        // a transaction is open would let the rest of that transaction write,
        // which is the caller's decision, not the driver's.
        self.read_only.store(read_only, Ordering::SeqCst);
        Ok(())
    }

    fn cancel_handle(&self) -> crate::CancelHandle {
        crate::CancelHandle::new({
            let canceller = Arc::clone(&self.canceller);
            move || canceller.cancel()
        })
    }
}

/// A short prefix of a statement for refusal messages, so the user can tell
/// which statement of a script was blocked.
fn first_words(sql: &str) -> String {
    let mut words: Vec<&str> = sql.split_whitespace().take(10).collect();
    let text = words.join(" ");
    words.clear();
    text.chars().take(60).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn refusals_name_the_statement_that_was_blocked() {
        assert_eq!(
            first_words("  DELETE FROM t WHERE 1 = 1"),
            "DELETE FROM t WHERE 1 = 1"
        );
        let long = "UPDATE really_long_table_name SET column_one = 1 WHERE column_two = 2";
        assert!(first_words(long).chars().count() <= 60);
    }
}
