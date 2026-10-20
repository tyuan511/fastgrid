//! Domain model shared by every crate. No IO and no UI dependencies live here.
//!
//! The types in this crate are the vocabulary of the multi-database abstraction
//! (see `docs/driver-abstraction.md`): drivers translate their native protocol into
//! these types, everything above the driver layer only speaks these types.

mod capabilities;
mod connection;
mod dialect;
mod error;
mod meta;
mod types;
mod value;

pub use capabilities::Capabilities;
pub use connection::{ConnectionConfig, ConnectionId, ConnectionParams, DbKind};
pub use dialect::{Dialect, MySqlDialect, PostgresDialect, SqliteDialect, dialect_for};
pub use error::{Error, QueryError, Result};
pub use meta::{
    ColumnInfo, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind, ObjectPath, TableDetail,
};
pub use types::{ColumnMeta, DataType, TypeCategory};
pub use value::Value;
