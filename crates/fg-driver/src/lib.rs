//! Database driver abstraction. See `docs/driver-abstraction.md`.
//!
//! * [`Driver`] is a stateless factory describing an engine (capabilities, dialect) and opening
//!   [`Session`]s.
//! * [`Session`] is one physical connection: metadata, statement execution, read-only switch,
//!   cancellation. Pooling lives above, in `fg-service`.
//! * Results are streamed as [`Event`]s through an [`ExecutionStream`]; drivers use [`Batcher`]
//!   so batching and row limits behave identically everywhere.

mod stream;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "mysql")]
pub mod mysql;

#[cfg(feature = "mssql")]
pub mod mssql;

use std::sync::Arc;

use async_trait::async_trait;
use fg_core::{
    Capabilities, ConnectionConfig, DbKind, Dialect, Error, ObjectInfo, ObjectPath, Relation,
    Result, TableDetail, Value,
};

pub use stream::{Batcher, CancelHandle, Event, ExecOptions, ExecutionStream, Push, ResultSet};

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;
    fn capabilities(&self) -> Capabilities;
    fn dialect(&self) -> &'static dyn Dialect;

    /// Open a session. `password` comes from the keychain and must never be logged.
    async fn connect(
        &self,
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Box<dyn Session>>;
}

#[async_trait]
pub trait Session: Send + Sync {
    /// Cheap liveness check ("Test connection", pool health).
    async fn ping(&self) -> Result<()>;

    async fn server_version(&self) -> Result<String>;

    /// Databases reachable from this connection. Empty unless
    /// `Capabilities::multiple_databases`.
    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// First-level containers shown under the connection (MySQL databases, PostgreSQL
    /// schemas, SQLite attached databases).
    async fn list_namespaces(&self) -> Result<Vec<String>>;

    /// Tables and views in a namespace (`None` = the session default).
    async fn list_objects(&self, namespace: Option<&str>) -> Result<Vec<ObjectInfo>>;

    async fn describe(&self, path: &ObjectPath) -> Result<TableDetail>;

    /// Every foreign key in a namespace, in one call (reverse references, ER diagrams, MCP).
    /// The default asks `describe` per table; drivers should override it with a bulk query.
    async fn list_relations(&self, namespace: Option<&str>) -> Result<Vec<Relation>> {
        let mut out = Vec::new();
        for object in self.list_objects(namespace).await? {
            if object.kind != fg_core::ObjectKind::Table {
                continue;
            }
            let detail = self.describe(&object.path).await?;
            out.extend(detail.foreign_keys.into_iter().map(|key| Relation {
                from: object.path.clone(),
                key,
            }));
        }
        Ok(out)
    }

    /// `CREATE` statement for an object, when `Capabilities::show_ddl`.
    async fn ddl(&self, _path: &ObjectPath) -> Result<Option<String>> {
        Ok(None)
    }

    /// Execute one or more statements. `params` bind to the (single) statement using the
    /// dialect's placeholders. Rows stream back; dropping the stream stops the query.
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        options: ExecOptions,
    ) -> Result<ExecutionStream>;

    /// Switch the session to read-only; used by the MCP policy layer. Engines that cannot
    /// enforce this return `Error::Unsupported` and callers must fall back to statement
    /// classification.
    async fn set_read_only(&self, _read_only: bool) -> Result<()> {
        Err(Error::Unsupported("read-only sessions".into()))
    }

    /// Handle that cancels whatever this session is currently running. Callable from any task.
    fn cancel_handle(&self) -> CancelHandle;
}

/// Look up the built-in driver for an engine.
pub fn driver_for(kind: DbKind) -> Result<Arc<dyn Driver>> {
    match kind {
        #[cfg(feature = "sqlite")]
        DbKind::Sqlite => Ok(Arc::new(sqlite::SqliteDriver)),
        #[cfg(feature = "postgres")]
        DbKind::Postgres => Ok(Arc::new(postgres::PostgresDriver)),
        #[cfg(feature = "mysql")]
        DbKind::MySql => Ok(Arc::new(mysql::MysqlDriver)),
        #[cfg(feature = "mssql")]
        DbKind::MsSql => Ok(Arc::new(mssql::MsSqlDriver)),
        other => Err(Error::Unsupported(format!(
            "{} driver is not available in this build",
            other.display_name()
        ))),
    }
}
