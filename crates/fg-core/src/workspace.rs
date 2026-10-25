//! UI state that should survive restarts: open tabs, per-table view state, settings.
//! Stored as JSON, so every field is optional/defaulted (see `compat`).

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{DocumentId, Filter, ObjectPath, SortKey, TabId, compat::lenient_vec};

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TableViewMode {
    #[default]
    Data,
    Structure,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TabKind {
    Table {
        path: ObjectPath,
        #[serde(default)]
        mode: TableViewMode,
    },
    Query {
        document_id: DocumentId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabState {
    pub id: TabId,
    pub kind: TabKind,
    #[serde(default)]
    pub pinned: bool,
}

/// Layout and tabs of one connection's window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub tabs: Vec<TabState>,
    #[serde(default)]
    pub active_tab: Option<TabId>,
    #[serde(default)]
    pub active_namespace: Option<String>,
    #[serde(default = "yes")]
    pub sidebar_visible: bool,
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    #[serde(default)]
    pub inspector_open: bool,
    #[serde(default)]
    pub inspector_width: Option<f32>,
    /// Expanded tree nodes (opaque keys owned by the UI).
    #[serde(default)]
    pub expanded_nodes: Vec<String>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: None,
            active_namespace: None,
            sidebar_visible: true,
            sidebar_width: None,
            inspector_open: false,
            inspector_width: None,
            expanded_nodes: Vec::new(),
        }
    }
}

/// Remembered state of one table's grid (per connection + table).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TableViewState {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub raw_where: Option<String>,
    #[serde(default)]
    pub sort: Vec<SortKey>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub hidden_columns: Vec<String>,
    #[serde(default)]
    pub column_order: Vec<String>,
    #[serde(default)]
    pub column_widths: BTreeMap<String, f32>,
    /// Number of columns frozen at the left.
    #[serde(default)]
    pub pinned_columns: u32,
}

/// A group of settings stored under one key in the `setting` table.
pub trait SettingGroup: Default + Serialize + DeserializeOwned {
    const KEY: &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RowHeight {
    Compact,
    #[default]
    Standard,
    Comfortable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppearanceSettings {
    #[serde(default)]
    pub theme: ThemeMode,
    #[serde(default)]
    pub row_height: RowHeight,
    #[serde(default)]
    pub font_family: Option<String>,
    #[serde(default)]
    pub font_size: Option<f32>,
    /// BCP-47 tag; `None` = follow the system.
    #[serde(default)]
    pub language: Option<String>,
}

impl SettingGroup for AppearanceSettings {
    const KEY: &'static str = "appearance";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridSettings {
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Cells larger than this are loaded lazily (`Value::Truncated`).
    #[serde(default = "default_max_cell_bytes")]
    pub max_cell_bytes: u32,
    #[serde(default = "yes")]
    pub confirm_discard_changes: bool,
}

fn default_page_size() -> u32 {
    300
}

fn default_max_cell_bytes() -> u32 {
    64 * 1024
}

impl Default for GridSettings {
    fn default() -> Self {
        Self {
            page_size: default_page_size(),
            max_cell_bytes: default_max_cell_bytes(),
            confirm_discard_changes: true,
        }
    }
}

impl SettingGroup for GridSettings {
    const KEY: &'static str = "grid";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditorSettings {
    #[serde(default)]
    pub font_size: Option<f32>,
    #[serde(default = "default_tab_width")]
    pub tab_width: u8,
    #[serde(default)]
    pub format_before_run: bool,
}

fn default_tab_width() -> u8 {
    4
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            font_size: None,
            tab_width: default_tab_width(),
            format_before_run: false,
        }
    }
}

impl SettingGroup for EditorSettings {
    const KEY: &'static str = "editor";
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct McpSettings {
    /// Run the local HTTP endpoint while the GUI is open.
    #[serde(default)]
    pub http_enabled: bool,
    #[serde(default)]
    pub http_port: Option<u16>,
    /// Seconds a write request waits for a human before it is rejected.
    #[serde(default)]
    pub confirm_timeout_secs: Option<u32>,
}

impl SettingGroup for McpSettings {
    const KEY: &'static str = "mcp";
}

/// Whether the first-launch keychain-backup banner has been dismissed. It is a setting, not
/// session state, so a fresh install sees the guidance exactly once — until the user closes it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BackupNudge {
    #[serde(default)]
    pub dismissed: bool,
}

impl SettingGroup for BackupNudge {
    const KEY: &'static str = "backup_nudge";
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PrivacySettings {
    /// Off unless the user turns it on.
    #[serde(default)]
    pub telemetry: bool,
}

impl SettingGroup for PrivacySettings {
    const KEY: &'static str = "privacy";
}

fn default_exec_max_rows() -> Option<u64> {
    Some(100_000)
}

fn default_history_keep() -> usize {
    2000
}

/// Caps and retention for arbitrary SQL (`OpenConnection::run`), shared by the GUI, CLI and MCP.
///
/// The effective row cap and statement timeout are the stricter of this value, the connection's
/// `timeouts.statement_secs` (timeout only), and the per-call option. `None` means no limit from
/// that source; a `Some` always beats a `None`, and two `Some` values take the minimum. A caller
/// cannot raise a stored cap by passing a larger number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSettings {
    /// Stop a result set after this many rows and tell the caller it was truncated.
    #[serde(default = "default_exec_max_rows")]
    pub max_rows: Option<u64>,
    /// Abort a statement after this long, where the driver can enforce it.
    #[serde(default)]
    pub statement_timeout_secs: Option<u32>,
    /// Newest history entries kept per connection. Older ones are tombstoned after each run.
    #[serde(default = "default_history_keep")]
    pub history_keep: usize,
}

impl Default for ExecSettings {
    fn default() -> Self {
        Self {
            max_rows: default_exec_max_rows(),
            statement_timeout_secs: None,
            history_keep: default_history_keep(),
        }
    }
}

impl SettingGroup for ExecSettings {
    const KEY: &'static str = "exec";
}

/// Command id -> key chord overrides (see `docs/shortcuts.md`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeybindingOverrides(pub BTreeMap<String, String>);

impl SettingGroup for KeybindingOverrides {
    const KEY: &'static str = "keybindings";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_yields_defaults() {
        let g: GridSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(g, GridSettings::default());
        let exec: ExecSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(exec, ExecSettings::default());
        assert_eq!(exec.max_rows, Some(100_000));
        assert_eq!(exec.history_keep, 2000);
        let w: WorkspaceState = serde_json::from_str("{}").unwrap();
        assert!(w.sidebar_visible);
    }

    #[test]
    fn unreadable_tabs_are_dropped_not_fatal() {
        let json = r#"{"tabs":[
            {"id":"t1","kind":{"type":"query","document_id":"d1"}},
            {"id":"t2","kind":{"type":"er_diagram_from_2030"}}],"active_tab":"t1"}"#;
        let w: WorkspaceState = serde_json::from_str(json).unwrap();
        assert_eq!(w.tabs.len(), 1);
    }

    #[test]
    fn setting_keys_are_unique() {
        let keys = [
            AppearanceSettings::KEY,
            GridSettings::KEY,
            EditorSettings::KEY,
            McpSettings::KEY,
            PrivacySettings::KEY,
            ExecSettings::KEY,
            KeybindingOverrides::KEY,
        ];
        let mut sorted = keys.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len());
    }
}
