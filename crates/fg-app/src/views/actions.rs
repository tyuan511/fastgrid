//! Keyboard shortcuts. One list, bound once at startup, so they are easy to review against
//! `docs/shortcuts.md`. `secondary` is Cmd on macOS and Ctrl elsewhere.
//!
//! Each shortcut is scoped to a key context so the same keys can mean different things in
//! different places (a context deeper in the focus path wins).

use gpui_kit::{App, KeyBinding};

/// The connection window.
pub const WORKSPACE_CONTEXT: &str = "Workspace";
/// A table tab (data grid).
pub const GRID_CONTEXT: &str = "TableTab";
/// A query tab's result grid.
pub const RESULT_CONTEXT: &str = "QueryResult";
/// The cell editor that opens on a grid cell; deeper than the grid so `Esc` is its own.
pub const CELL_EDITOR_CONTEXT: &str = "CellEditor";
/// The connection list.
pub const LAUNCHER_CONTEXT: &str = "Launcher";
/// The query-history panel, which has its own keys while it is open.
pub const HISTORY_CONTEXT: &str = "HistoryPanel";
/// The snippets panel, which has its own keys while it is open.
pub const SNIPPETS_CONTEXT: &str = "SnippetsPanel";
/// The SQL editor. GPUI Kit pushes the context `Input mode=editor` on its editor element; the
/// binding grammar is `identifier && key == value`, so that context matches like this. Scoping run
/// and cancel to the editor keeps those keys usable in the grid and the sidebar.
pub const EDITOR_CONTEXT: &str = "Input && mode == editor";

gpui_kit::actions!(
    fastgrid,
    [
        QuickOpen,
        CloseTab,
        NextTab,
        PrevTab,
        ToggleSidebar,
        ToggleFilters,
        RefreshSchema,
        CopySelection,
        LauncherNext,
        LauncherPrev,
        LauncherOpen,
        NewConnection,
        NewQuery,
        ToggleHistory,
        SwitchDatabase,
        HistoryOpen,
        HistoryNext,
        HistoryPrev,
        HistoryDismiss,
        ToggleSnippets,
        SaveSnippet,
        SnippetOpen,
        SnippetDismiss,
        RunStatement,
        RunAll,
        CancelQuery,
        FormatSql,
        ToggleComment,
        ExplainQuery,
        CommitChanges,
        UndoEdit,
        RedoEdit,
        BeginEdit,
        CancelCellEdit,
        InsertRow,
        ToggleDeleteRow,
        CloneRow,
        SetCellNull,
        SetCellEmpty,
        SetCellDefault,
        RevertCell,
        PreviewChanges,
        ToggleRowDetail,
        SwitchToData,
        SwitchToStructure,
        ExportWizard,
        ImportWizard,
    ]
);

/// Register every shortcut. Call once after `gpui_kit::init`.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-p", QuickOpen, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-k", SwitchDatabase, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-w", CloseTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-b", ToggleSidebar, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-r", RefreshSchema, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-]", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-[", PrevTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-f", ToggleFilters, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-c", CopySelection, Some(GRID_CONTEXT)),
        // Staged edits (docs/shortcuts.md, "数据网格"). `⌘Z` inside the cell editor is the input's
        // own undo: the editor's context is deeper and wins.
        KeyBinding::new("secondary-s", CommitChanges, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-z", UndoEdit, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-shift-z", RedoEdit, Some(GRID_CONTEXT)),
        KeyBinding::new("enter", BeginEdit, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-i", InsertRow, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-d", CloneRow, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-backspace", ToggleDeleteRow, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-shift-p", PreviewChanges, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-ctrl-[", SwitchToStructure, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-ctrl-]", SwitchToData, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-shift-e", ExportWizard, Some(GRID_CONTEXT)),
        KeyBinding::new("secondary-shift-e", ExportWizard, Some(RESULT_CONTEXT)),
        // Import targets a table tab only; `secondary-i` is InsertRow, so
        // the shifted form is free on the grid context.
        KeyBinding::new("secondary-shift-i", ImportWizard, Some(GRID_CONTEXT)),
        // Space is not a type-to-edit key, so the grid can use it for the
        // row-detail inspector without stealing a typed character.
        KeyBinding::new("space", ToggleRowDetail, Some(GRID_CONTEXT)),
        // An editable result grid shares the browse grid's edit keys, under its own context so
        // they never fire while the SQL editor has focus.
        KeyBinding::new("enter", BeginEdit, Some(RESULT_CONTEXT)),
        KeyBinding::new("secondary-s", CommitChanges, Some(RESULT_CONTEXT)),
        KeyBinding::new("escape", CancelCellEdit, Some(RESULT_CONTEXT)),
        KeyBinding::new("escape", CancelCellEdit, Some(CELL_EDITOR_CONTEXT)),
        KeyBinding::new("down", LauncherNext, Some(LAUNCHER_CONTEXT)),
        KeyBinding::new("up", LauncherPrev, Some(LAUNCHER_CONTEXT)),
        KeyBinding::new("enter", LauncherOpen, Some(LAUNCHER_CONTEXT)),
        KeyBinding::new("secondary-n", NewConnection, Some(LAUNCHER_CONTEXT)),
        // A new query tab, reachable whether or not the editor has focus.
        KeyBinding::new("secondary-t", NewQuery, Some(WORKSPACE_CONTEXT)),
        // Query history is a global shortcut, but on Windows/Linux `Ctrl+Y`
        // is the text input's own Redo and its deeper context wins, so the
        // history key there is `Ctrl+Shift+Y` instead. On macOS `⌘Y` has no
        // such conflict.
        #[cfg(target_os = "macos")]
        KeyBinding::new("secondary-y", ToggleHistory, Some(WORKSPACE_CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-shift-y", ToggleHistory, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("enter", HistoryOpen, Some(HISTORY_CONTEXT)),
        KeyBinding::new("down", HistoryNext, Some(HISTORY_CONTEXT)),
        KeyBinding::new("up", HistoryPrev, Some(HISTORY_CONTEXT)),
        KeyBinding::new("escape", HistoryDismiss, Some(HISTORY_CONTEXT)),
        // Snippets: `⌘⇧S` opens the panel, `⌘⇧K` saves the current statement as one.
        KeyBinding::new("secondary-shift-s", ToggleSnippets, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-shift-k", SaveSnippet, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("enter", SnippetOpen, Some(SNIPPETS_CONTEXT)),
        KeyBinding::new("escape", SnippetDismiss, Some(SNIPPETS_CONTEXT)),
        // Scoped to the editor: deeper context than the library's own `secondary-enter` newline,
        // so it wins without touching the grid's keys.
        KeyBinding::new("secondary-enter", RunStatement, Some(EDITOR_CONTEXT)),
        KeyBinding::new("secondary-shift-enter", RunAll, Some(EDITOR_CONTEXT)),
        // `secondary-period` is the library's "code actions" inside an editor. fastgrid has no code
        // actions yet and needs a cancel key that does not fight Escape (search overlay, IME).
        KeyBinding::new("secondary-.", CancelQuery, Some(EDITOR_CONTEXT)),
        // `⌥⇧F` and `⌘/` are the two editor actions that never leave the buffer.
        KeyBinding::new("alt-shift-f", FormatSql, Some(EDITOR_CONTEXT)),
        KeyBinding::new("secondary-/", ToggleComment, Some(EDITOR_CONTEXT)),
        KeyBinding::new("secondary-shift-e", ExplainQuery, Some(EDITOR_CONTEXT)),
    ]);
}
