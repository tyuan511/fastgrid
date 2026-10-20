use serde::{Deserialize, Serialize};

/// What a driver can do. UI and MCP consult this instead of matching on `DbKind`, so a new
/// driver lights up (or hides) features without touching upper layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `Session::list_databases` is meaningful and the connection can switch between them.
    pub multiple_databases: bool,
    /// Objects live in namespaces (MySQL databases, PostgreSQL schemas, SQLite `main`...).
    pub namespaces: bool,
    /// Multiple `;`-separated statements in a single `execute` call.
    pub multi_statement: bool,
    pub transactions: bool,
    /// DDL statements can be rolled back (PostgreSQL yes, MySQL no).
    pub transactional_ddl: bool,
    /// Running statements can be cancelled from another task.
    pub cancel: bool,
    /// `ExecOptions::statement_timeout_secs` is enforced by the driver.
    pub statement_timeout: bool,
    /// The session can be switched to read-only (`Session::set_read_only`).
    pub read_only_session: bool,
    /// `INSERT/UPDATE/DELETE ... RETURNING`.
    pub returning: bool,
    /// `Session::ddl` can return a CREATE statement.
    pub show_ddl: bool,
    /// `ObjectInfo::row_estimate` is populated.
    pub row_estimates: bool,
    /// Result columns carry `ColumnMeta::origin` (needed for editable query results).
    pub column_origin: bool,
}
