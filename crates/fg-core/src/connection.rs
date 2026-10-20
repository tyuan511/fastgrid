use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Supported database engines. Adding a variant is the first step of adding a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DbKind {
    Sqlite,
    MySql,
    Postgres,
}

impl DbKind {
    pub fn display_name(self) -> &'static str {
        match self {
            DbKind::Sqlite => "SQLite",
            DbKind::MySql => "MySQL",
            DbKind::Postgres => "PostgreSQL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub String);

/// Where the database lives. Transport concerns (SSH tunnel, TLS) are handled above the
/// driver by `fg-service`, which rewrites host/port before handing the config to a driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionParams {
    /// Server reachable over TCP.
    Network {
        host: String,
        port: u16,
        user: String,
        database: Option<String>,
    },
    /// Embedded database stored in a file (or `:memory:`).
    File { path: String },
}

/// Connection settings. The password is never stored here; it lives in the OS keychain
/// and is passed to the driver separately when connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: ConnectionId,
    pub name: String,
    pub kind: DbKind,
    pub params: ConnectionParams,
    /// Driver-specific knobs (e.g. `readonly=true`, `create=true` for SQLite).
    /// Unknown keys must be ignored by drivers.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Marks a production connection: red tint in the UI, hidden from MCP by default.
    #[serde(default)]
    pub is_production: bool,
}

impl ConnectionConfig {
    pub fn option_enabled(&self, key: &str) -> bool {
        self.options
            .get(key)
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
    }
}
