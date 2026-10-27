//! GPUI views. They render the models in [`crate::model`] and talk to `fg-service`.

mod actions;
pub mod cells;
mod filter_drawer;
mod grid;
pub mod icons;
mod info_grid;
mod launcher;
mod query_tab;
mod recovery;
mod style;
mod workspace;

pub use actions::{
    BeginEdit, CancelCellEdit, CancelQuery, CloseTab, CommitChanges, CopySelection, ExplainQuery,
    FormatSql, HistoryDismiss, HistoryNext, HistoryOpen, HistoryPrev, InsertRow, LauncherNext,
    LauncherOpen, LauncherPrev, NewConnection, NewQuery, NextTab, PrevTab, PreviewChanges,
    QuickOpen, RESULT_CONTEXT, RedoEdit, RefreshSchema, RunAll, RunStatement, SaveSnippet,
    SnippetDismiss, SnippetOpen, ToggleComment, ToggleDeleteRow, ToggleFilters, ToggleHistory,
    ToggleSidebar, ToggleSnippets, UndoEdit, bind_keys,
};
pub use cells::CellMark;
pub use filter_drawer::{FilterDrawer, FilterDrawerEvent};
pub use grid::{GridDelegate, PrefetchedPage, StructureData, TabMode, TableTab, TableTabEvent};
pub use icons::{FastgridAssets, Hugeicon, hugeicon};
pub use launcher::{Launcher, LauncherEvent, Row, SaveRequest, TestConnectionRequest, rows_from};
pub use query_tab::{
    CompletionCatalog, PendingConfirmation, QueryTab, QueryTabEvent, ResultDelegate,
};
pub use recovery::{Recovery, RecoveryEvent, move_store_aside};
pub use workspace::{
    ExportFinished, ExportSource, TabContent, WorkspaceEvent, WorkspaceView, next_safe_mode,
    open_workspace_window, restore_query, safe_mode_label,
};
