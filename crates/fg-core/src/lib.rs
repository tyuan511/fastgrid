//! Domain model shared by every crate: connections, schema metadata, values, errors.
//! No IO and no UI dependencies live here.

mod connection;
mod error;
mod value;

pub use connection::{ConnectionConfig, ConnectionId, DbKind};
pub use error::{Error, Result};
pub use value::Value;
