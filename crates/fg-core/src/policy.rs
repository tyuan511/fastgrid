use serde::{Deserialize, Serialize};

/// What kind of system this connection points at. Drives defaults (safe mode, MCP exposure,
/// header colour), never hard behaviour on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Environment {
    #[default]
    Development,
    Staging,
    Production,
    /// Written by a newer version.
    #[serde(other)]
    Unknown,
}

impl Environment {
    /// Unknown environments are treated like production: the cautious default.
    pub fn is_production_like(self) -> bool {
        matches!(self, Environment::Production | Environment::Unknown)
    }

    pub fn default_safe_mode(self) -> SafeMode {
        match self {
            Environment::Development => SafeMode::Normal,
            Environment::Staging => SafeMode::Alert,
            Environment::Production | Environment::Unknown => SafeMode::Alert,
        }
    }
}

/// Purely visual tag colour (the environment pill, connection list dot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TagColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
    #[serde(other)]
    Unknown,
}

/// Guard level for writes made from the UI (and, combined with `McpAccess`, from agents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SafeMode {
    /// Level 0: run everything.
    #[default]
    Normal,
    /// Level 1: confirm dangerous statements.
    Alert,
    /// Level 2: confirm every statement that may write.
    ///
    /// Password re-authentication before a write is part of this mode for
    /// editor runs confirmed through the GUI: when a password is stored for
    /// the connection, the confirmation prompt asks for it and the service
    /// verifies it before the run proceeds. Connections with no stored
    /// password keep a plain click confirmation, and grid staged commits
    /// and agent approvals have their own confirmation paths that do not
    /// ask for a password yet. This mode is strictly stronger than
    /// [`SafeMode::Alert`]: every statement whose class may write needs
    /// confirmation, not only dangerous ones. A read-only connection is
    /// still stricter, because confirmation cannot authorize a write.
    Strict,
    /// No writes at all.
    ReadOnly,
    /// Written by a newer version; treated as the most restrictive mode.
    #[serde(other)]
    Unknown,
}

impl SafeMode {
    /// Collapse `Unknown` to the most restrictive known mode.
    pub fn effective(self) -> SafeMode {
        match self {
            SafeMode::Unknown => SafeMode::ReadOnly,
            other => other,
        }
    }
}

/// Coarse classification of a SQL statement, computed by `fg-sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StatementClass {
    /// SELECT, SHOW, EXPLAIN (without ANALYZE of writes), VALUES...
    Read,
    /// INSERT / UPDATE / DELETE / MERGE / REPLACE.
    Write,
    /// CREATE / ALTER / DROP / TRUNCATE / RENAME.
    Ddl,
    /// BEGIN / COMMIT / ROLLBACK / SAVEPOINT.
    Transaction,
    /// SET, PRAGMA, USE, ANALYZE... session or maintenance statements.
    Utility,
    /// Could not be classified; callers must treat it as a write.
    #[serde(other)]
    Unknown,
}

impl StatementClass {
    pub fn may_write(self) -> bool {
        !matches!(self, StatementClass::Read | StatementClass::Transaction)
    }
}

/// Why a statement is considered dangerous (safe mode level 1 shows this to the user).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DangerReason {
    DropObject,
    Truncate,
    DeleteWithoutWhere,
    UpdateWithoutWhere,
    AlterDestructive,
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_from_newer_versions_degrade_safely() {
        let env: Environment = serde_json::from_str("\"disaster_recovery\"").unwrap();
        assert_eq!(env, Environment::Unknown);
        assert!(env.is_production_like());

        let mode: SafeMode = serde_json::from_str("\"paranoid\"").unwrap();
        assert_eq!(mode.effective(), SafeMode::ReadOnly);
    }

    #[test]
    fn unknown_statements_count_as_writes() {
        assert!(StatementClass::Unknown.may_write());
        assert!(!StatementClass::Read.may_write());
    }
}
