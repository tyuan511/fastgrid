//! GPUI views. They render the models in [`crate::model`] and talk to `fg-service`.

mod actions;
pub mod cells;
mod filter_drawer;
mod grid;
pub mod icons;
mod launcher;
mod query_tab;
mod style;
mod workspace;

pub use actions::{
    CancelQuery, CloseTab, CopySelection, ExplainQuery, FormatSql, HistoryDismiss, HistoryNext,
    HistoryOpen, HistoryPrev, LauncherNext, LauncherOpen, LauncherPrev, NewConnection, NewQuery,
    NextTab, PrevTab, QuickOpen, RefreshSchema, RunAll, RunStatement, SaveSnippet, SnippetDismiss,
    SnippetOpen, ToggleComment, ToggleFilters, ToggleHistory, ToggleSidebar, ToggleSnippets,
    bind_keys,
};
pub use filter_drawer::{FilterDrawer, FilterDrawerEvent};
pub use grid::{GridDelegate, TableTab, TableTabEvent};
pub use icons::{FastgridAssets, Hugeicon, hugeicon};
pub use launcher::{Launcher, LauncherEvent, Row, rows_from};
pub use query_tab::{
    CompletionCatalog, PendingConfirmation, QueryTab, QueryTabEvent, ResultDelegate,
};
pub use workspace::{
    TabContent, WorkspaceEvent, WorkspaceView, next_safe_mode, open_workspace_window,
    restore_query, safe_mode_label,
};
