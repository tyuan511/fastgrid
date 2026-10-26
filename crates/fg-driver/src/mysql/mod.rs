//! MySQL / MariaDB driver over `mysql_async` with rustls. See `docs/roadmap.md` (M5).

mod meta;
mod run;
mod tls;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use fg_core::{
    Capabilities, ConnectionConfig, ConnectionParams, DbKind, Dialect, Error, ObjectInfo,
    ObjectPath, Relation, Result, TableDetail,
};
use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder};
use tokio::sync::Mutex;

use run::Canceller;
use tls::{install_provider, ssl_opts};

/// Stateless factory for MySQL / MariaDB sessions.
pub struct MysqlDriver;

#[async_trait]
impl crate::Driver for MysqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Databases are namespaces inside one connection; switching is
            // `USE`, not a reconnect (the service reconnects anyway, which
            // is correct but not minimal — see the roadmap note).
            multiple_databases: false,
            databases_are_namespaces: true,
            namespaces: true,
            multi_statement: true,
            transactions: true,
            // MySQL commits implicitly around DDL.
            transactional_ddl: false,
            cancel: true,
            statement_timeout: true,
            read_only_session: true,
            returning: false,
            show_ddl: true,
            row_estimates: true,
            column_origin: true,
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &fg_core::MySqlDialect
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
                "MySQL connections need a host, not a file".into(),
            ));
        };
        if socket.is_some()
            && let Some(tls) = &config.tls
            && tls.mode != fg_core::TlsMode::Disable
        {
            return Err(Error::Unsupported(
                "TLS over a Unix socket is not supported; use a TCP host".into(),
            ));
        }
        install_provider();
        let (ssl, prefer) = ssl_opts(config.tls.as_ref())?;
        let want_tls = ssl.is_some();
        let builder = base_builder(host, *port, user, password, database, socket, ssl);
        // The kill connection must mirror the session's *actual* transport:
        // when Prefer falls back to plain, a TLS kill connection would
        // fail against the same server.
        let session_opts: mysql_async::Opts = builder.into();
        let (mut conn, session_tls) = match Conn::new(session_opts).await {
            Ok(conn) => (conn, want_tls),
            Err(_) if prefer => {
                // Prefer falls back to plain when the server does not speak
                // TLS. If plain fails too, its error is the meaningful one:
                // the server actually answered it.
                let plain: mysql_async::Opts =
                    base_builder(host, *port, user, password, database, socket, None).into();
                (Conn::new(plain).await.map_err(meta::map_err)?, false)
            }
            Err(e) => return Err(meta::map_err(e)),
        };
        let conn_id = connection_id(&mut conn).await?;
        // Killing needs no database, but it needs the session's transport:
        // a server that requires TLS rejects a plain kill.
        let kill_ssl = if session_tls {
            ssl_opts(config.tls.as_ref())?.0
        } else {
            None
        };
        let kill_opts: mysql_async::Opts =
            base_builder(host, *port, user, password, &None, socket, kill_ssl).into();
        Ok(Box::new(MysqlSession {
            conn: Arc::new(Mutex::new(conn)),
            canceller: Arc::new(Canceller {
                flag: std::sync::atomic::AtomicBool::new(false),
                conn_id,
                kill_opts,
            }),
            default_namespace: config
                .default_namespace
                .clone()
                .or_else(|| database.clone()),
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn base_builder(
    host: &str,
    port: u16,
    user: &str,
    password: Option<&str>,
    database: &Option<String>,
    socket: &Option<String>,
    ssl: Option<mysql_async::SslOpts>,
) -> OptsBuilder {
    let mut builder = OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(port)
        .user(Some(user))
        .pass(password)
        .db_name(database.clone());
    if let Some(socket) = socket {
        builder = builder.prefer_socket(Some(true)).socket(Some(socket));
    }
    builder = builder.ssl_opts(ssl);
    builder
}

/// `SELECT CONNECTION_ID()` for the kill handle. Parsed by hand: the row
/// is a single integer column.
async fn connection_id(conn: &mut Conn) -> Result<u32> {
    let mut result = conn
        .query_iter("SELECT CONNECTION_ID()")
        .await
        .map_err(meta::map_err)?;
    let row = result.next().await.map_err(meta::map_err)?;
    let Some(row) = row else {
        return Err(Error::Connection(
            "server did not report a connection id".into(),
        ));
    };
    let values = row.unwrap();
    match values.first() {
        Some(mysql_common::Value::UInt(v)) => Ok(*v as u32),
        Some(mysql_common::Value::Int(v)) => Ok(*v as u32),
        // The text protocol sends integers as text.
        Some(mysql_common::Value::Bytes(b)) => String::from_utf8_lossy(b)
            .parse::<u32>()
            .map_err(|_| Error::Connection("server reported an unusable connection id".into())),
        _ => Err(Error::Connection(
            "server reported an unusable connection id".into(),
        )),
    }
}

pub struct MysqlSession {
    conn: Arc<Mutex<Conn>>,
    canceller: Arc<Canceller>,
    default_namespace: Option<String>,
}

impl MysqlSession {
    fn namespace<'a>(&'a self, namespace: Option<&'a str>) -> Option<&'a str> {
        namespace.or(self.default_namespace.as_deref())
    }
}

#[async_trait]
impl crate::Session for MysqlSession {
    async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.lock().await;
        conn.query_drop("SELECT 1").await.map_err(meta::map_err)
    }

    async fn server_version(&self) -> Result<String> {
        let mut conn = self.conn.lock().await;
        let rows =
            meta::query_rows(&mut conn, "SELECT VERSION()", mysql_async::Params::Empty).await?;
        rows.into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|v| match v {
                mysql_common::Value::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                _ => None,
            })
            .ok_or_else(|| Error::Connection("server did not report a version".into()))
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
        let mut conn = self.conn.lock().await;
        // Verified against MariaDB 10.11: this blocks autocommit writes
        // too (error 1792), so it is a real session-level guard, not
        // just a transaction default.
        let sql = if read_only {
            "SET SESSION TRANSACTION READ ONLY"
        } else {
            "SET SESSION TRANSACTION READ WRITE"
        };
        conn.query_drop(sql).await.map_err(meta::map_err)
    }

    fn cancel_handle(&self) -> crate::CancelHandle {
        crate::CancelHandle::new({
            let canceller = Arc::clone(&self.canceller);
            move || canceller.cancel()
        })
    }
}
