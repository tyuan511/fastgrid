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
    ]
);

/// Register every shortcut. Call once after `gpui_kit::init`.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-p", QuickOpen, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-w", CloseTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-b", ToggleSidebar, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-r", RefreshSchema, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-]", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-[", PrevTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-f", ToggleFilters, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("secondary-c", CopySelection, Some(GRID_CONTEXT)),
    ]);
}
