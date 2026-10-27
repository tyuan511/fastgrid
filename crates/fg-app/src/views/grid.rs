//! One table's data grid: the `DataTable` plus a status bar with paging.
//!
//! Like the launcher this view is dumb: it shows what it is given and emits
//! [`TableTabEvent::Reload`] when the user changes something that needs fresh data (page, sort,
//! page size). [`crate::controller`] answers by running the query and feeding results back with a
//! generation number, so a slow, outdated query can never overwrite a newer one.

use std::cell::RefCell;
use std::rc::Rc;

use std::collections::{HashMap, HashSet};

use fg_core::{
    ColumnInfo, ColumnMeta, DataType, DbKind, Filter, FilterOp, ForeignKeyInfo, IndexColumn,
    IndexInfo, ObjectPath, ReferentialAction, SafeMode, TableDetail, TableQuery, TableViewState,
    TypeCategory, Value, dialect_for,
};
use fg_driver::driver_for;
use fg_service::ApplySchemaOutcome;
use fg_sql::{
    CellChange, RowChange, SchemaChange, SchemaStatement, compile_changes, compile_schema_changes,
};
use gpui_kit::base::actions::{SelectNextColumn, SelectPrevColumn};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableSelection, TableState,
};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    BeginEdit, CELL_EDITOR_CONTEXT, CancelCellEdit, CloneRow, CommitChanges, CopySelection,
    GRID_CONTEXT, InsertRow, PreviewChanges, RedoEdit, RevertCell, SetCellDefault, SetCellEmpty,
    SetCellNull, SwitchToData, SwitchToStructure, ToggleDeleteRow, ToggleRowDetail, UndoEdit,
};
use super::cells::{CellMark, CellSelection};
use super::filter_drawer::{FilterDrawer, FilterDrawerEvent};
use super::icons::{Hugeicon, hugeicon};
use super::info_grid::InfoDelegate;
use crate::model::{
    Align, BackRef, CellKind, CellState, Changeset, Counts, DEFAULT_PAGE_SIZE, FilterDraft,
    GridQuery, JumpTarget, PAGE_SIZES, RowId, RowState, SortState, StructureChangeset, align_for,
    copy_text, display, parse_cell_text, to_tsv,
};

/// Screen columns before the data: the row-number gutter.
const GUTTER: usize = 1;
const GUTTER_WIDTH: f32 = 44.;

/// A click on a column header, handed from the table delegate to the owning view.
/// A header click waiting to be applied: the column, and whether Shift was held, which keeps
/// the existing keys and adds this one instead of replacing them.
type SortRequest = Rc<RefCell<Option<(String, bool)>>>;
/// A click on a foreign-key arrow: (row, column) of the cell.
type JumpRequest = Rc<RefCell<Option<(usize, usize)>>>;
/// A Tab / Shift+Tab pressed in the cell editor: the (columns, rows) to move after confirming.
type MoveRequest = Rc<RefCell<Option<(i32, i32)>>>;

/// The cell being edited in place: where it is, and the text box over it.
struct CellEditor {
    row_ix: usize,
    /// Column on screen (hidden columns are skipped), as the table reports it.
    screen_col: usize,
    input: Entity<InputState>,
}

pub struct GridDelegate {
    metas: Vec<ColumnMeta>,
    /// Columns the user hid, by name. Hidden columns are still fetched, just not shown.
    hidden: HashSet<String>,
    /// Indices into `metas` / each row of the columns being shown, in order.
    visible: Vec<usize>,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    sort: SortState,
    pending_sort: SortRequest,
    /// Columns that can be followed to another table, by column name.
    jumps: HashMap<String, JumpTarget>,
    pending_jump: JumpRequest,
    pending_move: MoveRequest,
    loading: bool,
    /// The selected (row, column) on screen, mirrored from the table so cells can paint it.
    selected: Option<(usize, usize)>,
    /// Primary-key column names, in key order. Empty = the table cannot be edited.
    key_columns: Vec<String>,
    /// Staged edits; `None` until the columns are known.
    changeset: Option<Changeset>,
    editing: Option<CellEditor>,
}

impl GridDelegate {
    fn new(
        pending_sort: SortRequest,
        pending_jump: JumpRequest,
        pending_move: MoveRequest,
    ) -> Self {
        Self {
            metas: Vec::new(),
            hidden: HashSet::new(),
            visible: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            sort: SortState::default(),
            pending_sort,
            jumps: HashMap::new(),
            pending_jump,
            pending_move,
            loading: false,
            selected: None,
            key_columns: Vec::new(),
            changeset: None,
            editing: None,
        }
    }

    /// Keep the staged edits while the same table is reloaded (paging, sorting, filtering) and
    /// start fresh when the columns or the key changed underneath them.
    fn rebuild_changeset(&mut self) {
        let names: Vec<String> = self.metas.iter().map(|m| m.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        let same = self.changeset.as_ref().is_some_and(|c| {
            c.columns() == names.as_slice()
                && c.key_column_indices()
                    .iter()
                    .map(|&i| names[i].as_str())
                    .eq(self.key_columns.iter().map(String::as_str))
        });
        if !same {
            let keys: Vec<&str> = self.key_columns.iter().map(String::as_str).collect();
            self.changeset = Some(Changeset::new(names, &keys));
        }
    }

    fn new_row_ids(&self) -> Vec<u64> {
        self.changeset
            .as_ref()
            .map(Changeset::new_row_ids)
            .unwrap_or_default()
    }

    /// Loaded rows plus the rows added in the changeset, which show after them.
    fn total_rows(&self) -> usize {
        self.rows.len() + self.new_row_ids().len()
    }

    /// The on-screen row for one staged change, if it is loaded here. An
    /// update or delete is found by its primary-key values; an insert is the
    /// loaded rows plus its position among the inserts. A row staged from
    /// another page is not on screen, so this returns `None` and the
    /// selection stays where it was.
    fn failed_change_row(
        &self,
        change: &RowChange,
        changes: &[RowChange],
        number: usize,
    ) -> Option<usize> {
        match change {
            RowChange::Update { key, .. } | RowChange::Delete { key, .. } => {
                let wanted: Vec<Value> = key.iter().map(|(_, v)| v.clone()).collect();
                let indices = self.changeset.as_ref()?.key_column_indices().to_vec();
                self.rows.iter().position(|row| {
                    indices
                        .iter()
                        .map(|&i| row.get(i).cloned())
                        .collect::<Option<Vec<_>>>()
                        == Some(wanted.clone())
                })
            }
            RowChange::Insert { .. } => {
                let ordinal = changes[..number.saturating_sub(1)]
                    .iter()
                    .filter(|c| matches!(c, RowChange::Insert { .. }))
                    .count();
                Some(self.rows.len() + ordinal)
            }
        }
    }

    fn row_id_at(&self, row_ix: usize) -> Option<RowId> {
        let cs = self.changeset.as_ref()?;
        match self.rows.get(row_ix) {
            Some(row) => cs.row_id(row),
            None => cs
                .new_row_ids()
                .get(row_ix - self.rows.len())
                .map(|&n| RowId::New(n)),
        }
    }

    /// What the cell shows: the staged value if there is one, else what was loaded.
    fn shown_value(&self, row_ix: usize, data_col: usize) -> Value {
        let staged = self.row_id_at(row_ix).and_then(|id| {
            match self.changeset.as_ref()?.cell_state(&id, data_col) {
                CellState::Modified(CellChange::Set(v)) => Some(v),
                CellState::Modified(CellChange::Default) => Some(default_marker()),
                CellState::Unchanged => None,
            }
        });
        if let Some(v) = staged {
            return v;
        }
        match self.rows.get(row_ix) {
            Some(row) => row.get(data_col).cloned().unwrap_or(Value::Null),
            // A new row's untouched cell takes its column default.
            None => default_marker(),
        }
    }

    fn mark_for(&self, row_ix: usize, data_col: usize) -> CellMark {
        let (Some(cs), Some(id)) = (self.changeset.as_ref(), self.row_id_at(row_ix)) else {
            return CellMark::None;
        };
        match cs.row_state(&id) {
            RowState::Deleted => CellMark::Deleted,
            RowState::Inserted => CellMark::Inserted,
            _ if cs.cell_state(&id, data_col) != CellState::Unchanged => CellMark::Modified,
            _ => CellMark::None,
        }
    }

    /// The data column behind the `ix`-th column on screen; `None` for the row-number gutter.
    fn data_index(&self, ix: usize) -> Option<usize> {
        self.visible.get(ix.checked_sub(GUTTER)?).copied()
    }

    fn rebuild_columns(&mut self) {
        self.visible = (0..self.metas.len())
            .filter(|&i| !self.hidden.contains(&self.metas[i].name))
            .collect();
        // The row numbers stay put while the data scrolls sideways, as in TablePlus.
        let gutter = Column::new("#", "")
            .width(px(GUTTER_WIDTH))
            .p_0()
            .fixed_left()
            .resizable(false)
            .movable(false);
        let data = self.visible.iter().map(|&i| &self.metas[i]).map(|m| {
            let mut col = Column::new(m.name.clone(), m.name.clone())
                .width(px(default_width(m)))
                .p_0();
            if align_for(m.data_type.category) == Align::Right {
                col.align = TextAlign::Right;
            }
            // No library sort state: `render_th` draws the arrow itself and sorts on a click, so
            // the header keeps no space reserved for an arrow the column does not show.
            col
        });
        self.columns = std::iter::once(gutter).chain(data).collect();
    }
}

/// The 1-based change number in a commit error like `change 3: …`, if any.
fn parse_change_number(message: &str) -> Option<usize> {
    let rest = message.strip_prefix("change ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with(':') {
        return None;
    }
    digits.parse().ok()
}

/// What an unset cell of a new row shows.
fn default_marker() -> Value {
    Value::Other {
        type_name: String::new(),
        text: "DEFAULT".into(),
    }
}

/// A starting width per type; the user can resize.
fn default_width(m: &ColumnMeta) -> f32 {
    use fg_core::TypeCategory::*;
    let by_type: f32 = match m.data_type.category {
        Bool => 70.0,
        Integer => 90.0,
        Float | Decimal => 110.0,
        Date => 110.0,
        Time => 100.0,
        Timestamp | TimestampTz => 170.0,
        Uuid => 280.0,
        Json => 200.0,
        _ => 160.0,
    };
    // Wide enough for the column name plus the type badge.
    by_type
        .max(m.name.chars().count() as f32 * 8.0 + 60.0)
        .min(360.0)
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.total_rows()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        _sort: ColumnSort,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        // Our own `SortState` decides the cycle (asc, desc, off) and supports several keys;
        // the table only tells us which header was clicked.
        if let Some(meta) = self.data_index(col_ix).and_then(|i| self.metas.get(i)) {
            *self.pending_sort.borrow_mut() = Some((meta.name.clone(), window.modifiers().shift));
            cx.notify();
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(meta) = self.data_index(col_ix).and_then(|i| self.metas.get(i)) else {
            return div().into_any_element();
        };
        let name = meta.name.clone();
        let multiple_keys = self.sort.keys().len() > 1;
        let indicator = self.sort.indicator(&meta.name);
        let priority = indicator.filter(|_| multiple_keys).map(|(p, _)| p);
        // Only a sorted column shows an arrow, as in TablePlus.
        let arrow = indicator.map(|(_, descending)| if descending { "▼" } else { "▲" });
        let header = h_flex()
            .gap_1p5()
            .items_center()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(meta.name.clone()),
            )
            .when(self.jumps.contains_key(&meta.name), |d| {
                d.child(div().text_size(px(13.)).text_color(theme.info).child("↗"))
            })
            .when_some(arrow, |d, a| {
                d.child(
                    div()
                        .text_size(px(9.))
                        .text_color(theme.muted_foreground)
                        .child(a),
                )
            })
            .when_some(priority, |d, p| {
                d.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.muted_foreground)
                        .child(p.to_string()),
                )
            })
            .into_any_element();
        // A click on the header sorts by it, as in TablePlus, instead of selecting the column.
        div()
            .id(SharedString::from(format!("sort-{name}")))
            .test_support()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .cursor_pointer()
            .child(super::cells::th_frame(header, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                *this.delegate().pending_sort.borrow_mut() =
                    Some((name.clone(), window.modifiers().shift));
                cx.notify();
            }))
            .into_any_element()
    }

    fn fit_column(
        &mut self,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Option<Pixels> {
        // The table adds the column padding, which this grid sets to zero, so the width returned
        // here is the full inner width: the drawn text plus the frame's own padding, plus the
        // extras of the chip and foreign-key arrow where the cell draws them. The header counts
        // the same way: its name and badges plus the frame padding, the negative margin exactly
        // cancelling the sort-arrow space the table reserves. Called from inside the table's own
        // update, so this only measures; it never touches the table entity.
        let data_col = self.data_index(col_ix)?;
        let meta = self.metas.get(data_col)?;
        let theme = cx.theme().clone();
        let mono = Font {
            family: theme.mono_font_family.clone(),
            ..Default::default()
        };
        let mut widest = px(0.);
        for row_ix in 0..self.total_rows() {
            let value = self.shown_value(row_ix, data_col);
            let shown = display(&value);
            let mut width =
                super::cells::text_width(window, &shown.text, super::cells::GRID_TEXT_SIZE, &mono)
                    + super::cells::FRAME_PADDING;
            if shown.kind == CellKind::Pill {
                width += px(8.); // the chip's own px_1 on each side
            }
            let followable = !value.is_null()
                && !matches!(self.mark_for(row_ix, data_col), CellMark::Deleted)
                && self.jumps.contains_key(&meta.name);
            if followable {
                // gap_1, then the arrow's own px_1 padding and glyph.
                width += px(4.)
                    + px(8.)
                    + super::cells::text_width(window, "↗", super::cells::GRID_TEXT_SIZE, &mono);
            }
            widest = widest.max(width);
        }
        let multiple_keys = self.sort.keys().len() > 1;
        let indicator = self.sort.indicator(&meta.name);
        let mut header = super::cells::text_width(
            window,
            &meta.name,
            super::cells::GRID_TEXT_SIZE,
            &Font {
                family: theme.mono_font_family.clone(),
                weight: FontWeight::MEDIUM,
                ..Default::default()
            },
        );
        let mut children = 1;
        if self.jumps.contains_key(&meta.name) {
            header += super::cells::text_width(window, "↗", super::cells::GRID_TEXT_SIZE, &mono);
            children += 1;
        }
        if let Some((_, descending)) = indicator {
            header +=
                super::cells::text_width(window, if descending { "▼" } else { "▲" }, px(9.), &mono);
            children += 1;
        }
        if multiple_keys && let Some((priority, _)) = indicator {
            header += super::cells::text_width(
                window,
                &priority.to_string(),
                super::cells::GRID_TEXT_SIZE,
                &mono,
            );
            children += 1;
        }
        header += px(6. * (children - 1) as f32); // gap_1p5 between header children
        header += super::cells::FRAME_PADDING;
        Some(widest.max(header))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        if col_ix < GUTTER {
            let selected = self.selected.is_some_and(|(r, _)| r == row_ix);
            // A 3px status bar at the left of the gutter slot, coloured by the
            // row's staged state so a staged row reads at a glance even when
            // its cells are scrolled out of view.
            let row_mark = match (self.changeset.as_ref(), self.row_id_at(row_ix)) {
                (Some(cs), Some(id)) => match cs.row_state(&id) {
                    RowState::Deleted => CellMark::Deleted,
                    RowState::Inserted => CellMark::Inserted,
                    RowState::Modified => CellMark::Modified,
                    RowState::Unchanged => CellMark::None,
                },
                _ => CellMark::None,
            };
            let bar_color = match row_mark {
                CellMark::Modified => Some(theme.warning),
                CellMark::Inserted => Some(theme.success),
                CellMark::Deleted => Some(theme.danger),
                CellMark::None => None,
            };
            return div()
                .id(SharedString::from(format!("row-number-{row_ix}")))
                .test_support()
                .size_full()
                .flex()
                .items_center()
                .font_family(theme.mono_font_family.clone())
                .text_size(super::cells::GRID_TEXT_SIZE)
                .when(selected, |d| d.bg(theme.primary.opacity(0.12)))
                .text_color(if selected {
                    theme.foreground
                } else {
                    theme.muted_foreground.opacity(0.7)
                })
                .when_some(bar_color, |d, color| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("row-status-{row_ix}")))
                            .test_support()
                            .w(px(3.))
                            .h_full()
                            .bg(color),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child((row_ix + 1).to_string()),
                )
                .into_any_element();
        }
        // `col_ix` counts the columns on screen; the data may have hidden ones in between.
        let screen_col = col_ix;
        let col_ix = self.data_index(col_ix).unwrap_or(usize::MAX);
        let value = self.shown_value(row_ix, col_ix);
        let mark = self.mark_for(row_ix, col_ix);
        let selection = match self.selected {
            Some((r, c)) if r == row_ix && c == screen_col => CellSelection::Cell,
            Some((r, _)) if r == row_ix => CellSelection::Row,
            _ => CellSelection::None,
        };
        if let Some(editor) = self
            .editing
            .as_ref()
            .filter(|e| e.row_ix == row_ix && e.screen_col == screen_col)
        {
            // The text box replaces the cell's content; `Esc` is bound on this context.
            let input = div()
                .key_context(CELL_EDITOR_CONTEXT)
                .id("cell-editor")
                .test_support()
                .size_full()
                .rounded_sm()
                .bg(theme.background)
                // Tab / Shift+Tab confirm and move sideways. The table binds them to its own
                // "select next / previous column"; taking those actions here, on the box, keeps the
                // table from moving the selection out from under a pending edit.
                .on_action(cx.listener(|this, _: &SelectNextColumn, _, cx| {
                    *this.delegate().pending_move.borrow_mut() = Some((1, 0));
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &SelectPrevColumn, _, cx| {
                    *this.delegate().pending_move.borrow_mut() = Some((-1, 0));
                    cx.notify();
                }))
                .child(
                    Input::new(&editor.input)
                        .id("cell-editor-input")
                        .appearance(false)
                        .bordered(false)
                        .h_full(),
                )
                .into_any_element();
            return super::cells::td_frame_marked(input, selection, mark, cx);
        }
        let followable = !value.is_null()
            && !matches!(mark, CellMark::Deleted)
            && self
                .metas
                .get(col_ix)
                .is_some_and(|m| self.jumps.contains_key(&m.name));
        let element = super::cells::cell_element(&value, cx);
        if !followable {
            return super::cells::td_frame_marked(element, selection, mark, cx);
        }
        // A small arrow at the right edge opens the referenced row.
        let linked = h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .justify_between()
            .child(div().min_w_0().overflow_hidden().child(element))
            .child(
                div()
                    .id(SharedString::from(format!("fk-{row_ix}-{col_ix}")))
                    .test_support()
                    .aria_label("Open the referenced row")
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme.info)
                    .hover(|d| d.bg(theme.muted))
                    .child("↗")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        *this.delegate().pending_jump.borrow_mut() = Some((row_ix, col_ix));
                        cx.notify();
                    })),
            )
            .into_any_element();
        super::cells::td_frame_marked(linked, selection, mark, cx)
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let col_ix = self.data_index(col_ix).unwrap_or(usize::MAX);
        if row_ix >= self.total_rows() || col_ix >= self.metas.len() {
            return String::new();
        }
        copy_text(&self.shown_value(row_ix, col_ix))
    }
}

/// What the grid asks its owner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum TableTabEvent {
    /// The query changed (page, sort, page size...): fetch again.
    Reload,
    /// The user clicked a foreign-key arrow: show `table` filtered to `filter`.
    FollowForeignKey { table: ObjectPath, filter: Filter },
    /// The user clicked a "referenced by" entry on a primary-key cell: show
    /// `table` filtered to every column pair of the referencing key.
    FollowBackReference {
        table: ObjectPath,
        filters: Vec<Filter>,
    },
    /// Something that is remembered per table but needs no new data changed (hidden columns).
    ViewChanged,
    /// The user asked to commit the staged changes (`⌘S`). The owner runs them and answers with
    /// [`TableTab::commit_finished`].
    CommitRequested,
    /// The user switched to the structure view and it has no data yet. The
    /// owner loads `describe` + `ddl` and answers with
    /// [`TableTab::set_structure`].
    StructureRequested,
    /// The user asked to apply the staged structure changes. The owner runs
    /// them and answers with [`TableTab::structure_commit_finished`].
    StructureCommitRequested,
}

/// Which half of a table tab is on screen. Data is the grid; Structure is
/// the read-only-then-editable definition (columns, indexes, foreign keys,
/// DDL text). Both halves share the tab, its title, and its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabMode {
    Data,
    Structure,
    Index,
}

/// The structure view's data: the table definition plus its DDL text, when
/// the engine provides one.
#[derive(Debug, Clone)]
pub struct StructureData {
    pub detail: std::sync::Arc<TableDetail>,
    pub ddl: Option<String>,
}

/// One page fetched ahead of time, waiting for the user to step onto it. The key is the query
/// without its offset, so a sort, filter or page-size change invalidates it instead of serving
/// stale rows.
#[derive(Debug, Clone)]
pub struct PrefetchedPage {
    pub key: TableQuery,
    pub page: u64,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
}

/// A selected row's full field details for the inspector panel.
#[derive(Debug, Clone)]
pub struct SelectedRowData {
    pub row_ix: usize,
    pub fields: Vec<FieldDetail>,
}

/// One field's metadata and formatted text in the selected row.
#[derive(Debug, Clone)]
pub struct FieldDetail {
    pub name: String,
    pub data_type: String,
    pub value_text: String,
    pub is_null: bool,
}

pub struct TableTab {
    table: Entity<TableState<GridDelegate>>,
    drawer: Entity<FilterDrawer>,
    drawer_open: bool,
    columns_open: bool,
    /// The paging panel behind the ⚙ button: first / last page and the page size.
    paging_open: bool,
    /// Set once the saved view state was applied; before that a jump must wait for it.
    restored: bool,
    /// A foreign-key jump that arrived before the saved filters were restored.
    /// A back-reference jump carries one filter per key column.
    pending_filter: Option<Vec<Filter>>,
    query: GridQuery,
    loading: bool,
    error: Option<SharedString>,
    elapsed_ms: Option<u64>,
    loaded_rows: usize,
    generation: u64,
    /// A commit is running; a second `⌘S` is ignored until it answers.
    committing: bool,
    /// The engine, to write the changes in its dialect for the SQL preview.
    engine: Option<DbKind>,
    preview_open: bool,
    /// The Discard button was pressed and is waiting for a second click.
    confirm_discard: bool,
    /// Values the open cell editor offers as a list: an enum column's
    /// variants, or, for a foreign-key column, the distinct values on the
    /// loaded page (typing anything else still works). Empty = no list.
    editor_options: Vec<String>,
    /// Whether `editor_options` are the column's allowed values or page values.
    editor_options_are_allowed: bool,
    /// The row-detail inspector (`Space`): the selected cell's full value,
    /// editable, with Text and JSON modes. `detail_cell` is the cell the
    /// input last synced to; a mismatch makes the next render resync it,
    /// which is how grid edits flow back into the inspector.
    detail_open: bool,
    detail_json: bool,
    detail_input: Option<Entity<InputState>>,
    detail_error: Option<String>,
    detail_cell: Option<(usize, usize)>,
    _detail_subscription: Option<Subscription>,
    /// The cell a right-click opened the action menu for, as (row, screen column). The menu is
    /// a panel, not the library's floating menu: that one is row-based, while these actions need
    /// the exact cell.
    cell_menu: Option<(usize, usize)>,
    /// The Save button was pressed under Alert/Strict and is waiting for
    /// the batch confirmation below.
    confirm_commit: bool,
    /// The connection's guard level, as the workspace last told this tab.
    safe_mode: SafeMode,
    /// The page after the one on screen, fetched ahead by the controller. Served only when the
    /// tab is still on the page before it under the same query; anything else is dropped.
    prefetched: Option<PrefetchedPage>,
    /// Data / Structure switch (roadmap M6). Structure data loads lazily
    /// on the first switch and is cleared after a structure commit, so the
    /// next switch reloads it.
    mode: TabMode,
    structure: Option<StructureData>,
    /// The structure and index views' lists, in the same grid widget as the data.
    col_grid: Entity<TableState<InfoDelegate>>,
    idx_grid: Entity<TableState<InfoDelegate>>,
    fk_grid: Entity<TableState<InfoDelegate>>,
    structure_loading: bool,
    structure_error: Option<SharedString>,
    /// Staged structure changes (roadmap M6): the same stage → preview →
    /// confirm → apply rhythm as the row overlay, with its own op list.
    structure_changes: StructureChangeset,
    structure_preview_open: bool,
    structure_preview: Option<Result<Vec<SchemaStatement>, String>>,
    structure_committing: bool,
    /// The Save button was pressed and the confirm bar (with warnings) is
    /// waiting for the second click.
    structure_confirm: bool,
    /// A form validation error, shown under the structure forms.
    structure_form_error: Option<String>,
    /// The column whose row is currently an edit form, if any.
    edit_column: Option<String>,
    /// The structure forms' text inputs, created on the first windowed
    /// switch to Structure (inputs need a window). `None` in tests that
    /// drive the tab without one: the forms simply do not render.
    structure_forms: Option<StructureForms>,
    /// Tables referencing this one (roadmap M6 back-references), set by the
    /// controller at attach time.
    back_references: Vec<BackRef>,
    _edit_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// Text inputs for the structure forms, plus their checkbox states.
struct StructureForms {
    add_col_name: Entity<InputState>,
    add_col_type: Entity<InputState>,
    add_col_default: Entity<InputState>,
    add_col_nullable: bool,
    edit_col_type: Entity<InputState>,
    edit_col_default: Entity<InputState>,
    edit_col_nullable: bool,
    add_idx_name: Entity<InputState>,
    add_idx_columns: Entity<InputState>,
    add_idx_unique: bool,
    add_fk_name: Entity<InputState>,
    add_fk_columns: Entity<InputState>,
    add_fk_ref_table: Entity<InputState>,
    add_fk_ref_columns: Entity<InputState>,
    add_fk_on_delete: Entity<InputState>,
    add_fk_on_update: Entity<InputState>,
    rename_table_name: Entity<InputState>,
}

impl EventEmitter<TableTabEvent> for TableTab {}

impl TableTab {
    pub fn new(path: ObjectPath, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_query(GridQuery::new(path, DEFAULT_PAGE_SIZE), window, cx)
    }

    pub fn with_query(query: GridQuery, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pending: SortRequest = Rc::new(RefCell::new(None));
        let pending_jump: JumpRequest = Rc::new(RefCell::new(None));
        let pending_move: MoveRequest = Rc::new(RefCell::new(None));
        let delegate = GridDelegate::new(
            Rc::clone(&pending),
            Rc::clone(&pending_jump),
            Rc::clone(&pending_move),
        );
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .row_header(false)
        });
        let info_grid = |headers: &[(&'static str, f32)],
                         empty: &'static str,
                         window: &mut Window,
                         cx: &mut Context<Self>| {
            let delegate = InfoDelegate::new(headers, empty);
            cx.new(|cx| {
                TableState::new(delegate, window, cx)
                    .cell_selectable(true)
                    .row_header(false)
            })
        };
        let col_grid = info_grid(
            &[
                ("Column", 180.),
                ("Type", 140.),
                ("Nullable", 80.),
                ("Key", 70.),
                ("Default", 160.),
                ("Extra", 140.),
            ],
            "No columns.",
            window,
            cx,
        );
        let idx_grid = info_grid(
            &[
                ("Index", 220.),
                ("Columns", 280.),
                ("Unique", 80.),
                ("Primary", 80.),
            ],
            "No indexes.",
            window,
            cx,
        );
        let fk_grid = info_grid(
            &[
                ("Name", 180.),
                ("Columns", 160.),
                ("References", 160.),
                ("Referenced columns", 180.),
                ("On delete", 110.),
                ("On update", 110.),
            ],
            "No foreign keys.",
            window,
            cx,
        );
        let drawer = cx.new(|cx| FilterDrawer::new(window, cx));
        let on_apply = cx.subscribe(&drawer, |this, _, event: &FilterDrawerEvent, cx| {
            let FilterDrawerEvent::Apply(applied) = event;
            this.query
                .set_filters(applied.filters.clone(), applied.raw_where.clone());
            cx.emit(TableTabEvent::Reload);
            cx.notify();
        });
        let observer = cx.observe_in(&table, window, move |this, table, window, cx| {
            // A click on a row number selects the whole row.
            if let TableSelection::Cell(row, col) = table.read(cx).selection()
                && col < GUTTER
            {
                table.update(cx, |t, cx| t.set_selected_row(row, cx));
                return;
            }
            let selected = match table.read(cx).selection() {
                TableSelection::Cell(r, c) => Some((r, c)),
                _ => None,
            };
            if table.read(cx).delegate().selected != selected {
                table.update(cx, |t, cx| {
                    t.delegate_mut().selected = selected;
                    cx.notify();
                });
            }
            let clicked = pending.borrow_mut().take();
            if let Some((column, additive)) = clicked {
                this.toggle_sort(&column, additive, cx);
            }
            // Selecting another cell (a click, an arrow key) while a box is open keeps what was
            // typed, the way clicking away from a spreadsheet cell does.
            let moved_away = this
                .table
                .read(cx)
                .delegate()
                .editing
                .as_ref()
                .is_some_and(|e| selected != Some((e.row_ix, e.screen_col)));
            if moved_away {
                this.finish_on_blur(window, cx);
            }
            let step = pending_move.borrow_mut().take();
            if let Some((dx, dy)) = step {
                this.commit_and_move(dx, dy, window, cx);
            }
            let jump = pending_jump.borrow_mut().take();
            if let Some((row, col)) = jump {
                this.follow_foreign_key(row, col, cx);
            }
            cx.notify();
        });
        // A double click opens the cell for editing, like `Enter` does.
        let on_table = cx.subscribe_in(
            &table,
            window,
            |this, _, event: &TableEvent, window, cx| match event {
                TableEvent::DoubleClickedCell(row, col) => {
                    this.begin_edit_at(*row, *col, None, window, cx);
                }
                TableEvent::RightClickedCell(row, col) => {
                    this.open_cell_menu(*row, *col, cx);
                }
                _ => {}
            },
        );
        let initial = FilterDraft::from_applied(&query.filters, query.raw_where.as_deref());
        drawer.update(cx, |d, cx| d.load(initial, cx));
        Self {
            table,
            drawer,
            drawer_open: false,
            columns_open: false,
            paging_open: false,
            restored: false,
            pending_filter: None,
            query,
            loading: false,
            error: None,
            elapsed_ms: None,
            loaded_rows: 0,
            generation: 0,
            committing: false,
            engine: None,
            preview_open: false,
            confirm_discard: false,
            editor_options: Vec::new(),
            editor_options_are_allowed: false,
            detail_open: false,
            detail_json: false,
            detail_input: None,
            detail_error: None,
            detail_cell: None,
            _detail_subscription: None,
            cell_menu: None,
            confirm_commit: false,
            safe_mode: SafeMode::default(),
            prefetched: None,
            mode: TabMode::Data,
            structure: None,
            col_grid,
            idx_grid,
            fk_grid,
            structure_loading: false,
            structure_error: None,
            structure_changes: StructureChangeset::new(),
            structure_preview_open: false,
            structure_preview: None,
            structure_committing: false,
            structure_confirm: false,
            structure_form_error: None,
            edit_column: None,
            structure_forms: None,
            back_references: Vec::new(),
            _edit_subscription: None,
            _subscriptions: vec![observer, on_apply, on_table],
        }
    }

    /// Replace the query (restoring what was saved for this table) before the first load.
    pub fn restore_query(&mut self, query: GridQuery, cx: &mut Context<Self>) {
        let mut query = query;
        self.restored = true;
        // A foreign-key jump that arrived first wins over whatever filters were saved.
        if let Some(filters) = self.pending_filter.take() {
            query.set_filters(filters, None);
        }
        let draft = FilterDraft::from_applied(&query.filters, query.raw_where.as_deref());
        self.drawer.update(cx, |d, cx| d.load(draft, cx));
        self.query = query;
        self.sync_sort(cx);
        cx.notify();
    }

    /// The half of the tab on screen.
    pub fn mode(&self) -> TabMode {
        self.mode
    }

    /// Switch halves. Switching to Structure with no data yet asks the
    /// owner to load it; switching back needs nothing.
    pub fn set_mode(&mut self, mode: TabMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        if mode != TabMode::Data && self.structure.is_none() && !self.structure_loading {
            self.structure_loading = true;
            self.structure_error = None;
            cx.emit(TableTabEvent::StructureRequested);
        }
        cx.notify();
    }

    /// Fill the three lists from the loaded definition.
    fn sync_info_grids(&mut self, cx: &mut Context<Self>) {
        let Some(data) = &self.structure else {
            return;
        };
        let detail = &data.detail;
        let yes = |b: bool| if b { "YES" } else { "NO" }.to_owned();
        let cols = detail
            .columns
            .iter()
            .map(|c| {
                vec![
                    c.name.clone(),
                    c.data_type.native.clone(),
                    yes(c.nullable),
                    c.primary_key_position
                        .map(|p| format!("PK({p})"))
                        .unwrap_or_default(),
                    c.default.clone().unwrap_or_default(),
                    if c.auto_increment {
                        "AUTO_INCREMENT".to_owned()
                    } else {
                        String::new()
                    },
                ]
            })
            .collect();
        let idx = detail
            .indexes
            .iter()
            .map(|ix| {
                vec![
                    ix.name.clone(),
                    ix.columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    yes(ix.unique),
                    yes(ix.primary),
                ]
            })
            .collect();
        let fks = detail
            .foreign_keys
            .iter()
            .map(|fk| {
                vec![
                    fk.name.clone().unwrap_or_default(),
                    fk.columns.join(", "),
                    fk.referenced.name.clone(),
                    fk.referenced_columns.join(", "),
                    fk.on_delete
                        .as_ref()
                        .map(|a| a.sql().to_owned())
                        .unwrap_or_default(),
                    fk.on_update
                        .as_ref()
                        .map(|a| a.sql().to_owned())
                        .unwrap_or_default(),
                ]
            })
            .collect();
        self.col_grid.update(cx, |t, cx| {
            t.delegate_mut().replace(cols);
            t.refresh(cx);
        });
        self.idx_grid.update(cx, |t, cx| {
            t.delegate_mut().replace(idx);
            t.refresh(cx);
        });
        self.fk_grid.update(cx, |t, cx| {
            t.delegate_mut().replace(fks);
            t.refresh(cx);
        });
    }

    /// The row picked in one of the structure lists.
    fn info_selection(grid: &Entity<TableState<InfoDelegate>>, cx: &App) -> Option<usize> {
        match grid.read(cx).selection() {
            TableSelection::Cell(r, _) | TableSelection::Row(r) => Some(r),
            _ => None,
        }
    }

    /// Pick a row of the column list, as a click would (for tests).
    pub fn select_structure_column(&mut self, row: usize, cx: &mut Context<Self>) {
        self.col_grid
            .update(cx, |t, cx| t.set_selected_row(row, cx));
    }

    /// The structure view's data, when loaded.
    pub fn structure(&self) -> Option<&StructureData> {
        self.structure.as_ref()
    }

    /// Answer to `StructureRequested`: store the definition (and DDL text,
    /// when the engine provides one) or the reason loading failed.
    pub fn set_structure(
        &mut self,
        detail: Result<std::sync::Arc<TableDetail>, String>,
        ddl: Result<Option<String>, String>,
        cx: &mut Context<Self>,
    ) {
        self.structure_loading = false;
        match (detail, ddl) {
            (Ok(detail), Ok(ddl)) => {
                self.structure = Some(StructureData { detail, ddl });
                self.sync_info_grids(cx);
                self.structure_error = None;
            }
            (Err(e), _) | (_, Err(e)) => {
                self.structure_error = Some(e.into());
            }
        }
        cx.notify();
    }

    /// Drop the structure data, so the next switch reloads it. The
    /// controller calls this after a structure commit.
    pub fn clear_structure(&mut self, cx: &mut Context<Self>) {
        self.structure = None;
        self.structure_error = None;
        cx.notify();
    }

    // ---- Structure editing (M6) ----

    /// The staged structure changes, in order.
    pub fn structure_changes(&self) -> &[SchemaChange] {
        self.structure_changes.changes()
    }

    /// Stage one structure change. The preview, if open, goes stale and is
    /// recomputed on the next toggle.
    pub fn push_structure_change(&mut self, change: SchemaChange, cx: &mut Context<Self>) {
        self.structure_changes.push(change);
        self.structure_preview = None;
        self.structure_confirm = false;
        cx.notify();
    }

    pub fn discard_structure_changes(&mut self, cx: &mut Context<Self>) {
        self.structure_changes.clear();
        self.structure_preview = None;
        self.structure_confirm = false;
        self.edit_column = None;
        cx.notify();
    }

    pub fn undo_structure(&mut self, cx: &mut Context<Self>) {
        if self.structure_changes.undo() {
            self.structure_preview = None;
            cx.notify();
        }
    }

    pub fn redo_structure(&mut self, cx: &mut Context<Self>) {
        if self.structure_changes.redo() {
            self.structure_preview = None;
            cx.notify();
        }
    }

    /// The compiled preview, when the panel is open.
    pub fn structure_preview(&self) -> Option<&Result<Vec<SchemaStatement>, String>> {
        self.structure_preview.as_ref()
    }

    pub fn toggle_structure_preview(&mut self, cx: &mut Context<Self>) {
        if self.structure_preview_open {
            self.structure_preview_open = false;
        } else {
            self.structure_preview = Some(self.compile_structure());
            self.structure_preview_open = true;
        }
        cx.notify();
    }

    fn compile_structure(&self) -> Result<Vec<SchemaStatement>, String> {
        let Some(data) = &self.structure else {
            return Err("No structure loaded.".into());
        };
        let Some(engine) = self.engine else {
            return Err("The engine for this tab is unknown.".into());
        };
        let Some(dialect) = dialect_for(engine) else {
            return Err("The engine for this tab is unknown.".into());
        };
        compile_schema_changes(
            self.query.path(),
            &data.detail,
            self.structure_changes.changes(),
            dialect,
        )
        .map_err(|e| e.to_string())
    }

    /// Warnings for the confirm bar, computed locally from the staged
    /// changes and the compiled preview: dropping a table, DDL that cannot
    /// roll back, and table rebuilds. The service computes the same set for
    /// its own callers.
    pub fn structure_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self
            .structure_changes
            .changes()
            .iter()
            .any(|c| matches!(c, SchemaChange::DropTable))
        {
            warnings.push("Dropping a table cannot be undone.".into());
        }
        if let Some(engine) = self.engine
            && let Ok(driver) = driver_for(engine)
            && !driver.capabilities().transactional_ddl
        {
            warnings.push(
                "DDL on this engine cannot be rolled back: if a later statement fails, \
                 earlier ones stay applied."
                    .into(),
            );
        }
        if let Ok(steps) = self.compile_structure()
            && steps
                .iter()
                .any(|s| s.description.contains("via table rebuild"))
        {
            warnings.push(
                "One or more changes rebuild the table: a new table is created, rows are \
                 copied, the old table is dropped, and the new one is renamed back. Indexes \
                 and triggers are recreated from their stored definitions."
                    .into(),
            );
        }
        warnings
    }

    /// Ask the owner to apply the staged changes. Guarded like the data
    /// commit: a second Save while one runs is ignored.
    pub fn request_structure_commit(&mut self, cx: &mut Context<Self>) {
        if self.structure_committing || self.structure_changes.is_empty() {
            return;
        }
        self.structure_confirm = false;
        self.structure_committing = true;
        cx.emit(TableTabEvent::StructureCommitRequested);
        cx.notify();
    }

    /// Answer to `StructureCommitRequested`. A partial failure (engines
    /// without transactional DDL) keeps the staged changes and reports
    /// which statement failed; a full success clears the staging and
    /// reloads the structure.
    pub fn structure_commit_finished(
        &mut self,
        result: Result<ApplySchemaOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        self.structure_committing = false;
        match result {
            Ok(outcome) => match outcome.failed {
                Some(failed) => {
                    self.structure_error = Some(failed.into());
                }
                None => {
                    self.structure_changes.clear();
                    self.structure_preview = None;
                    self.structure_preview_open = false;
                    self.reload_structure(cx);
                    return;
                }
            },
            Err(e) => {
                self.structure_error = Some(e.into());
            }
        }
        cx.notify();
    }

    /// Reload the structure data on the next switch-independent load: used
    /// after a successful commit.
    pub fn reload_structure(&mut self, cx: &mut Context<Self>) {
        self.structure = None;
        self.structure_loading = true;
        self.structure_error = None;
        cx.emit(TableTabEvent::StructureRequested);
        cx.notify();
    }

    /// One of the structure forms' text inputs, by id, for tests and for
    /// reading values in handlers.
    pub fn structure_form(&self, name: &str) -> Option<Entity<InputState>> {
        let f = self.structure_forms.as_ref()?;
        Some(match name {
            "add-col-name" => f.add_col_name.clone(),
            "add-col-type" => f.add_col_type.clone(),
            "add-col-default" => f.add_col_default.clone(),
            "edit-col-type" => f.edit_col_type.clone(),
            "edit-col-default" => f.edit_col_default.clone(),
            "add-idx-name" => f.add_idx_name.clone(),
            "add-idx-columns" => f.add_idx_columns.clone(),
            "add-fk-name" => f.add_fk_name.clone(),
            "add-fk-columns" => f.add_fk_columns.clone(),
            "add-fk-ref-table" => f.add_fk_ref_table.clone(),
            "add-fk-ref-columns" => f.add_fk_ref_columns.clone(),
            "add-fk-on-delete" => f.add_fk_on_delete.clone(),
            "add-fk-on-update" => f.add_fk_on_update.clone(),
            "rename-table-name" => f.rename_table_name.clone(),
            _ => return None,
        })
    }

    fn ensure_structure_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.structure_forms.is_some() {
            return;
        }
        let mk =
            |window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx));
        self.structure_forms = Some(StructureForms {
            add_col_name: mk(window, cx),
            add_col_type: mk(window, cx),
            add_col_default: mk(window, cx),
            add_col_nullable: true,
            edit_col_type: mk(window, cx),
            edit_col_default: mk(window, cx),
            edit_col_nullable: true,
            add_idx_name: mk(window, cx),
            add_idx_columns: mk(window, cx),
            add_idx_unique: false,
            add_fk_name: mk(window, cx),
            add_fk_columns: mk(window, cx),
            add_fk_ref_table: mk(window, cx),
            add_fk_ref_columns: mk(window, cx),
            add_fk_on_delete: mk(window, cx),
            add_fk_on_update: mk(window, cx),
            rename_table_name: mk(window, cx),
        });
    }

    fn form_value(&self, name: &str, cx: &App) -> String {
        self.structure_form(name)
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }

    fn set_form_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.structure_form_error = Some(message.into());
        cx.notify();
    }

    fn clear_input(&self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.structure_form(name) {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    fn add_structure_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.form_value("add-col-name", cx);
        let ty = self.form_value("add-col-type", cx);
        let default = self.form_value("add-col-default", cx);
        if name.trim().is_empty() {
            self.set_form_error("A column needs a name.", cx);
            return;
        }
        if ty.trim().is_empty() {
            self.set_form_error("A column needs a type.", cx);
            return;
        }
        let nullable = self
            .structure_forms
            .as_ref()
            .is_some_and(|f| f.add_col_nullable);
        let ordinal = self
            .structure
            .as_ref()
            .map(|d| d.detail.columns.len() as u32 + 1)
            .unwrap_or(1);
        let mut column = ColumnInfo::new(
            name.trim(),
            ordinal,
            DataType::new(ty.trim(), TypeCategory::Other),
        );
        column.nullable = nullable;
        column.default = if default.is_empty() {
            None
        } else {
            Some(default)
        };
        self.structure_form_error = None;
        self.push_structure_change(SchemaChange::AddColumn { column }, cx);
        self.clear_input("add-col-name", window, cx);
        self.clear_input("add-col-type", window, cx);
        self.clear_input("add-col-default", window, cx);
    }

    fn begin_structure_edit(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self
            .structure
            .as_ref()
            .and_then(|d| d.detail.column(name))
            .cloned()
        else {
            return;
        };
        self.edit_column = Some(name.to_owned());
        if let Some(forms) = self.structure_forms.as_mut() {
            forms.edit_col_nullable = column.nullable;
        }
        if let Some(input) = self.structure_form("edit-col-type") {
            let ty = column.data_type.native.clone();
            input.update(cx, |s, cx| s.set_value(ty, window, cx));
        }
        if let Some(input) = self.structure_form("edit-col-default") {
            let default = column.default.clone().unwrap_or_default();
            input.update(cx, |s, cx| s.set_value(default, window, cx));
        }
        cx.notify();
    }

    fn save_structure_edit(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.edit_column.clone() else {
            return;
        };
        let ty = self.form_value("edit-col-type", cx);
        let default = self.form_value("edit-col-default", cx);
        if ty.trim().is_empty() {
            self.set_form_error("A column needs a type.", cx);
            return;
        }
        let nullable = self
            .structure_forms
            .as_ref()
            .is_some_and(|f| f.edit_col_nullable);
        self.structure_form_error = None;
        self.edit_column = None;
        // Empty default means "drop the default" when one exists; the
        // compiler filters it back to a no-op when there is none.
        let default = if default.is_empty() {
            None
        } else {
            Some(default)
        };
        self.push_structure_change(
            SchemaChange::AlterColumn {
                name,
                new_type: Some(ty.trim().to_owned()),
                nullable: Some(nullable),
                default: Some(default),
            },
            cx,
        );
    }

    fn drop_structure_column(&mut self, name: &str, cx: &mut Context<Self>) {
        self.edit_column = None;
        self.push_structure_change(SchemaChange::DropColumn { name: name.into() }, cx);
    }

    fn drop_structure_index(&mut self, name: &str, cx: &mut Context<Self>) {
        self.push_structure_change(SchemaChange::DropIndex { name: name.into() }, cx);
    }

    fn drop_structure_fk(&mut self, name: &str, cx: &mut Context<Self>) {
        self.push_structure_change(SchemaChange::DropForeignKey { name: name.into() }, cx);
    }

    fn add_structure_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.form_value("add-idx-name", cx);
        let columns = self.form_value("add-idx-columns", cx);
        if name.trim().is_empty() {
            self.set_form_error("An index needs a name.", cx);
            return;
        }
        let cols: Vec<IndexColumn> = columns
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(IndexColumn::column)
            .collect();
        if cols.is_empty() {
            self.set_form_error("An index needs at least one column.", cx);
            return;
        }
        let unique = self
            .structure_forms
            .as_ref()
            .is_some_and(|f| f.add_idx_unique);
        let index = IndexInfo {
            name: name.trim().to_owned(),
            columns: cols,
            unique,
            primary: false,
            method: None,
            predicate: None,
            comment: None,
            extra: Default::default(),
        };
        self.structure_form_error = None;
        self.push_structure_change(SchemaChange::AddIndex { index }, cx);
        self.clear_input("add-idx-name", window, cx);
        self.clear_input("add-idx-columns", window, cx);
    }

    fn add_structure_fk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.form_value("add-fk-name", cx);
        let columns = self.form_value("add-fk-columns", cx);
        let ref_table = self.form_value("add-fk-ref-table", cx);
        let ref_columns = self.form_value("add-fk-ref-columns", cx);
        let on_delete = self.form_value("add-fk-on-delete", cx);
        let on_update = self.form_value("add-fk-on-update", cx);
        let cols: Vec<String> = columns
            .split(',')
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
            .collect();
        if cols.is_empty() {
            self.set_form_error("A foreign key needs at least one column.", cx);
            return;
        }
        if ref_table.trim().is_empty() {
            self.set_form_error("A foreign key needs a referenced table.", cx);
            return;
        }
        let namespace = self.query.path().namespace.clone();
        let fk = ForeignKeyInfo {
            name: if name.trim().is_empty() {
                None
            } else {
                Some(name.trim().to_owned())
            },
            columns: cols,
            referenced: ObjectPath::new(namespace.as_deref(), ref_table.trim()),
            referenced_columns: ref_columns
                .split(',')
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
                .collect(),
            on_update: if on_update.trim().is_empty() {
                None
            } else {
                Some(ReferentialAction::parse(&on_update))
            },
            on_delete: if on_delete.trim().is_empty() {
                None
            } else {
                Some(ReferentialAction::parse(&on_delete))
            },
            extra: Default::default(),
        };
        self.structure_form_error = None;
        self.push_structure_change(SchemaChange::AddForeignKey { fk }, cx);
        for id in [
            "add-fk-name",
            "add-fk-columns",
            "add-fk-ref-table",
            "add-fk-ref-columns",
            "add-fk-on-delete",
            "add-fk-on-update",
        ] {
            self.clear_input(id, window, cx);
        }
    }

    fn rename_structure_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.form_value("rename-table-name", cx);
        if name.trim().is_empty() {
            self.set_form_error("A table needs a name.", cx);
            return;
        }
        self.structure_form_error = None;
        self.push_structure_change(
            SchemaChange::RenameTable {
                new_name: name.trim().to_owned(),
            },
            cx,
        );
        self.clear_input("rename-table-name", window, cx);
    }

    fn drop_structure_table(&mut self, cx: &mut Context<Self>) {
        self.push_structure_change(SchemaChange::DropTable, cx);
    }

    /// The structure half's content: four sections (columns, indexes,
    /// foreign keys, DDL text), each with an `aria_label` carrying its
    /// lines for tests and readers. Loading and error states get their own
    /// panels instead.
    fn structure_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.structure_loading {
            return div()
                .id("structure-loading")
                .test_support()
                .aria_label("Loading structure…")
                .px_3()
                .py_2()
                .text_sm()
                .child("Loading…")
                .into_any_element();
        }
        let Some(data) = &self.structure else {
            if let Some(error) = &self.structure_error {
                return div()
                    .id("structure-error")
                    .test_support()
                    .aria_label(error.clone())
                    .px_3()
                    .py_2()
                    .text_sm()
                    .child(error.clone())
                    .into_any_element();
            }
            return div()
                .id("structure-empty")
                .test_support()
                .aria_label("No structure loaded.")
                .px_3()
                .py_2()
                .text_sm()
                .child("No structure loaded.")
                .into_any_element();
        };
        let detail = &data.detail;
        let columns: Vec<String> = detail
            .columns
            .iter()
            .map(|c| {
                let mut line = format!("{} {}", c.name, c.data_type.native);
                if !c.nullable {
                    line.push_str(" NOT NULL");
                }
                if let Some(pos) = c.primary_key_position {
                    line.push_str(&format!(" PK({pos})"));
                }
                if c.auto_increment {
                    line.push_str(" AUTO_INCREMENT");
                }
                if let Some(default) = &c.default {
                    line.push_str(&format!(" DEFAULT {default}"));
                }
                line
            })
            .collect();
        let indexes: Vec<String> = detail
            .indexes
            .iter()
            .map(|ix| {
                let cols = ix
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut line = format!("{} ({cols})", ix.name);
                if ix.unique {
                    line.push_str(" UNIQUE");
                }
                if ix.primary {
                    line.push_str(" PRIMARY");
                }
                line
            })
            .collect();
        let fks: Vec<String> = detail
            .foreign_keys
            .iter()
            .map(|fk| {
                let cols = fk.columns.join(", ");
                let mut line = format!(
                    "{}: ({cols}) -> {}",
                    fk.name.clone().unwrap_or_default(),
                    fk.referenced.name
                );
                if !fk.referenced_columns.is_empty() {
                    line.push_str(&format!(" ({})", fk.referenced_columns.join(", ")));
                }
                if let Some(action) = &fk.on_delete {
                    line.push_str(&format!(" ON DELETE {}", action.sql()));
                }
                if let Some(action) = &fk.on_update {
                    line.push_str(&format!(" ON UPDATE {}", action.sql()));
                }
                line
            })
            .collect();
        let theme = cx.theme().clone();
        let index_mode = self.mode == TabMode::Index;
        let ddl = data
            .ddl
            .clone()
            .unwrap_or_else(|| "This engine does not provide DDL text.".into());
        let mut panel = v_flex()
            .id("structure-view")
            .test_support()
            .size_full()
            .gap_3()
            .p_3()
            .overflow_y_scroll();
        if let Some(error) = &self.structure_error {
            panel = panel.child(
                div()
                    .id("structure-commit-error")
                    .test_support()
                    .aria_label(error.clone())
                    .px_3()
                    .py_2()
                    .text_sm()
                    .child(error.clone()),
            );
        }
        // Columns, with per-row Alter / Drop and an add form.
        let mut col_section = v_flex()
            .id("structure-columns")
            .test_support()
            .aria_label(if columns.is_empty() {
                "Columns: none".to_owned()
            } else {
                columns.join("\n")
            })
            .gap_1p5()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Columns"),
            );
        col_section = col_section.child(
            div()
                .h(px(240.))
                .border_1()
                .border_color(theme.border)
                .child(
                    DataTable::new(&self.col_grid)
                        .with_size(px(22.))
                        .bordered(false)
                        .fixed_cols_border(false)
                        .header_row_height(px(24.))
                        .stripe(true),
                ),
        );
        let picked_col = Self::info_selection(&self.col_grid, cx)
            .and_then(|i| detail.columns.get(i))
            .map(|c| c.name.clone());
        let (alter_target, drop_target) = (picked_col.clone(), picked_col.clone());
        col_section = col_section.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("structure-alter-col")
                        .label("Alter")
                        .small()
                        .disabled(picked_col.is_none())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(name) = &alter_target {
                                this.begin_structure_edit(name, window, cx);
                            }
                        })),
                )
                .child(
                    Button::new("structure-drop-col")
                        .label("Drop")
                        .small()
                        .disabled(picked_col.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(name) = &drop_target {
                                this.drop_structure_column(name, cx);
                            }
                        })),
                ),
        );
        if let Some(name) = self.edit_column.clone() {
            let mut row = h_flex()
                .gap_2()
                .child(div().text_sm().child(format!("{name}:")));
            if let Some(input) = self.structure_form("edit-col-type") {
                row = row.child(
                    div()
                        .flex_1()
                        .child(Input::new(&input).id("structure-edit-col-type")),
                );
            }
            if let Some(input) = self.structure_form("edit-col-default") {
                row = row.child(
                    div()
                        .flex_1()
                        .child(Input::new(&input).id("structure-edit-col-default")),
                );
            }
            if let Some(forms) = &self.structure_forms {
                let checked = forms.edit_col_nullable;
                row = row.child(
                    Checkbox::new(SharedString::from("structure-edit-col-nullable"))
                        .label("Nullable")
                        .checked(checked)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(forms) = this.structure_forms.as_mut() {
                                forms.edit_col_nullable = !forms.edit_col_nullable;
                            }
                            cx.notify();
                        })),
                );
            }
            row = row.child(
                Button::new("structure-save-col-edit")
                    .label("Save")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.save_structure_edit(cx))),
            );
            row = row.child(
                Button::new("structure-cancel-col-edit")
                    .label("Cancel")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.edit_column = None;
                        cx.notify();
                    })),
            );
            col_section = col_section.child(row);
        }
        if let Some(forms) = &self.structure_forms {
            let add_col_name = forms.add_col_name.clone();
            let add_col_type = forms.add_col_type.clone();
            let add_col_default = forms.add_col_default.clone();
            let nullable = forms.add_col_nullable;
            col_section = col_section.child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&add_col_name).id("structure-add-col-name")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&add_col_type).id("structure-add-col-type")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&add_col_default).id("structure-add-col-default")),
                    )
                    .child(
                        Checkbox::new(SharedString::from("structure-add-col-nullable"))
                            .label("Nullable")
                            .checked(nullable)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(forms) = this.structure_forms.as_mut() {
                                    forms.add_col_nullable = !forms.add_col_nullable;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("structure-add-col")
                            .label("Add column")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_structure_column(window, cx);
                            })),
                    ),
            );
        }
        if !index_mode {
            panel = panel.child(col_section);
        }
        // Indexes, with per-row Drop and an add form.
        let mut idx_section = v_flex()
            .id("structure-indexes")
            .test_support()
            .aria_label(if indexes.is_empty() {
                "Indexes: none".to_owned()
            } else {
                indexes.join("\n")
            })
            .gap_1p5()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Indexes"),
            );
        idx_section = idx_section.child(
            div()
                .h(px(280.))
                .border_1()
                .border_color(theme.border)
                .child(
                    DataTable::new(&self.idx_grid)
                        .with_size(px(22.))
                        .bordered(false)
                        .fixed_cols_border(false)
                        .header_row_height(px(24.))
                        .stripe(true),
                ),
        );
        let picked_idx = Self::info_selection(&self.idx_grid, cx)
            .and_then(|i| detail.indexes.get(i))
            .map(|ix| ix.name.clone());
        idx_section = idx_section.child(
            h_flex().gap_2().child(
                Button::new("structure-drop-idx")
                    .label("Drop")
                    .small()
                    .disabled(picked_idx.is_none())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(name) = &picked_idx {
                            this.drop_structure_index(name, cx);
                        }
                    })),
            ),
        );
        if let Some(forms) = &self.structure_forms {
            let add_idx_name = forms.add_idx_name.clone();
            let add_idx_columns = forms.add_idx_columns.clone();
            let unique = forms.add_idx_unique;
            idx_section = idx_section.child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&add_idx_name).id("structure-add-idx-name")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&add_idx_columns).id("structure-add-idx-columns")),
                    )
                    .child(
                        Checkbox::new(SharedString::from("structure-add-idx-unique"))
                            .label("Unique")
                            .checked(unique)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(forms) = this.structure_forms.as_mut() {
                                    forms.add_idx_unique = !forms.add_idx_unique;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("structure-add-idx")
                            .label("Add index")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_structure_index(window, cx);
                            })),
                    ),
            );
        }
        if index_mode {
            panel = panel.child(idx_section);
        }
        // Foreign keys, with per-row Drop and an add form.
        let mut fk_section = v_flex()
            .id("structure-fks")
            .test_support()
            .aria_label(if fks.is_empty() {
                "Foreign keys: none".to_owned()
            } else {
                fks.join("\n")
            })
            .gap_1p5()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Foreign keys"),
            );
        fk_section = fk_section.child(
            div()
                .h(px(150.))
                .border_1()
                .border_color(theme.border)
                .child(
                    DataTable::new(&self.fk_grid)
                        .with_size(px(22.))
                        .bordered(false)
                        .fixed_cols_border(false)
                        .header_row_height(px(24.))
                        .stripe(true),
                ),
        );
        let picked_fk = Self::info_selection(&self.fk_grid, cx)
            .and_then(|i| detail.foreign_keys.get(i))
            .and_then(|fk| fk.name.clone());
        fk_section = fk_section.child(
            h_flex().gap_2().child(
                Button::new("structure-drop-fk")
                    .label("Drop")
                    .small()
                    .disabled(picked_fk.is_none())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(name) = &picked_fk {
                            this.drop_structure_fk(name, cx);
                        }
                    })),
            ),
        );
        if let Some(forms) = &self.structure_forms {
            let add_fk_name = forms.add_fk_name.clone();
            let add_fk_columns = forms.add_fk_columns.clone();
            let add_fk_ref_table = forms.add_fk_ref_table.clone();
            let add_fk_ref_columns = forms.add_fk_ref_columns.clone();
            let add_fk_on_delete = forms.add_fk_on_delete.clone();
            let add_fk_on_update = forms.add_fk_on_update.clone();
            fk_section =
                fk_section.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&add_fk_name).id("structure-add-fk-name")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&add_fk_columns).id("structure-add-fk-columns")),
                        )
                        .child(
                            div().flex_1().child(
                                Input::new(&add_fk_ref_table).id("structure-add-fk-ref-table"),
                            ),
                        )
                        .child(div().flex_1().child(
                            Input::new(&add_fk_ref_columns).id("structure-add-fk-ref-columns"),
                        ))
                        .child(
                            div().flex_1().child(
                                Input::new(&add_fk_on_delete).id("structure-add-fk-on-delete"),
                            ),
                        )
                        .child(
                            div().flex_1().child(
                                Input::new(&add_fk_on_update).id("structure-add-fk-on-update"),
                            ),
                        )
                        .child(
                            Button::new("structure-add-fk")
                                .label("Add foreign key")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_structure_fk(window, cx);
                                })),
                        ),
                );
        }
        if !index_mode {
            panel = panel.child(fk_section);
        }
        // Table actions: rename and drop.
        if let (false, Some(forms)) = (index_mode, &self.structure_forms) {
            let rename_table_name = forms.rename_table_name.clone();
            panel =
                panel.child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .child(div().flex_1().child(
                            Input::new(&rename_table_name).id("structure-rename-table-name"),
                        ))
                        .child(
                            Button::new("structure-rename-table")
                                .label("Rename table")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.rename_structure_table(window, cx);
                                })),
                        )
                        .child(
                            Button::new("structure-drop-table")
                                .label("Drop table")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.drop_structure_table(cx);
                                })),
                        ),
                );
        }
        // DDL text, as reported by the engine.
        if !index_mode {
            panel = panel.child(
                v_flex()
                    .id("structure-ddl")
                    .test_support()
                    .aria_label(ddl.clone())
                    .gap_1p5()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("DDL"),
                    )
                    .child(div().text_sm().font_family("monospace").child(ddl)),
            );
        }
        // Form validation errors.
        if let Some(error) = &self.structure_form_error {
            panel = panel.child(
                div()
                    .id("structure-form-error")
                    .test_support()
                    .aria_label(error.clone())
                    .px_3()
                    .py_2()
                    .text_sm()
                    .child(error.clone()),
            );
        }
        // Staging bar, preview, and confirm bar.
        let staged = self.structure_changes.len();
        if staged > 0 {
            panel = panel.child(
                h_flex()
                    .id("structure-staging")
                    .test_support()
                    .aria_label(format!("{staged} staged structure change(s)"))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(div().text_sm().child(format!("{staged} staged change(s)")))
                    .child(
                        Button::new("structure-preview-toggle")
                            .label("Preview")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_structure_preview(cx);
                            })),
                    )
                    .child(
                        Button::new("structure-save")
                            .label("Save")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.structure_confirm = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("structure-discard")
                            .label("Discard")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.discard_structure_changes(cx);
                            })),
                    ),
            );
        }
        if self.structure_preview_open {
            let preview_text = match &self.structure_preview {
                Some(Ok(steps)) => steps
                    .iter()
                    .map(|s| s.sql.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(Err(e)) => e.clone(),
                None => String::new(),
            };
            panel = panel.child(
                div()
                    .id("structure-preview")
                    .test_support()
                    .aria_label(preview_text.clone())
                    .px_3()
                    .py_2()
                    .child(div().text_sm().child("Preview"))
                    .child(div().text_sm().child(preview_text)),
            );
        }
        if self.structure_confirm {
            let warnings = self.structure_warnings();
            let joined = if warnings.is_empty() {
                "No warnings.".to_owned()
            } else {
                warnings.join("\n")
            };
            panel = panel.child(
                v_flex()
                    .id("structure-confirm")
                    .test_support()
                    .aria_label(joined.clone())
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(div().text_sm().child(joined))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("structure-apply")
                                    .label("Apply")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.request_structure_commit(cx);
                                    })),
                            )
                            .child(
                                Button::new("structure-cancel-confirm")
                                    .label("Cancel")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.structure_confirm = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        panel.into_any_element()
    }

    pub fn query(&self) -> &GridQuery {
        &self.query
    }

    /// The value shown at (row, column) of the current page.
    pub fn value_at(&self, row: usize, col: usize, cx: &App) -> Option<Value> {
        self.table
            .read(cx)
            .delegate()
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
    }

    /// Returns the selected row data (column names, types, values) if a cell or row is selected.
    pub fn selected_row_detail(&self, cx: &App) -> Option<SelectedRowData> {
        let table = self.table.read(cx);
        let d = table.delegate();
        let row_ix = match table.selection() {
            TableSelection::Cell(r, _) => r,
            TableSelection::Row(r) => r,
            _ => return None,
        };
        let row = d.rows.get(row_ix)?;
        let mut fields = Vec::with_capacity(d.metas.len());
        for (i, meta) in d.metas.iter().enumerate() {
            let val = row.get(i).unwrap_or(&Value::Null);
            let is_null = val.is_null();
            let value_text = copy_text(val);
            fields.push(FieldDetail {
                name: meta.name.clone(),
                data_type: meta.data_type.native.clone(),
                value_text,
                is_null,
            });
        }
        Some(SelectedRowData { row_ix, fields })
    }

    /// The selected row index, if any.
    pub fn selected_row_index(&self, cx: &App) -> Option<usize> {
        let table = self.table.read(cx);
        match table.selection() {
            TableSelection::Cell(r, _) => Some(r),
            TableSelection::Row(r) => Some(r),
            _ => None,
        }
    }

    pub fn column_names(&self, cx: &App) -> Vec<String> {
        self.table
            .read(cx)
            .delegate()
            .metas
            .iter()
            .map(|m| m.name.clone())
            .collect()
    }

    pub fn loaded_rows(&self) -> usize {
        self.loaded_rows
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    // ---- staged edits (roadmap M3) -------------------------------------------------------------

    /// The table's primary-key columns, from its metadata. Without one the grid is read-only.
    pub fn set_primary_key(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.key_columns = keys;
            d.rebuild_changeset();
            cx.notify();
        });
        cx.notify();
    }

    /// Rows modified, added and marked for deletion.
    pub fn pending(&self, cx: &App) -> Counts {
        self.table
            .read(cx)
            .delegate()
            .changeset
            .as_ref()
            .map(Changeset::counts)
            .unwrap_or_default()
    }

    /// The staged changes, ready for `fg-sql` to compile.
    pub fn changes(&self, cx: &App) -> Vec<RowChange> {
        self.table
            .read(cx)
            .delegate()
            .changeset
            .as_ref()
            .map(Changeset::changes)
            .unwrap_or_default()
    }

    /// Why rows cannot be edited here, if they cannot.
    pub fn read_only_reason(&self, cx: &App) -> Option<&'static str> {
        self.table
            .read(cx)
            .delegate()
            .changeset
            .as_ref()
            .and_then(Changeset::read_only_reason)
    }

    /// The selected cell as (row, data column), counted like [`Self::select_cell`]: the
    /// row-number gutter is not a column.
    pub fn selected_cell(&self, cx: &App) -> Option<(usize, usize)> {
        match self.table.read(cx).selection() {
            TableSelection::Cell(r, c) => c.checked_sub(GUTTER).map(|c| (r, c)),
            _ => None,
        }
    }

    /// Give the grid the keyboard (what clicking a cell does).
    pub fn focus_grid(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_table(window, cx);
    }

    pub fn is_editing(&self, cx: &App) -> bool {
        self.table.read(cx).delegate().editing.is_some()
    }

    /// The text in the open cell editor.
    pub fn editor_text(&self, cx: &App) -> Option<String> {
        self.table
            .read(cx)
            .delegate()
            .editing
            .as_ref()
            .map(|e| e.input.read(cx).value().to_string())
    }

    /// The editor's text box, so a test (or the owner) can type into it.
    pub fn editor_input(&self, cx: &App) -> Option<Entity<InputState>> {
        self.table
            .read(cx)
            .delegate()
            .editing
            .as_ref()
            .map(|e| e.input.clone())
    }

    /// The value on screen at (row, data column), staged edits included.
    pub fn shown_value_at(&self, row: usize, col: usize, cx: &App) -> Option<Value> {
        let d = self.table.read(cx).delegate();
        (row < d.total_rows() && col < d.metas.len()).then(|| d.shown_value(row, col))
    }

    /// How the cell is marked: edited, added or deleted.
    pub fn mark_at(&self, row: usize, col: usize, cx: &App) -> CellMark {
        self.table.read(cx).delegate().mark_for(row, col)
    }

    pub fn total_rows(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().total_rows()
    }

    fn fail_edit(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    /// Open the selected cell for editing (`Enter`).
    pub fn begin_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let TableSelection::Cell(row, col) = self.table.read(cx).selection() {
            self.begin_edit_at(row, col, None, window, cx);
        }
    }

    /// Typing on a selected cell starts editing it with what was typed, replacing the old value
    /// (the spreadsheet way). Returns whether the key was taken.
    fn type_to_edit(&mut self, typed: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // Only when the grid itself has the keyboard: the filter drawer's text boxes sit inside
        // this view and must keep their own keystrokes.
        if !self.table.focus_handle(cx).is_focused(window) || self.is_editing(cx) {
            return false;
        }
        let TableSelection::Cell(row, col) = self.table.read(cx).selection() else {
            return false;
        };
        self.begin_edit_at(row, col, Some(typed.to_owned()), window, cx);
        self.is_editing(cx)
    }

    fn begin_edit_at(
        &mut self,
        row: usize,
        screen_col: usize,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let d = self.table.read(cx).delegate();
        if d.editing.is_some() || row >= d.total_rows() {
            return;
        }
        if let Some(reason) = d.changeset.as_ref().and_then(Changeset::read_only_reason) {
            self.fail_edit(reason, cx);
            return;
        }
        let Some(data_col) = d.data_index(screen_col) else {
            return;
        };
        if d.mark_for(row, data_col) == CellMark::Deleted {
            self.fail_edit("This row is marked for deletion; restore it first.", cx);
            return;
        }
        // The editor's list, when the column has one: an enum/set column's
        // variants are the allowed values; a foreign-key column offers the
        // distinct values already on the loaded page as a shortcut, and
        // typing a different key still works.
        let (options, allowed) = {
            let meta = &d.metas[data_col];
            if matches!(
                meta.data_type.category,
                TypeCategory::Enum | TypeCategory::Set
            ) && !meta.data_type.variants.is_empty()
            {
                (meta.data_type.variants.clone(), true)
            } else if d.jumps.contains_key(&meta.name) {
                let mut seen = HashSet::new();
                let mut values = Vec::new();
                for row in &d.rows {
                    if let Some(v) = row.get(data_col)
                        && !v.is_null()
                        && seen.insert(copy_text(v))
                    {
                        values.push(copy_text(v));
                    }
                }
                values.truncate(100);
                (values, false)
            } else {
                (Vec::new(), false)
            }
        };
        self.editor_options = options;
        self.editor_options_are_allowed = allowed;
        let replacing = initial.is_some();
        let text = match initial {
            Some(typed) => typed,
            None => match d.shown_value(row, data_col) {
                Value::Null => "NULL".to_owned(),
                Value::Other { type_name, text } if type_name.is_empty() && text == "DEFAULT" => {
                    String::new()
                }
                other => copy_text(&other),
            },
        };
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(text, window, cx);
            state
        });
        self._edit_subscription =
            Some(
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        // Enter confirms and moves down, like a spreadsheet.
                        InputEvent::PressEnter { .. } => this.commit_and_move(0, 1, window, cx),
                        // Clicking away keeps what was typed instead of throwing it away.
                        InputEvent::Blur => this.finish_on_blur(window, cx),
                        _ => {}
                    }
                }),
            );
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            if !replacing {
                state.select_all(window, cx);
            }
        });
        self.error = None;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().editing = Some(CellEditor {
                row_ix: row,
                screen_col,
                input,
            });
            cx.notify();
        });
        cx.notify();
    }

    /// Stage what is in the editor (`Enter`). A value that does not fit the column keeps the
    /// editor open with the reason, rather than staging something else.
    pub fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let d = self.table.read(cx).delegate();
        let Some(editor) = d.editing.as_ref() else {
            return true;
        };
        let (row, screen_col) = (editor.row_ix, editor.screen_col);
        let text = editor.input.read(cx).value().to_string();
        let Some(data_col) = d.data_index(screen_col) else {
            return false;
        };
        let category = d.metas[data_col].data_type.category;
        let value = match parse_cell_text(&text, category) {
            Ok(v) => v,
            Err(message) => {
                self.fail_edit(message, cx);
                return false;
            }
        };
        let staged = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let id = d.row_id_at(row).ok_or("This row is no longer on screen.")?;
            let baseline = d.rows.get(row).cloned().unwrap_or_default();
            let cs = d.changeset.as_mut().ok_or("Nothing to edit yet.")?;
            cs.edit_cell(&id, &baseline, data_col, CellChange::Set(value))?;
            d.editing = None;
            cx.notify();
            Ok::<(), String>(())
        });
        match staged {
            Ok(()) => {
                self.error = None;
                self._edit_subscription = None;
                self.editor_options.clear();
                if self.detail_open {
                    self.detail_cell = None;
                }
                self.focus_table(window, cx);
                cx.notify();
                true
            }
            Err(message) => {
                self.fail_edit(message, cx);
                false
            }
        }
    }

    /// Pick one of the editor's listed values and stage it, as choosing from
    /// a dropdown does. A value that does not parse keeps the editor open
    /// with the reason, like typing it by hand would.
    pub fn pick_editor_option(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(value) = self.editor_options.get(index).cloned() else {
            return;
        };
        let d = self.table.read(cx).delegate();
        if let Some(editor) = d.editing.as_ref() {
            let input = editor.input.clone();
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.commit_edit(window, cx);
    }

    // ---- row-detail inspector ---------------------------------------------------------------

    pub fn detail_open(&self, cx: &App) -> bool {
        let _ = cx;
        self.detail_open && self.detail_input.is_some()
    }

    pub fn detail_text(&self, cx: &App) -> Option<String> {
        self.detail_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
    }

    /// Replace the inspector's text, e.g. after pasting a value into it.
    pub fn set_detail_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.detail_input.clone() {
            let text = text.to_owned();
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
    }

    /// Open the inspector on the selected cell, or close it. While a cell
    /// editor is open, Space types into that editor instead.
    pub fn toggle_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_editing(cx) {
            return;
        }
        if self.detail_open {
            self.close_detail();
            cx.notify();
            return;
        }
        if !matches!(self.table.read(cx).selection(), TableSelection::Cell(..)) {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        self.detail_input = Some(input.clone());
        self.detail_open = true;
        self.detail_cell = None;
        self._detail_subscription =
            Some(
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save_detail(window, cx);
                    }
                }),
            );
        cx.notify();
    }

    fn close_detail(&mut self) {
        self.detail_open = false;
        self.detail_input = None;
        self.detail_cell = None;
        self.detail_error = None;
        self._detail_subscription = None;
    }

    /// Switch the inspector between raw text and JSON. Switching to JSON
    /// formats the current text when it parses, and reports the parse error
    /// when it does not, instead of staging anything.
    pub fn set_detail_json(&mut self, json: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.detail_json = json;
        if let Some(input) = self.detail_input.clone() {
            let text = input.read(cx).value().to_string();
            if json {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(value) => {
                        let pretty = serde_json::to_string_pretty(&value).unwrap_or(text);
                        input.update(cx, |state, cx| state.set_value(pretty, window, cx));
                        self.detail_error = None;
                    }
                    Err(e) => self.detail_error = Some(format!("Invalid JSON: {e}")),
                }
            } else {
                self.detail_error = None;
            }
        }
        cx.notify();
    }

    /// Stage the inspector's text back into the grid cell it shows. JSON
    /// mode validates first and stages the formatted text; Text mode parses
    /// like the cell editor. Success resyncs the input from the grid, so a
    /// typed `042` in an integer cell comes back as `42`.
    pub fn save_detail(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(input) = self.detail_input.clone() else {
            return false;
        };
        let text = input.read(cx).value().to_string();
        let table = self.table.read(cx);
        let (row, screen_col) = match table.selection() {
            TableSelection::Cell(r, c) => (r, c),
            _ => return false,
        };
        let d = table.delegate();
        let Some(data_col) = d.data_index(screen_col) else {
            return false;
        };
        let category = d.metas[data_col].data_type.category;
        let value = if self.detail_json {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => Value::Text(serde_json::to_string_pretty(&value).unwrap_or(text)),
                Err(e) => {
                    self.detail_error = Some(format!("Invalid JSON: {e}"));
                    cx.notify();
                    return false;
                }
            }
        } else {
            match parse_cell_text(&text, category) {
                Ok(value) => value,
                Err(message) => {
                    self.detail_error = Some(message);
                    cx.notify();
                    return false;
                }
            }
        };
        let staged = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let id = d.row_id_at(row).ok_or("This row is no longer on screen.")?;
            let baseline = d.rows.get(row).cloned().unwrap_or_default();
            let cs = d.changeset.as_mut().ok_or("Nothing to edit yet.")?;
            cs.edit_cell(&id, &baseline, data_col, CellChange::Set(value))?;
            cx.notify();
            Ok::<(), String>(())
        });
        match staged {
            Ok(()) => {
                self.detail_error = None;
                self.detail_cell = None;
                cx.notify();
                true
            }
            Err(message) => {
                self.detail_error = Some(message);
                cx.notify();
                false
            }
        }
    }

    /// Sync the inspector's input to the selected cell, when the selection
    /// moved since the last sync. Called from `render`, which is the only
    /// place with both the window and the fresh selection; typing in the
    /// inspector does not move the selection, so it never fights the user.
    fn sync_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.detail_open {
            return;
        }
        let table = self.table.read(cx);
        let (row, screen_col) = match table.selection() {
            TableSelection::Cell(r, c) => (r, c),
            _ => {
                self.close_detail();
                return;
            }
        };
        let d = table.delegate();
        let Some(data_col) = d.data_index(screen_col) else {
            self.close_detail();
            return;
        };
        if self.detail_cell == Some((row, data_col)) {
            return;
        }
        let value = d.shown_value(row, data_col);
        let category = d.metas[data_col].data_type.category;
        let text = match &value {
            Value::Null => "NULL".to_owned(),
            Value::Other { type_name, text } if type_name.is_empty() && text == "DEFAULT" => {
                String::new()
            }
            other => copy_text(other),
        };
        let json = matches!(category, TypeCategory::Json);
        let (display, error) = if json {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(parsed) => (serde_json::to_string_pretty(&parsed).unwrap_or(text), None),
                Err(e) => (text, Some(format!("Invalid JSON: {e}"))),
            }
        } else {
            (text, None)
        };
        self.detail_json = json;
        self.detail_error = error;
        self.detail_cell = Some((row, data_col));
        if let Some(input) = self.detail_input.clone() {
            input.update(cx, |state, cx| state.set_value(display, window, cx));
        }
    }

    /// Stage the open edit, then move the selection: `(1, 0)` right, `(-1, 0)` left (wrapping to
    /// the next / previous row), `(0, 1)` down. Stays put at the edge of the grid.
    fn commit_and_move(&mut self, dx: i32, dy: i32, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.table.read(cx).delegate();
        let Some((row, col)) = d
            .editing
            .as_ref()
            .map(|e| (e.row_ix as i32, e.screen_col.saturating_sub(GUTTER) as i32))
        else {
            return;
        };
        let (rows, cols) = (d.total_rows() as i32, d.visible.len() as i32);
        if !self.commit_edit(window, cx) {
            return;
        }
        let (mut r, mut c) = (row + dy, col + dx);
        if c >= cols {
            (r, c) = (r + 1, 0);
        } else if c < 0 {
            (r, c) = (r - 1, cols - 1);
        }
        if (0..rows).contains(&r) && (0..cols).contains(&c) {
            self.select_cell(r as usize, c as usize, cx);
        }
    }

    /// The editor lost the keyboard (a click elsewhere). Keep a valid value; for an invalid one,
    /// close the editor and say it was not saved, rather than leaving a stale box on screen.
    fn finish_on_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_editing(cx) || self.commit_edit(window, cx) {
            return;
        }
        let reason = self.error.take();
        self.cancel_edit(window, cx);
        if let Some(reason) = reason {
            self.fail_edit(format!("Not saved: {reason}"), cx);
        }
    }

    /// Close the editor without staging anything (`Esc`).
    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_open = self.table.update(cx, |t, cx| {
            let was_open = t.delegate_mut().editing.take().is_some();
            cx.notify();
            was_open
        });
        if was_open {
            self._edit_subscription = None;
            self.editor_options.clear();
            self.error = None;
            self.focus_table(window, cx);
            cx.notify();
        }
    }

    fn focus_table(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.table.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Add an empty row after the loaded ones (`⌘I`, the `+ Row` button).
    pub fn insert_row(&mut self, cx: &mut Context<Self>) {
        let added = self.table.update(cx, |t, cx| {
            let result = match t.delegate_mut().changeset.as_mut() {
                Some(cs) => cs.insert_row().map(|_| ()),
                None => Err("The table is still loading.".to_owned()),
            };
            cx.notify();
            result
        });
        match added {
            Ok(()) => {
                self.error = None;
                let last = self.total_rows(cx).saturating_sub(1);
                self.select_cell(last, 0, cx);
            }
            Err(message) => self.fail_edit(message, cx),
        }
    }

    /// Mark the selected row for deletion, or bring it back if it is already marked (`⌘⌫`).
    /// Open the cell action menu for a right-clicked cell, and select that cell so the menu's
    /// actions (which all work on the selection) hit it. The gutter is not a cell: a right-click
    /// there selects the row instead and opens no menu.
    pub fn open_cell_menu(&mut self, row: usize, screen_col: usize, cx: &mut Context<Self>) {
        if screen_col < GUTTER || self.is_editing(cx) {
            return;
        }
        self.table.update(cx, |t, cx| {
            t.set_selected_cell(row, screen_col, cx);
        });
        self.cell_menu = Some((row, screen_col));
        cx.notify();
    }

    /// Close the cell action menu without doing anything.
    pub fn close_cell_menu(&mut self, cx: &mut Context<Self>) {
        self.cell_menu = None;
        cx.notify();
    }

    /// Whether the cell menu is open, and for which cell, for tests.
    pub fn cell_menu_cell(&self) -> Option<(usize, usize)> {
        self.cell_menu
    }

    pub fn toggle_delete_row(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row_index(cx) else {
            return;
        };
        let outcome = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let id = d.row_id_at(row);
            let result = match (id, d.changeset.as_mut()) {
                (Some(id), Some(cs)) => {
                    if cs.row_state(&id) == RowState::Deleted {
                        cs.restore_row(&id);
                        Ok(())
                    } else {
                        cs.delete_row(&id)
                    }
                }
                _ => Err("This row cannot be deleted.".to_owned()),
            };
            cx.notify();
            result
        });
        if let Err(message) = outcome {
            self.fail_edit(message, cx);
        } else {
            cx.notify();
        }
    }

    /// Duplicate the selected row (`⌘D`). A loaded row is copied with its
    /// primary-key columns cleared, so the copy takes its defaults; a staged
    /// new row is copied the same way. The copy shows after the loaded rows.
    pub fn clone_row(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row_index(cx) else {
            return;
        };
        let outcome = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let id = d.row_id_at(row).ok_or("This row is no longer on screen.")?;
            let result = match &id {
                RowId::Existing(_) => {
                    let baseline = d.rows.get(row).cloned().unwrap_or_default();
                    let clear = d
                        .changeset
                        .as_ref()
                        .map(|cs| cs.key_column_indices().to_vec())
                        .unwrap_or_default();
                    d.changeset
                        .as_mut()
                        .ok_or("Nothing to edit yet.")?
                        .clone_row(&baseline, &clear)
                }
                RowId::New(n) => d
                    .changeset
                    .as_mut()
                    .ok_or("Nothing to edit yet.")?
                    .clone_new_row(*n),
            };
            cx.notify();
            result
        });
        match outcome {
            Ok(_) => {
                self.error = None;
                let last = self.total_rows(cx).saturating_sub(1);
                self.select_cell(last, 0, cx);
            }
            Err(message) => self.fail_edit(message, cx),
        }
    }

    /// Stage a value for the selected cell without opening the editor. An
    /// open editor wins: staging underneath it would silently replace what
    /// the user is typing.
    fn stage_selected_cell(&mut self, change: CellChange, cx: &mut Context<Self>) {
        if self.is_editing(cx) {
            self.fail_edit("Finish the open edit first.", cx);
            return;
        }
        let Some((row, data_col)) = self.selected_cell(cx) else {
            return;
        };
        let outcome = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let id = d
                .row_id_at(row)
                .ok_or("This cell is no longer on screen.")?;
            let baseline = d.rows.get(row).cloned().unwrap_or_default();
            let cs = d.changeset.as_mut().ok_or("Nothing to edit yet.")?;
            cs.edit_cell(&id, &baseline, data_col, change)?;
            cx.notify();
            Ok::<(), String>(())
        });
        match outcome {
            Ok(()) => {
                self.error = None;
                cx.notify();
            }
            Err(message) => self.fail_edit(message, cx),
        }
    }

    /// Stage SQL NULL for the selected cell.
    pub fn set_selected_cell_to_null(&mut self, cx: &mut Context<Self>) {
        self.stage_selected_cell(CellChange::Set(Value::Null), cx);
    }

    /// Stage an empty string for the selected cell. Only text-like columns
    /// have an empty value; for other columns this is refused instead of
    /// staging a value the commit would reject.
    pub fn set_selected_cell_to_empty(&mut self, cx: &mut Context<Self>) {
        let Some((_, data_col)) = self.selected_cell(cx) else {
            return;
        };
        let category = self
            .table
            .read(cx)
            .delegate()
            .metas
            .get(data_col)
            .map(|m| m.data_type.category);
        if !matches!(
            category,
            Some(TypeCategory::Text | TypeCategory::Json | TypeCategory::Enum | TypeCategory::Set)
        ) {
            self.fail_edit("Empty is only for text columns.", cx);
            return;
        }
        self.stage_selected_cell(CellChange::Set(Value::Text(String::new())), cx);
    }

    /// Stage the column default for the selected cell. On insert the column
    /// is omitted; on update it compiles to `SET c = DEFAULT` where the
    /// dialect has it, and the commit refuses it where it does not.
    pub fn set_selected_cell_to_default(&mut self, cx: &mut Context<Self>) {
        self.stage_selected_cell(CellChange::Default, cx);
    }

    /// Put the selected cell back to its loaded value, staging nothing for
    /// it. A cell with no staged edit is left alone.
    pub fn revert_selected_cell(&mut self, cx: &mut Context<Self>) {
        if self.is_editing(cx) {
            self.fail_edit("Finish the open edit first.", cx);
            return;
        }
        let Some((row, data_col)) = self.selected_cell(cx) else {
            return;
        };
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            if let (Some(id), Some(cs)) = (d.row_id_at(row), d.changeset.as_mut()) {
                cs.revert_cell(&id, data_col);
            }
            cx.notify();
        });
        self.error = None;
        cx.notify();
    }

    pub fn undo_edit(&mut self, cx: &mut Context<Self>) {
        self.step_history(cx, Changeset::undo);
    }

    pub fn redo_edit(&mut self, cx: &mut Context<Self>) {
        self.step_history(cx, Changeset::redo);
    }

    fn step_history(&mut self, cx: &mut Context<Self>, step: fn(&mut Changeset) -> bool) {
        self.table.update(cx, |t, cx| {
            if let Some(cs) = t.delegate_mut().changeset.as_mut() {
                step(cs);
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Ask before throwing every staged change away: the first click only
    /// opens the confirmation strip, the second actually discards.
    pub fn discard_changes(&mut self, cx: &mut Context<Self>) {
        if self.pending(cx).total() == 0 {
            return;
        }
        self.confirm_discard = true;
        cx.notify();
    }

    /// Discard after the second click. Still undoable, like any other step.
    pub fn confirm_discard_now(&mut self, cx: &mut Context<Self>) {
        self.confirm_discard = false;
        self.editor_options.clear();
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.editing = None;
            if let Some(cs) = d.changeset.as_mut() {
                cs.discard_all();
            }
            cx.notify();
        });
        self.preview_open = false;
        self.error = None;
        cx.notify();
    }

    pub fn cancel_discard(&mut self, cx: &mut Context<Self>) {
        self.confirm_discard = false;
        cx.notify();
    }

    /// Which engine the table is on, so the changes can be shown in its dialect.
    pub fn set_engine(&mut self, kind: DbKind, cx: &mut Context<Self>) {
        self.engine = Some(kind);
        cx.notify();
    }

    /// The statements a commit would run, with the values written in. For showing only: what is
    /// executed uses bound parameters.
    pub fn preview_statements(&self, cx: &App) -> Result<Vec<String>, String> {
        let kind = self
            .engine
            .ok_or("The engine of this table is not known yet.")?;
        let dialect = dialect_for(kind).ok_or("This engine has no SQL dialect here.")?;
        let changes = self.changes(cx);
        compile_changes(self.query.path(), &changes, dialect)
            .map(|statements| statements.into_iter().map(|s| s.preview).collect())
            .map_err(|e| e.to_string())
    }

    pub fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_open = !self.preview_open;
        cx.notify();
    }

    pub fn preview_open(&self) -> bool {
        self.preview_open
    }

    /// Ask the owner to commit (`⌘S`). An open editor is staged first, so what is on screen is
    /// what gets committed.
    /// The workspace tells every table tab the guard level, so a batch
    /// commit can ask first when the level says so.
    pub fn set_safe_mode(&mut self, mode: SafeMode, cx: &mut Context<Self>) {
        self.safe_mode = mode;
        cx.notify();
    }

    pub fn request_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_editing(cx) && !self.commit_edit(window, cx) {
            return; // the value was rejected; the message is showing
        }
        if self.committing || self.pending(cx).total() == 0 {
            return;
        }
        // Alert and Strict both confirm a batch write before it runs; Normal
        // does not, and ReadOnly is refused by the commit itself instead.
        if matches!(self.safe_mode, SafeMode::Alert | SafeMode::Strict) && !self.confirm_commit {
            self.confirm_commit = true;
            cx.notify();
            return;
        }
        self.confirm_commit = false;
        self.committing = true;
        self.error = None;
        cx.emit(TableTabEvent::CommitRequested);
        cx.notify();
    }

    pub fn cancel_commit(&mut self, cx: &mut Context<Self>) {
        self.confirm_commit = false;
        cx.notify();
    }

    /// The owner's answer to [`TableTabEvent::CommitRequested`]. On success the staged changes
    /// are gone and the page is reloaded; on failure they are kept so nothing typed is lost.
    pub fn commit_finished(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        self.committing = false;
        match result {
            Ok(()) => {
                self.table.update(cx, |t, cx| {
                    let d = t.delegate_mut();
                    d.changeset = None;
                    d.rebuild_changeset();
                    cx.notify();
                });
                self.preview_open = false;
                // Rows were added or removed: the count has to be asked for again.
                self.query.page.set_total(None);
                cx.emit(TableTabEvent::Reload);
            }
            Err(message) => {
                self.error = Some(message.clone().into());
                self.locate_failed_change(&message, cx);
            }
        }
        cx.notify();
    }

    /// Move the selection to the row a failed commit named, when it is on
    /// screen. The service names the failing change 1-based (`change 3: …`)
    /// in the same delete → update → insert order as [`Self::changes`], so
    /// the user lands on the row to fix instead of hunting for it.
    fn locate_failed_change(&mut self, message: &str, cx: &mut Context<Self>) {
        let Some(number) = parse_change_number(message) else {
            return;
        };
        let changes = self.changes(cx);
        let Some(change) = changes.get(number.wrapping_sub(1)) else {
            return;
        };
        let row = self
            .table
            .read(cx)
            .delegate()
            .failed_change_row(change, &changes, number);
        if let Some(row) = row {
            self.select_cell(row, 0, cx);
        }
    }

    pub fn is_committing(&self) -> bool {
        self.committing
    }

    // ---- user actions: change the query, ask for a reload ------------------------------------

    pub fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.query.page.next_page() {
            cx.emit(TableTabEvent::Reload);
        }
    }

    pub fn prev_page(&mut self, cx: &mut Context<Self>) {
        if self.query.page.prev_page() {
            cx.emit(TableTabEvent::Reload);
        }
    }

    pub fn first_page(&mut self, cx: &mut Context<Self>) {
        if self.query.page.has_prev() {
            self.query.page.first();
            cx.emit(TableTabEvent::Reload);
        }
    }

    pub fn last_page(&mut self, cx: &mut Context<Self>) {
        if self.query.page.last() {
            cx.emit(TableTabEvent::Reload);
        }
    }

    /// One entry point for the shared pager widget, which knows only which button was pressed.
    fn navigate(&mut self, nav: super::cells::PageNav, cx: &mut Context<Self>) {
        match nav {
            super::cells::PageNav::First => self.first_page(cx),
            super::cells::PageNav::Prev => self.prev_page(cx),
            super::cells::PageNav::Next => self.next_page(cx),
            super::cells::PageNav::Last => self.last_page(cx),
        }
    }

    pub fn set_page_size(&mut self, size: u32, cx: &mut Context<Self>) {
        if size != self.query.page.size() {
            self.query.page.set_size(size);
            cx.emit(TableTabEvent::Reload);
        }
    }

    /// Sort by `column`; with `additive` (Shift-click) it joins the existing keys instead of
    /// replacing them. The header already shows each key's priority once there are several.
    pub fn toggle_sort(&mut self, column: &str, additive: bool, cx: &mut Context<Self>) {
        self.query.toggle_sort(column, additive);
        self.sync_sort(cx);
        cx.emit(TableTabEvent::Reload);
    }

    /// The text `⌘C` would copy: a cell's value, a whole row (tab-separated), or a column of this
    /// page (one value per line). `None` when nothing is selected.
    pub fn selection_text(&self, cx: &App) -> Option<String> {
        let table = self.table.read(cx);
        let d = table.delegate();
        match table.selection() {
            TableSelection::Cell(r, c) => d.rows.get(r)?.get(d.data_index(c)?).map(copy_text),
            TableSelection::Row(r) => {
                // What is copied is what is shown: hidden columns stay out.
                let row = d.rows.get(r)?;
                let shown: Vec<String> = d
                    .visible
                    .iter()
                    .filter_map(|&i| row.get(i))
                    .map(copy_text)
                    .collect();
                Some(to_tsv(&[shown]))
            }
            TableSelection::Column(c) => {
                let c = d.data_index(c)?;
                let lines: Vec<Vec<String>> = d
                    .rows
                    .iter()
                    .filter_map(|r| r.get(c).map(|v| vec![copy_text(v)]))
                    .collect();
                Some(to_tsv(&lines))
            }
            TableSelection::None => None,
        }
    }

    /// Select a cell; `col` counts the data columns on screen (the row-number gutter is not one).
    pub fn select_cell(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.table
            .update(cx, |t, cx| t.set_selected_cell(row, col + GUTTER, cx));
        cx.notify();
    }

    pub fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
        cx.notify();
    }

    pub fn select_column(&mut self, col: usize, cx: &mut Context<Self>) {
        self.table
            .update(cx, |t, cx| t.set_selected_col(col + GUTTER, cx));
        cx.notify();
    }

    /// Put the current selection on the clipboard (what `⌘C` does).
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Columns that can be followed to another table (from the table's foreign keys).
    pub fn set_foreign_keys(&mut self, targets: Vec<JumpTarget>, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            t.delegate_mut().jumps = targets.into_iter().map(|j| (j.column.clone(), j)).collect();
            t.refresh(cx);
        });
    }

    /// The tables referencing this one, for the "referenced by" menu on
    /// primary-key cells. Set by the controller alongside the forward jumps.
    pub fn set_back_references(&mut self, refs: Vec<BackRef>, cx: &mut Context<Self>) {
        self.back_references = refs;
        cx.notify();
    }

    pub fn has_foreign_key(&self, column: &str, cx: &App) -> bool {
        self.table.read(cx).delegate().jumps.contains_key(column)
    }

    fn follow_foreign_key(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        let d = self.table.read(cx).delegate();
        let Some(meta) = d.metas.get(col) else { return };
        let Some(target) = d.jumps.get(&meta.name) else {
            return;
        };
        let Some(value) = d.rows.get(row).and_then(|r| r.get(col)).cloned() else {
            return;
        };
        cx.emit(TableTabEvent::FollowForeignKey {
            table: target.table.clone(),
            filter: Filter {
                column: target.referenced_column.clone(),
                op: FilterOp::Eq,
                values: vec![value],
                enabled: true,
            },
        });
    }

    /// Show only the rows matching `filter` (used when arriving through a foreign key). If the
    /// tab's saved view state has not been restored yet, the filter is applied right after it.
    pub fn apply_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.apply_filters(vec![filter], cx);
    }

    /// Show only the rows matching every filter (used when arriving through
    /// a back-reference, where one key column means one filter). Same
    /// pending rule as [`TableTab::apply_filter`].
    pub fn apply_filters(&mut self, filters: Vec<Filter>, cx: &mut Context<Self>) {
        if !self.restored {
            self.pending_filter = Some(filters);
            return;
        }
        self.query.set_filters(filters, None);
        let draft = FilterDraft::from_applied(&self.query.filters, None);
        self.drawer.update(cx, |d, cx| d.load(draft, cx));
        cx.emit(TableTabEvent::Reload);
        cx.notify();
    }

    // ---- column visibility ---------------------------------------------------------------------

    /// Hidden column names, sorted.
    pub fn hidden_columns(&self, cx: &App) -> Vec<String> {
        let mut names: Vec<String> = self
            .table
            .read(cx)
            .delegate()
            .hidden
            .iter()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// `(shown, total)` for the `Columns (5/8)` label.
    pub fn column_counts(&self, cx: &App) -> (usize, usize) {
        let d = self.table.read(cx).delegate();
        (d.visible.len(), d.metas.len())
    }

    /// Replace the hidden set without announcing it (restoring what was saved).
    pub fn restore_hidden_columns(&mut self, names: Vec<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.hidden = names.into_iter().collect();
            d.rebuild_columns();
            t.refresh(cx);
        });
        cx.notify();
    }

    /// Show or hide one column. The last visible column cannot be hidden.
    pub fn toggle_column(&mut self, name: &str, cx: &mut Context<Self>) {
        let changed = self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            if d.hidden.contains(name) {
                d.hidden.remove(name);
            } else if d.visible.len() > 1 {
                d.hidden.insert(name.to_owned());
            } else {
                return false;
            }
            d.rebuild_columns();
            t.refresh(cx);
            true
        });
        if changed {
            cx.emit(TableTabEvent::ViewChanged);
            cx.notify();
        }
    }

    pub fn show_all_columns(&mut self, cx: &mut Context<Self>) {
        if !self.hidden_columns(cx).is_empty() {
            self.restore_hidden_columns(Vec::new(), cx);
            cx.emit(TableTabEvent::ViewChanged);
        }
    }

    pub fn toggle_columns_panel(&mut self, cx: &mut Context<Self>) {
        self.columns_open = !self.columns_open;
        cx.notify();
    }

    pub fn columns_panel_open(&self) -> bool {
        self.columns_open
    }

    pub fn toggle_paging_panel(&mut self, cx: &mut Context<Self>) {
        self.paging_open = !self.paging_open;
        cx.notify();
    }

    pub fn paging_panel_open(&self) -> bool {
        self.paging_open
    }

    /// Everything remembered per table: query state plus hidden columns.
    pub fn view_state(&self, cx: &App) -> TableViewState {
        self.query.to_view_state(TableViewState {
            hidden_columns: self.hidden_columns(cx),
            ..TableViewState::default()
        })
    }

    pub fn toggle_filters(&mut self, cx: &mut Context<Self>) {
        self.drawer_open = !self.drawer_open;
        cx.notify();
    }

    pub fn filters_open(&self) -> bool {
        self.drawer_open
    }

    pub fn filter_drawer(&self) -> &Entity<FilterDrawer> {
        &self.drawer
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.emit(TableTabEvent::Reload);
    }

    fn sync_sort(&mut self, cx: &mut Context<Self>) {
        let sort = self.query.sort.clone();
        self.table.update(cx, |t, cx| {
            t.delegate_mut().sort = sort;
            t.delegate_mut().rebuild_columns();
            t.refresh(cx);
        });
    }

    // ---- results: fed in by the controller, tagged with the generation they belong to ----------

    /// Start a load. Everything fed back must carry the returned generation.
    pub fn begin_load(&mut self, cx: &mut Context<Self>) -> u64 {
        self.generation += 1;
        self.loading = true;
        self.error = None;
        self.elapsed_ms = None;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().loading = true;
            cx.notify();
        });
        cx.notify();
        self.generation
    }

    /// New column set; the old rows are dropped (they belong to the previous result).
    pub fn set_columns(
        &mut self,
        generation: u64,
        columns: Vec<ColumnMeta>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.loaded_rows = 0;
        self.query.page.set_loaded(0);
        self.drawer
            .update(cx, |d, cx| d.set_columns(columns.clone(), cx));
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.metas = columns;
            d.rows.clear();
            d.editing = None;
            d.sort = SortState::from_keys(Vec::new());
            d.rebuild_changeset();
            d.rebuild_columns();
            t.refresh(cx);
        });
        self.sync_sort(cx);
    }

    pub fn append_rows(&mut self, generation: u64, rows: Vec<Vec<Value>>, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.loaded_rows += rows.len();
        self.query.page.set_loaded(self.loaded_rows);
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.rows.extend(rows);
            d.loading = false;
            cx.notify();
        });
        cx.notify();
    }

    pub fn finish(&mut self, generation: u64, elapsed_ms: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        self.elapsed_ms = Some(elapsed_ms);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().loading = false;
            cx.notify();
        });
        cx.notify();
    }

    /// Hand over the prefetched page for `page` under `key`, if one is waiting. Anything else
    /// (another query, another page) is stale and dropped, never served.
    pub fn take_prefetch(&mut self, key: &TableQuery, page: u64) -> Option<PrefetchedPage> {
        let prefetched = self.prefetched.take()?;
        if prefetched.key == *key && prefetched.page == page {
            Some(prefetched)
        } else {
            None
        }
    }

    /// Park a page fetched ahead. It is only served when the tab is still on the page before
    /// it under the same query; the controller checks that before storing.
    pub fn set_prefetch(&mut self, page: PrefetchedPage) {
        self.prefetched = Some(page);
    }

    pub fn fail(&mut self, generation: u64, message: String, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        self.error = Some(message.into());
        self.table.update(cx, |t, cx| {
            t.delegate_mut().loading = false;
            cx.notify();
        });
        cx.notify();
    }

    pub fn set_total(&mut self, generation: u64, total: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.query.page.set_total(Some(total));
        cx.notify();
    }
}

/// `⌘S` on macOS, `Ctrl+S` elsewhere.
fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

impl TableTab {
    /// "3 unsaved changes · 1 edited · 1 new · 1 deleted"
    fn pending_summary(pending: Counts, is_zh: bool) -> String {
        let parts = [
            (pending.modified_rows, "已修改", "edited"),
            (pending.inserted_rows, "新增", "new"),
            (pending.deleted_rows, "待删除", "deleted"),
        ];
        let detail: Vec<String> = parts
            .iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, zh, en)| {
                if is_zh {
                    format!("{n} 行{zh}")
                } else {
                    format!("{n} {en}")
                }
            })
            .collect();
        let total = pending.total();
        let head = if is_zh {
            format!("{total} 项未保存的更改")
        } else if total == 1 {
            "1 unsaved change".to_owned()
        } else {
            format!("{total} unsaved changes")
        };
        format!("{head} · {}", detail.join(" · "))
    }

    /// The strip above the status bar that says there is unsaved work, and offers to save it.
    fn pending_bar(&self, pending: Counts, is_zh: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let summary = Self::pending_summary(pending, is_zh);
        let saving = self.committing;
        h_flex()
            .id("pending-bar")
            .test_support()
            .aria_label(summary.clone())
            .w_full()
            .flex_shrink_0()
            .gap_3()
            .px_3()
            .py_1p5()
            .items_center()
            .border_t_1()
            .border_color(theme.warning.opacity(0.5))
            .bg(theme.warning.opacity(0.14))
            .child(div().text_color(theme.warning).child("●"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(summary),
            )
            .child(
                Button::new("preview-sql")
                    .label(format!(
                        "{} {}",
                        if is_zh { "预览 SQL" } else { "Preview SQL" },
                        shortcut("⇧P")
                    ))
                    .small()
                    .when(self.preview_open, |b| b.primary())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_preview(cx))),
            )
            .child(
                Button::new("discard-changes")
                    .label(if is_zh { "放弃" } else { "Discard" })
                    .small()
                    .disabled(saving)
                    .on_click(cx.listener(|this, _, _, cx| this.discard_changes(cx))),
            )
            .child(
                Button::new("commit-changes")
                    .label(if saving {
                        (if is_zh { "保存中…" } else { "Saving…" }).to_owned()
                    } else {
                        format!("{} {}", if is_zh { "保存" } else { "Save" }, shortcut("S"))
                    })
                    .primary()
                    .small()
                    .disabled(saving)
                    .on_click(cx.listener(|this, _, window, cx| this.request_commit(window, cx))),
            )
            .into_any_element()
    }

    /// The second click for Discard: restate what is about to be lost.
    fn confirm_discard_bar(
        &self,
        pending: Counts,
        is_zh: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let summary = Self::pending_summary(pending, is_zh);
        h_flex()
            .id("confirm-discard")
            .test_support()
            .aria_label(summary.clone())
            .w_full()
            .flex_shrink_0()
            .gap_3()
            .px_3()
            .py_1p5()
            .items_center()
            .border_t_1()
            .border_color(theme.danger.opacity(0.5))
            .bg(theme.danger.opacity(0.14))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(if is_zh {
                        format!("确认放弃以下更改？{summary}")
                    } else {
                        format!("Discard these changes? {summary}")
                    }),
            )
            .child(
                Button::new("confirm-discard-yes")
                    .label(if is_zh { "确认放弃" } else { "Discard all" })
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_discard_now(cx))),
            )
            .child(
                Button::new("confirm-discard-no")
                    .label(if is_zh { "保留" } else { "Keep changes" })
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_discard(cx))),
            )
            .into_any_element()
    }

    /// The batch confirmation for Alert/Strict: restate the batch, then Save
    /// for real or keep editing.
    fn confirm_commit_bar(
        &self,
        pending: Counts,
        is_zh: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let summary = Self::pending_summary(pending, is_zh);
        h_flex()
            .id("confirm-commit")
            .test_support()
            .aria_label(summary.clone())
            .w_full()
            .flex_shrink_0()
            .gap_3()
            .px_3()
            .py_1p5()
            .items_center()
            .border_t_1()
            .border_color(theme.warning.opacity(0.5))
            .bg(theme.warning.opacity(0.14))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(if is_zh {
                        format!("确认提交以下更改？{summary}")
                    } else {
                        format!("Commit these changes? {summary}")
                    }),
            )
            .child(
                Button::new("confirm-commit-yes")
                    .label(if is_zh { "确认提交" } else { "Commit" })
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.request_commit(window, cx))),
            )
            .child(
                Button::new("confirm-commit-no")
                    .label(if is_zh { "再想想" } else { "Keep editing" })
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_commit(cx))),
            )
            .into_any_element()
    }

    /// The SQL a save would run, in the order it runs, inside one transaction.
    fn preview_panel(&self, is_zh: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (lines, error) = match self.preview_statements(cx) {
            Ok(lines) => (lines, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let label = lines.join("\n");
        v_flex()
            .id("preview-panel")
            .test_support()
            .aria_label(label)
            .w_full()
            .flex_shrink_0()
            .max_h(px(220.))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(if is_zh {
                        "保存时在同一个事务里按顺序执行:"
                    } else {
                        "Runs in one transaction, in this order:"
                    }),
            )
            .child(
                v_flex()
                    .id("preview-scroll")
                    .overflow_y_scroll()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(px(12.))
                    .children(lines.into_iter().map(|line| div().child(line)))
                    .when_some(error, |d, e| {
                        d.child(div().text_color(theme.danger).child(e))
                    }),
            )
            .into_any_element()
    }
}

impl Render for TableTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_detail(window, cx);
        let theme = cx.theme().clone();
        let page = &self.query.page;
        let (has_prev, has_next) = (page.has_prev(), page.has_next());
        // While a page is on its way the old rows are still on screen, so the range label
        // (already pointing at the new page) would describe the wrong rows.
        let status = if self.loading {
            "Loading…".to_owned()
        } else {
            match self.elapsed_ms {
                Some(ms) => format!("{} · {ms} ms", page.label()),
                None => page.label(),
            }
        };
        let page_size = page.size();
        let pending = self.pending(cx);
        let pending_bar = (pending.total() > 0)
            .then(|| self.pending_bar(pending, crate::views::style::is_zh_locale(), cx));
        let confirm_discard_bar = (pending.total() > 0 && self.confirm_discard)
            .then(|| self.confirm_discard_bar(pending, crate::views::style::is_zh_locale(), cx));
        let confirm_commit_bar = (pending.total() > 0 && self.confirm_commit)
            .then(|| self.confirm_commit_bar(pending, crate::views::style::is_zh_locale(), cx));
        let detail_panel = self.detail_input.clone().and_then(|input| {
            if !self.detail_open {
                return None;
            }
            let is_zh = crate::views::style::is_zh_locale();
            let column = match self.table.read(cx).selection() {
                TableSelection::Cell(_, screen_col) => self
                    .table
                    .read(cx)
                    .delegate()
                    .data_index(screen_col)
                    .and_then(|i| {
                        self.table
                            .read(cx)
                            .delegate()
                            .metas
                            .get(i)
                            .map(|m| m.name.clone())
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let error = self.detail_error.clone();
            let json = self.detail_json;
            let panel = v_flex()
                .id("row-detail")
                .test_support()
                .w_full()
                .flex_shrink_0()
                .gap_2()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .child(if is_zh {
                                    format!("行详情：{column}")
                                } else {
                                    format!("Row detail: {column}")
                                }),
                        )
                        .child(
                            Button::new("detail-mode-text")
                                .label("Text")
                                .small()
                                .when(!json, |b| b.primary())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_detail_json(false, window, cx);
                                })),
                        )
                        .child(
                            Button::new("detail-mode-json")
                                .label("JSON")
                                .small()
                                .when(json, |b| b.primary())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_detail_json(true, window, cx);
                                })),
                        )
                        .child(
                            Button::new("detail-save")
                                .label(if is_zh { "应用" } else { "Apply" })
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_detail(window, cx);
                                })),
                        ),
                )
                .child(
                    div().w_full().child(
                        Input::new(&input)
                            .id("detail-input")
                            .appearance(false)
                            .bordered(false),
                    ),
                )
                .when_some(error, |d, e| {
                    d.child(
                        div()
                            .id("detail-error")
                            .test_support()
                            .aria_label(e.clone())
                            .text_size(px(13.))
                            .text_color(theme.danger)
                            .child(e),
                    )
                })
                .into_any_element();
            Some(panel)
        });
        let cell_menu_panel = self.cell_menu.map(|(row, screen_col)| {
            let is_zh = crate::views::style::is_zh_locale();
            let column = self
                .table
                .read(cx)
                .delegate()
                .data_index(screen_col)
                .and_then(|i| {
                    self.table
                        .read(cx)
                        .delegate()
                        .metas
                        .get(i)
                        .map(|m| m.name.clone())
                })
                .unwrap_or_default();
            let label = if is_zh {
                format!("单元格：第 {} 行 · {column}", row + 1)
            } else {
                format!("Cell: row {} · {column}", row + 1)
            };
            let button = |id: &'static str, label: String| Button::new(id).label(label).small();
            // Back-references, only on a primary-key cell: every table
            // referencing this one, each opening filtered to this row's key.
            // Filters resolve now, while the row is on screen; a key whose
            // columns do not line up offers no button.
            let backref_buttons: Vec<AnyElement> = {
                let delegate = self.table.read(cx).delegate();
                let pk = delegate.key_columns.clone();
                if pk.contains(&column) && !self.back_references.is_empty() {
                    let metas: Vec<String> =
                        delegate.metas.iter().map(|m| m.name.clone()).collect();
                    let row_values: Vec<Value> =
                        delegate.rows.get(row).cloned().unwrap_or_default();
                    self.back_references
                        .iter()
                        .enumerate()
                        .filter_map(|(i, backref)| {
                            let ref_cols = if backref.referenced_columns.is_empty() {
                                pk.clone()
                            } else {
                                backref.referenced_columns.clone()
                            };
                            if ref_cols.len() != backref.from_columns.len() {
                                return None;
                            }
                            let mut filters = Vec::new();
                            for (from_col, ref_col) in
                                backref.from_columns.iter().zip(ref_cols.iter())
                            {
                                let ix = metas.iter().position(|m| m == ref_col)?;
                                let value = row_values.get(ix)?.clone();
                                filters.push(Filter {
                                    column: from_col.clone(),
                                    op: FilterOp::Eq,
                                    values: vec![value],
                                    enabled: true,
                                });
                            }
                            let table = backref.from.clone();
                            let id = format!("cell-menu-backref-{i}-{}", backref.from.name);
                            Some(
                                Button::new(SharedString::from(id))
                                    .label(format!("Referenced by {}", backref.from.name))
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.close_cell_menu(cx);
                                        cx.emit(TableTabEvent::FollowBackReference {
                                            table: table.clone(),
                                            filters: filters.clone(),
                                        });
                                    }))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            };
            v_flex()
                .id("cell-menu")
                .test_support()
                .gap_2()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_wrap()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .child(label),
                        )
                        .child(
                            button(
                                "cell-menu-null",
                                if is_zh {
                                    "设为 NULL".into()
                                } else {
                                    "Set NULL".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_selected_cell_to_null(cx);
                                this.close_cell_menu(cx);
                            })),
                        )
                        .child(
                            button(
                                "cell-menu-empty",
                                if is_zh {
                                    "设为空串".into()
                                } else {
                                    "Set empty".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_selected_cell_to_empty(cx);
                                this.close_cell_menu(cx);
                            })),
                        )
                        .child(
                            button(
                                "cell-menu-default",
                                if is_zh {
                                    "设为默认值".into()
                                } else {
                                    "Set default".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_selected_cell_to_default(cx);
                                this.close_cell_menu(cx);
                            })),
                        )
                        .child(
                            button(
                                "cell-menu-revert",
                                if is_zh {
                                    "还原".into()
                                } else {
                                    "Revert".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.revert_selected_cell(cx);
                                this.close_cell_menu(cx);
                            })),
                        )
                        .child(
                            button(
                                "cell-menu-clone",
                                if is_zh {
                                    "克隆行".into()
                                } else {
                                    "Clone row".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clone_row(cx);
                                this.close_cell_menu(cx);
                            })),
                        )
                        .child(
                            button(
                                "cell-menu-delete",
                                if is_zh {
                                    "删除 / 恢复行".into()
                                } else {
                                    "Delete / restore row".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_delete_row(cx);
                                this.close_cell_menu(cx);
                            })),
                        )
                        .children(backref_buttons)
                        .child(
                            button(
                                "cell-menu-close",
                                if is_zh {
                                    "关闭".into()
                                } else {
                                    "Close".into()
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_cell_menu(cx);
                            })),
                        ),
                )
                .into_any_element()
        });
        let editor_options_panel =
            (!self.editor_options.is_empty() && self.is_editing(cx)).then(|| {
                let is_zh = crate::views::style::is_zh_locale();
                let label = if self.editor_options_are_allowed {
                    if is_zh { "可选值" } else { "Values" }
                } else if is_zh {
                    "本页已有的值（也可直接输入其他值）"
                } else {
                    "Values on this page (type any other value)"
                };
                let buttons: Vec<AnyElement> = self
                    .editor_options
                    .iter()
                    .enumerate()
                    .map(|(i, option)| {
                        Button::new(SharedString::from(format!("cell-option-{i}")))
                            .label(option.clone())
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.pick_editor_option(i, window, cx);
                            }))
                            .into_any_element()
                    })
                    .collect();
                h_flex()
                    .id("cell-options")
                    .test_support()
                    .w_full()
                    .flex_shrink_0()
                    .flex_wrap()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .items_center()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .children(buttons)
                    .into_any_element()
            });
        let preview_panel = (pending.total() > 0 && self.preview_open)
            .then(|| self.preview_panel(crate::views::style::is_zh_locale(), cx));
        let filter_count = self.query.active_filter_count();
        let (shown, total) = self.column_counts(cx);
        let is_zh = crate::views::style::is_zh_locale();
        let selected_info = self.selected_row_index(cx).map(|sel| {
            let total_str = page
                .total()
                .map(|t| t.to_string())
                .unwrap_or_else(|| self.loaded_rows.to_string());
            if is_zh {
                format!("第 {} 行（共 {} 行）", sel + 1, total_str)
            } else {
                format!("{} of {} rows selected", sel + 1, total_str)
            }
        });
        let display_status = selected_info.unwrap_or_else(|| status.clone());

        let filter_label = match (is_zh, filter_count) {
            (true, 0) => "过滤器".to_owned(),
            (true, n) => format!("过滤器 ({n})"),
            (false, 0) => "Filter".to_owned(),
            (false, n) => format!("Filters ({n})"),
        };
        // Like TablePlus the button just says "Columns"; the count shows only when some are hidden.
        let columns_label = match (is_zh, shown < total) {
            (true, false) => "列".to_owned(),
            (true, true) => format!("列 ({shown}/{total})"),
            (false, false) => "Columns".to_owned(),
            (false, true) => format!("Columns ({shown}/{total})"),
        };
        let pager_state = super::cells::PagerState {
            has_prev,
            has_next,
            has_total: page.total().is_some(),
        };
        let on_nav: fn(&mut Self, super::cells::PageNav, &mut Context<Self>) =
            |this, nav, cx| this.navigate(nav, cx);

        // First / last page and the page size, behind the ⚙ between the arrows.
        let paging_panel = self.paging_open.then(|| {
            let sizes =
                super::cells::page_size_buttons(cx, &PAGE_SIZES, page_size, |this, n, cx| {
                    this.set_page_size(n, cx)
                });
            h_flex()
                .id("paging-panel")
                .test_support()
                .gap_2()
                .px_3()
                .py_1p5()
                .items_center()
                .justify_end()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.background)
                .text_color(theme.muted_foreground)
                .child(if is_zh {
                    "每页行数"
                } else {
                    "Rows per page"
                })
                .child(sizes)
                .child(div().w_2())
                .child(super::cells::page_button(
                    cx,
                    "grid",
                    pager_state,
                    super::cells::PageNav::First,
                    on_nav,
                ))
                .child(super::cells::page_button(
                    cx,
                    "grid",
                    pager_state,
                    super::cells::PageNav::Last,
                    on_nav,
                ))
        });

        let mode = self.mode;
        // A segment of the Data | Structure | Index switch.
        let segment = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .test_support()
                .px_2p5()
                .h(px(20.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .when(active, |d| {
                    d.bg(theme.primary)
                        .text_color(theme.primary_foreground)
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!active, |d| {
                    d.text_color(theme.foreground)
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.secondary_hover))
                })
                .child(label)
        };

        // The column list: one checkbox per column of the result.
        let column_panel = self.columns_open.then(|| {
            let hidden = self.table.read(cx).delegate().hidden.clone();
            let boxes: Vec<AnyElement> = self
                .table
                .read(cx)
                .delegate()
                .metas
                .iter()
                .map(|m| {
                    let name = m.name.clone();
                    Checkbox::new(SharedString::from(format!("col-visible-{name}")))
                        .label(name.clone())
                        .checked(!hidden.contains(&name))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_column(&name, cx)))
                        .into_any_element()
                })
                .collect();
            v_flex()
                .id("columns-panel")
                .test_support()
                .gap_2()
                .p_3()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(h_flex().flex_wrap().gap_x_4().gap_y_1().children(boxes))
                .child(
                    h_flex().child(
                        Button::new("columns-show-all")
                            .label("Show all")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.show_all_columns(cx))),
                    ),
                )
        });

        v_flex()
            .size_full()
            .key_context(GRID_CONTEXT)
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.copy_selection(cx)))
            .on_action(
                cx.listener(|this, _: &CommitChanges, window, cx| this.request_commit(window, cx)),
            )
            .on_action(cx.listener(|this, _: &UndoEdit, _, cx| {
                if this.mode != TabMode::Data {
                    this.undo_structure(cx);
                } else {
                    this.undo_edit(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RedoEdit, _, cx| {
                if this.mode != TabMode::Data {
                    this.redo_structure(cx);
                } else {
                    this.redo_edit(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &BeginEdit, window, cx| this.begin_edit(window, cx)))
            .on_action(
                cx.listener(|this, _: &CancelCellEdit, window, cx| this.cancel_edit(window, cx)),
            )
            .on_action(cx.listener(|this, _: &PreviewChanges, _, cx| this.toggle_preview(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let k = &event.keystroke;
                let plain = !k.modifiers.control && !k.modifiers.platform && !k.modifiers.alt;
                let typed = k.key_char.as_deref().filter(|c| {
                    let mut chars = c.chars();
                    matches!(
                        (chars.next(), chars.next()),
                        (Some(ch), None) if !ch.is_control() && ch != ' '
                    )
                });
                if let (true, Some(typed)) = (plain, typed)
                    && this.type_to_edit(typed, window, cx)
                {
                    // The key is spent: the platform must not also insert it into the new box.
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &InsertRow, _, cx| this.insert_row(cx)))
            .on_action(cx.listener(|this, _: &ToggleDeleteRow, _, cx| this.toggle_delete_row(cx)))
            .on_action(cx.listener(|this, _: &CloneRow, _, cx| this.clone_row(cx)))
            .on_action(
                cx.listener(|this, _: &SetCellNull, _, cx| this.set_selected_cell_to_null(cx)),
            )
            .on_action(
                cx.listener(|this, _: &SetCellEmpty, _, cx| this.set_selected_cell_to_empty(cx)),
            )
            .on_action(
                cx.listener(|this, _: &SetCellDefault, _, cx| {
                    this.set_selected_cell_to_default(cx)
                }),
            )
            .on_action(cx.listener(|this, _: &RevertCell, _, cx| this.revert_selected_cell(cx)))
            .on_action(cx.listener(|this, _: &ToggleRowDetail, window, cx| {
                this.toggle_detail(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToData, _, cx| {
                this.set_mode(TabMode::Data, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchToStructure, window, cx| {
                this.set_mode(TabMode::Structure, cx);
                this.ensure_structure_forms(window, cx);
            }))
            .bg(theme.background)
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .id("grid-error")
                        .test_support()
                        .aria_label(e.clone())
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(e),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(if self.mode != TabMode::Data {
                        self.structure_panel(cx)
                    } else {
                        DataTable::new(&self.table)
                            .with_size(px(22.))
                            .bordered(false)
                            .fixed_cols_border(false)
                            .header_row_height(px(24.))
                            .stripe(true)
                            .into_any_element()
                    }),
            )
            .when(self.drawer_open, |d| d.child(self.drawer.clone()))
            .children(column_panel)
            .children(preview_panel)
            .children(pending_bar)
            .children(confirm_commit_bar)
            .children(confirm_discard_bar)
            .children(editor_options_panel)
            .children(detail_panel)
            .children(cell_menu_panel)
            .children(paging_panel)
            .child(
                // Status bar, as TablePlus lays it out:
                // Data | Structure | Index, + Row   1-300 of 1,000 rows   Filter, Columns, ‹ ⚙ ›
                h_flex()
                    .h(px(32.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.status_bar)
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .child(
                                h_flex()
                                    .p(px(2.))
                                    .gap_0p5()
                                    .items_center()
                                    .rounded_md()
                                    .bg(theme.secondary)
                                    .child(
                                        segment("mode-data", "Data", mode == TabMode::Data)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.set_mode(TabMode::Data, cx);
                                            })),
                                    )
                                    .child(
                                        segment(
                                            "mode-structure",
                                            "Structure",
                                            mode == TabMode::Structure,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.set_mode(TabMode::Structure, cx);
                                                this.ensure_structure_forms(window, cx);
                                            }),
                                        ),
                                    )
                                    .child(div().w_px().h(px(12.)).bg(theme.border))
                                    .child(
                                        segment("mode-index", "Index", mode == TabMode::Index)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.set_mode(TabMode::Index, cx);
                                                this.ensure_structure_forms(window, cx);
                                            })),
                                    ),
                            )
                            .when(mode == TabMode::Data, |d| {
                                d.child(
                                    Button::new("add-row")
                                        .icon(hugeicon(Hugeicon::Plus))
                                        .label(if is_zh { "行" } else { "Row" })
                                        .small()
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.insert_row(cx)),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("grid-status")
                            .test_support()
                            .aria_label(status.clone())
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_center()
                            .child(display_status),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("filters-toggle")
                                    .icon(hugeicon(Hugeicon::Search))
                                    .label(filter_label)
                                    .small()
                                    .min_w(px(84.))
                                    .when(filter_count > 0 || self.drawer_open, |b| b.primary())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_filters(cx)),
                                    ),
                            )
                            .child(
                                Button::new("columns-toggle")
                                    .label(columns_label)
                                    .small()
                                    .min_w(px(84.))
                                    .when(shown < total || self.columns_open, |b| b.primary())
                                    .disabled(total == 0)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_columns_panel(cx)),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .items_center()
                                    .child(
                                        super::cells::page_button(
                                            cx,
                                            "grid",
                                            pager_state,
                                            super::cells::PageNav::Prev,
                                            on_nav,
                                        )
                                        .ghost(),
                                    )
                                    .child(
                                        Button::new("page-settings")
                                            .icon(hugeicon(Hugeicon::Sliders))
                                            .ghost()
                                            .small()
                                            .tooltip(if is_zh { "分页" } else { "Pagination" })
                                            .when(self.paging_open, |b| b.primary())
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_paging_panel(cx)
                                            })),
                                    )
                                    .child(
                                        super::cells::page_button(
                                            cx,
                                            "grid",
                                            pager_state,
                                            super::cells::PageNav::Next,
                                            on_nav,
                                        )
                                        .ghost(),
                                    ),
                            ),
                    ),
            )
    }
}
