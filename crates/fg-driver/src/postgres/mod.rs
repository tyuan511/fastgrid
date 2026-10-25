//! PostgreSQL driver over tokio-postgres with rustls.
//!
//! The session owns one `tokio_postgres::Client`; the connection future is spawned
//! onto the runtime. SSH tunnels and TLS termination are fg-service concerns in the
//! architecture docs, but this driver also accepts a plain host/port directly: the
//! service passes the tunnel's local endpoint as the host, so the driver never knows
//! whether a tunnel is in front of it. TLS here is the PostgreSQL wire encryption
//! (SSLRequest), configured from `ConnectionConfig::tls`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fg_core::{
    Capabilities, ConnectionConfig, ConnectionParams, DbKind, Dialect, Error, ObjectInfo,
    ObjectPath, PostgresDialect, Relation, Result, TableDetail, Value,
};
use tokio::runtime::Handle;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config, NoTls};

use crate::{CancelHandle, Driver, ExecOptions, ExecutionStream, Session};

mod meta;
mod run;
mod tls;
mod types;

use run::{CancelTls, PgCancel, map_err};
use tls::build_factory;

pub struct PostgresDriver;

#[async_trait]
impl Driver for PostgresDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            multiple_databases: true,
            namespaces: true,
            multi_statement: true,
            transactions: true,
            transactional_ddl: true,
            cancel: true,
            statement_timeout: true,
            read_only_session: true,
            returning: true,
            show_ddl: true,
            row_estimates: true,
            column_origin: true,
        }
    }

    fn dialect(&self) -> &'static dyn Dialect {
        &PostgresDialect
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let (host, port, user, database, socket) = match &config.params {
            ConnectionParams::Network {
                host,
                port,
                user,
                database,
                socket,
            } => (
                host.clone(),
                *port,
                user.clone(),
                database.clone(),
                socket.clone(),
            ),
            ConnectionParams::File { .. } => {
                return Err(Error::Connection(
                    "PostgreSQL needs a network connection, not a file path".into(),
                ));
            }
        };
        let tls_mode = config.tls.as_ref().map(|t| t.mode).unwrap_or_default();
        if socket.is_some() && tls_mode != fg_core::TlsMode::Disable {
            return Err(Error::Unsupported(
                "TLS over a Unix socket is not supported; use a TCP host".into(),
            ));
        }
        let mut pg = Config::new();
        if let Some(dir) = &socket {
            pg.host(dir);
        } else {
            pg.host(&host);
        }
        pg.port(port);
        pg.user(&user);
        if let Some(database) = &database {
            pg.dbname(database);
        }
        if let Some(password) = password {
            pg.password(password);
        }
        if config.timeouts.connect_secs > 0 {
            pg.connect_timeout(Duration::from_secs(u64::from(config.timeouts.connect_secs)));
        }
        pg.ssl_mode(match tls_mode {
            fg_core::TlsMode::Disable => SslMode::Disable,
            fg_core::TlsMode::Prefer => SslMode::Prefer,
            fg_core::TlsMode::Require
            | fg_core::TlsMode::VerifyCa
            | fg_core::TlsMode::VerifyFull => SslMode::Require,
            // `TlsMode` is non_exhaustive; unknown future modes degrade to Prefer.
            _ => SslMode::Prefer,
        });

        let (client, cancel) = match build_factory(config.tls.as_ref())? {
            None => {
                let (client, connection) = pg
                    .connect(NoTls)
                    .await
                    .map_err(|e| with_hint(map_err(e), &host, port))?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                let cancel = PgCancel::new(client.cancel_token(), CancelTls::Plain);
                (client, cancel)
            }
            Some(factory) => {
                let (client, connection) = pg
                    .connect(factory.clone())
                    .await
                    .map_err(|e| with_hint(map_err(e), &host, port))?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                let cancel = PgCancel::new(client.cancel_token(), CancelTls::Tls(factory));
                (client, cancel)
            }
        };
        Ok(Box::new(PgSession {
            client: Arc::new(client),
            cancel,
            config: Arc::new(config.clone()),
            handle: Handle::current(),
        }))
    }
}

fn with_hint(error: Error, host: &str, port: u16) -> Error {
    match error {
        Error::Connection(message) => Error::Connection(format!("{message} ({host}:{port})")),
        other => other,
    }
}

pub struct PgSession {
    // `tokio_postgres::Client` is not `Clone`; the session shares it with the
    // spawned execution task through `Arc`.
    client: Arc<Client>,
    cancel: PgCancel,
    config: Arc<ConnectionConfig>,
    handle: Handle,
}

impl PgSession {
    fn namespace(&self, namespace: Option<&str>) -> String {
        namespace
            .or(self.config.default_namespace.as_deref())
            .unwrap_or("public")
            .to_owned()
    }
}

#[async_trait]
impl Session for PgSession {
    async fn ping(&self) -> Result<()> {
        self.client
            .simple_query("SELECT 1")
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let row = self
            .client
            .query_one("SELECT current_setting('server_version')", &[])
            .await
            .map_err(map_err)?;
        Ok(row.get(0))
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT datname FROM pg_database WHERE datallowconn ORDER BY datname",
                &[],
            )
            .await
            .map_err(map_err)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn list_namespaces(&self) -> Result<Vec<String>> {
        meta::list_namespaces(&self.client).await
    }

    async fn list_objects(&self, namespace: Option<&str>) -> Result<Vec<ObjectInfo>> {
        let namespace = self.namespace(namespace);
        meta::list_objects(&self.client, &namespace).await
    }

    async fn describe(&self, path: &ObjectPath) -> Result<TableDetail> {
        meta::describe(&self.client, path).await
    }

    async fn ddl(&self, path: &ObjectPath) -> Result<Option<String>> {
        meta::ddl(&self.client, path).await
    }

    async fn list_relations(&self, namespace: Option<&str>) -> Result<Vec<Relation>> {
        let namespace = self.namespace(namespace);
        meta::list_relations(&self.client, &namespace).await
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        options: ExecOptions,
    ) -> Result<ExecutionStream> {
        let (tx, stream) = ExecutionStream::channel(self.cancel_handle());
        let client = self.client.clone();
        let cancel = self.cancel.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        tokio::spawn(async move {
            match run::run(&client, &cancel, &sql, &params, options, &tx).await {
                Ok(()) => {}
                Err(run::RunError::Db(e)) => {
                    let _ = tx.send(Err(e)).await;
                }
                Err(run::RunError::Closed) => {}
            }
        });
        Ok(stream)
    }

    async fn set_read_only(&self, read_only: bool) -> Result<()> {
        let value = if read_only { "on" } else { "off" };
        self.client
            .simple_query(&format!("SET default_transaction_read_only = {value}"))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    fn cancel_handle(&self) -> CancelHandle {
        let cancel = self.cancel.clone();
        let handle = self.handle.clone();
        CancelHandle::new(move || {
            let cancel = cancel.clone();
            handle.spawn(async move {
                cancel.fire().await;
            });
        })
    }
}
