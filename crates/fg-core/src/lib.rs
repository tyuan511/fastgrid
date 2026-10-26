//! Domain model shared by every crate. No IO and no UI dependencies live here.
//!
//! These types are the vocabulary of the multi-database abstraction
//! (`docs/driver-abstraction.md`) and of everything persisted or exposed externally
//! (`docs/data-model.md`): changing them is expensive, so they are designed to grow by
//! *adding* fields and variants only (see [`compat`]).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod compat;

mod capabilities;
mod connection;
mod dialect;
mod edit;
mod error;
mod id;
mod meta;
mod policy;
mod query;
mod records;
mod secret;
mod types;
mod value;
mod workspace;

pub use capabilities::Capabilities;
pub use connection::{
    ConnectionConfig, ConnectionGroup, ConnectionParams, DbKind, McpAccess, SshAuth, SshConfig,
    Timeouts, TlsConfig, TlsMode,
};
pub use dialect::{Dialect, MySqlDialect, PostgresDialect, SqliteDialect, dialect_for};
pub use edit::{CellValue, Changeset, RowChange, RowKey};
pub use error::{Error, QueryError, Result};
pub use id::{
    AuditId, ConnectionId, DocumentId, GroupId, HistoryId, McpClientId, SnippetId, TabId,
    UnixMillis,
};
pub use meta::{
    CheckConstraint, ColumnInfo, ForeignKeyInfo, GeneratedColumn, IndexColumn, IndexInfo,
    ObjectInfo, ObjectKind, ObjectPath, ReferentialAction, Relation, TableDetail, TriggerInfo,
};
pub use policy::{DangerReason, Environment, SafeMode, StatementClass, TagColor};
pub use query::{Filter, FilterOp, NullsOrder, SortKey, TableQuery};
pub use records::{
    Actor, AuditEntry, Decision, McpClient, Outcome, QueryDocument, QueryHistoryEntry, QuerySource,
    Snippet,
};
pub use secret::{SecretKind, SecretRecord};
pub use types::{ColumnMeta, ColumnOrigin, DataType, TypeCategory, TypeParams};
pub use value::{LargeKind, Truncated, Value};
pub use workspace::{
    AppearanceSettings, BackupNudge, EditorSettings, ExecSettings, GridSettings,
    KeybindingOverrides, McpHttpSettings, McpSettings, PrivacySettings, RowHeight, SettingGroup,
    SyncBackendKind, SyncSettings, TabKind, TabState, TableViewMode, TableViewState, ThemeMode,
    WorkspaceState,
};
