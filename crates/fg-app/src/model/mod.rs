//! UI logic that does not need a window.

pub mod changeset;
pub mod connection_draft;
pub mod filter_draft;
pub mod fk;
pub mod format;
pub mod fuzzy;
pub mod grid_query;
pub mod query_tab;
pub mod tabs;
pub mod tree;

pub use format::{
    Align, CellDisplay, CellKind, align_for, copy_text, display, group_digits, human_bytes, to_tsv,
};

pub use changeset::{CellState, Changeset, Counts, RowId, RowState};
pub use connection_draft::{
    ConnectionDraft, ENVIRONMENTS, SAFE_MODES, TAG_COLORS, color_from_label, color_label,
    duplicate, environment_from_label, environment_label, safe_mode_from_label, safe_mode_label,
};
pub use filter_draft::{
    Applied, DraftRow, FilterDraft, OPERATORS, RowError, op_from_label, op_label, parse_cell_text,
    value_slots,
};
pub use fk::{JumpTarget, jump_targets, tables_needing_primary_key};
pub use fuzzy::{FuzzyMatch, fuzzy_match, rank};
pub use grid_query::{
    DEFAULT_PAGE_SIZE, DEFAULT_RESULT_PAGE_SIZE, GridQuery, PAGE_SIZES, PageState,
    RESULT_PAGE_SIZES, SortState,
};
pub use query_tab::{
    ErrorSpan, QueryDocumentState, QueryResults, RefusalInfo, ResultSetState, RunFailure, RunMode,
    RunStatus, RunTarget, run_target,
};
pub use tabs::{TabSet, table_title};
pub use tree::{
    NodeKind, QuickItem, TreeNode, build_tree, compact_count, quick_items, quick_search,
};
