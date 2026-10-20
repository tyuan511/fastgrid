//! UI logic that does not need a window.

pub mod filter_draft;
pub mod format;
pub mod fuzzy;
pub mod grid_query;
pub mod tabs;
pub mod tree;

pub use format::{
    Align, CellDisplay, CellKind, align_for, copy_text, display, group_digits, human_bytes, to_tsv,
};

pub use filter_draft::{
    Applied, DraftRow, FilterDraft, OPERATORS, RowError, op_from_label, op_label, value_slots,
};
pub use fuzzy::{FuzzyMatch, fuzzy_match, rank};
pub use grid_query::{DEFAULT_PAGE_SIZE, GridQuery, PAGE_SIZES, PageState, SortState};
pub use tabs::{TabSet, table_title};
pub use tree::{
    NodeKind, QuickItem, TreeNode, build_tree, compact_count, quick_items, quick_search,
};
