use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ConnectionId, Environment, GroupId, SafeMode, TagColor, UnixMillis};

/// Supported database engines. Adding a variant is the first step of adding a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DbKind {
    Sqlite,
    MySql,
    Postgres,
    /// Microsoft SQL Server (TDS).
    MsSql,
    /// A connection saved by a newer version for an engine this build does not know.
    /// It is kept (never deleted) and shown as unsupported.
    #[serde(other)]
    Unknown,
}

impl DbKind {
    pub fn display_name(self) -> &'static str {
        match self {
            DbKind::Sqlite => "SQLite",
            DbKind::MySql => "MySQL",
            DbKind::Postgres => "PostgreSQL",
            DbKind::MsSql => "SQL Server",
            DbKind::Unknown => "Unsupported engine",
        }
    }
}

/// Where the database lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionParams {
    /// Server reachable over TCP (or a Unix socket when `socket` is set).
    Network {
        host: String,
        port: u16,
        user: String,
        #[serde(default)]
        database: Option<String>,
        #[serde(default)]
        socket: Option<String>,
    },
    /// Embedded database stored in a file (or `:memory:`).
    File { path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TlsMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub mode: TlsMode,
    #[serde(default)]
    pub ca_cert_path: Option<String>,
    #[serde(default)]
    pub client_cert_path: Option<String>,
    #[serde(default)]
    pub client_key_path: Option<String>,
    /// Override the name checked against the server certificate.
    #[serde(default)]
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SshAuth {
    /// Password is in the keychain (`SecretKind::SshPassword`).
    Password,
    /// Passphrase, if any, is in the keychain (`SecretKind::SshKeyPassphrase`).
    PrivateKey {
        key_path: String,
    },
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    #[serde(default)]
    pub keepalive_secs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeouts {
    #[serde(default = "default_connect_secs")]
    pub connect_secs: u32,
    /// Default per-statement timeout; `None` = unlimited. Drivers that cannot enforce it ignore
    /// it (see `Capabilities::statement_timeout`).
    #[serde(default)]
    pub statement_secs: Option<u32>,
}

fn default_connect_secs() -> u32 {
    10
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_secs: default_connect_secs(),
            statement_secs: None,
        }
    }
}

/// Per-connection rules for external agents (MCP). See `docs/mcp-security.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpAccess {
    /// `None` = decide by environment: exposed unless production-like.
    #[serde(default)]
    pub exposed: Option<bool>,
    /// Agents may request writes (each still needs GUI confirmation).
    #[serde(default)]
    pub allow_write: bool,
    #[serde(default)]
    pub max_rows: Option<u32>,
    #[serde(default)]
    pub statement_timeout_secs: Option<u32>,
}

/// A saved connection. Persisted as JSON (see `fg-store`); follows the rules in `compat`.
/// Passwords and key passphrases are **not** here: they live in the keychain
/// (see [`crate::SecretKind`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: ConnectionId,
    pub name: String,
    pub kind: DbKind,
    pub params: ConnectionParams,
    #[serde(default)]
    pub group_id: Option<GroupId>,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub color: Option<TagColor>,
    /// `None` = use the environment's default.
    #[serde(default)]
    pub safe_mode: Option<SafeMode>,
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub timeouts: Timeouts,
    /// Executed right after connecting (e.g. `SET search_path`, `SET NAMES`).
    #[serde(default)]
    pub startup_sql: Vec<String>,
    /// Namespace selected when the connection opens.
    #[serde(default)]
    pub default_namespace: Option<String>,
    #[serde(default)]
    pub mcp: McpAccess,
    /// Driver-specific knobs (e.g. `readonly=true`, `create=true` for SQLite). Unknown keys
    /// must be ignored by drivers.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default)]
    pub created_at: Option<UnixMillis>,
    #[serde(default)]
    pub updated_at: Option<UnixMillis>,
    #[serde(default)]
    pub last_used_at: Option<UnixMillis>,
}

impl ConnectionConfig {
    /// Minimal config; everything else takes its default.
    pub fn new(name: impl Into<String>, kind: DbKind, params: ConnectionParams) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            kind,
            params,
            group_id: None,
            environment: Environment::default(),
            color: None,
            safe_mode: None,
            ssh: None,
            tls: None,
            timeouts: Timeouts::default(),
            startup_sql: Vec::new(),
            default_namespace: None,
            mcp: McpAccess::default(),
            options: BTreeMap::new(),
            notes: None,
            sort_order: 0,
            created_at: None,
            updated_at: None,
            last_used_at: None,
        }
    }

    pub fn option_enabled(&self, key: &str) -> bool {
        self.options
            .get(key)
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
    }

    /// The safe mode in force: the explicit setting, else the environment's default.
    pub fn effective_safe_mode(&self) -> SafeMode {
        self.safe_mode
            .unwrap_or_else(|| self.environment.default_safe_mode())
            .effective()
    }

    /// Whether external agents may see this connection.
    pub fn mcp_exposed(&self) -> bool {
        self.mcp
            .exposed
            .unwrap_or_else(|| !self.environment.is_production_like())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionGroup {
    pub id: GroupId,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<GroupId>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default)]
    pub created_at: Option<UnixMillis>,
    #[serde(default)]
    pub updated_at: Option<UnixMillis>,
}

impl ConnectionGroup {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: GroupId::new(),
            name: name.into(),
            parent_id: None,
            sort_order: 0,
            created_at: None,
            updated_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_minimal_json_still_loads_with_defaults() {
        // What the very first version wrote: only the required fields.
        let json =
            r#"{"id":"c1","name":"x","kind":"sqlite","params":{"type":"file","path":"a.db"}}"#;
        let c: ConnectionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.environment, Environment::Development);
        assert_eq!(c.timeouts.connect_secs, 10);
        assert!(c.mcp_exposed());
    }

    #[test]
    fn fields_from_a_newer_version_are_ignored_not_fatal() {
        let json = r#"{"id":"c1","name":"x","kind":"clickhouse","params":{"type":"file","path":"a"},
                       "environment":"qa","shiny_new_field":{"a":1}}"#;
        let c: ConnectionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.kind, DbKind::Unknown);
        assert_eq!(c.environment, Environment::Unknown);
        assert_eq!(c.effective_safe_mode(), SafeMode::Alert);
        assert!(
            !c.mcp_exposed(),
            "unknown environment is hidden from agents by default"
        );
    }

    #[test]
    fn production_defaults_are_conservative() {
        let mut c = ConnectionConfig::new(
            "p",
            DbKind::Postgres,
            ConnectionParams::Network {
                host: "h".into(),
                port: 5432,
                user: "u".into(),
                database: None,
                socket: None,
            },
        );
        c.environment = Environment::Production;
        assert!(!c.mcp_exposed());
        assert_eq!(c.effective_safe_mode(), SafeMode::Alert);
        c.mcp.exposed = Some(true);
        assert!(c.mcp_exposed(), "explicit setting wins");
    }
}
