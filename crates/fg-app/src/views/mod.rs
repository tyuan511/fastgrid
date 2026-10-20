//! GPUI views. They render the models in [`crate::model`] and talk to `fg-service`.

mod actions;
mod filter_drawer;
mod grid;
mod launcher;
mod style;
mod workspace;

pub use actions::{
    CloseTab, CopySelection, NextTab, PrevTab, QuickOpen, RefreshSchema, ToggleFilters,
    ToggleSidebar, bind_keys,
};
pub use filter_drawer::{FilterDrawer, FilterDrawerEvent};
pub use grid::{GridDelegate, TableTab, TableTabEvent};
pub use launcher::{Launcher, LauncherEvent, Row, rows_from};
pub use workspace::{WorkspaceEvent, WorkspaceView, open_workspace_window, restore_query};
