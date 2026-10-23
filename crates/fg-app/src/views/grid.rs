//! One table's data grid: the `DataTable` plus a status bar with paging.
//!
//! Like the launcher this view is dumb: it shows what it is given and emits
//! [`TableTabEvent::Reload`] when the user changes something that needs fresh data (page, sort,
//! page size). [`crate::controller`] answers by running the query and feeding results back with a
//! generation number, so a slow, outdated query can never overwrite a newer one.

use std::cell::RefCell;
use std::rc::Rc;

use std::collections::{HashMap, HashSet};

use fg_core::{ColumnMeta, Filter, FilterOp, ObjectPath, TableViewState, Value};
use fg_sql::{CellChange, RowChange};
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
    BeginEdit, CELL_EDITOR_CONTEXT, CancelCellEdit, CommitChanges, CopySelection, GRID_CONTEXT,
    InsertRow, RedoEdit, ToggleDeleteRow, UndoEdit,
};
use super::cells::{CellMark, CellSelection};
use super::filter_drawer::{FilterDrawer, FilterDrawerEvent};
use super::icons::{Hugeicon, hugeicon};
use crate::model::{
    Align, CellState, Changeset, Counts, DEFAULT_PAGE_SIZE, FilterDraft, GridQuery, JumpTarget,
    PAGE_SIZES, RowId, RowState, SortState, align_for, copy_text, parse_cell_text, to_tsv,
};

/// Screen columns before the data: the row-number gutter.
const GUTTER: usize = 1;
const GUTTER_WIDTH: f32 = 44.;

/// A click on a column header, handed from the table delegate to the owning view.
type SortRequest = Rc<RefCell<Option<String>>>;
/// A click on a foreign-key arrow: (row, column) of the cell.
type JumpRequest = Rc<RefCell<Option<(usize, usize)>>>;

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
    fn new(pending_sort: SortRequest, pending_jump: JumpRequest) -> Self {
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
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        // Our own `SortState` decides the cycle (asc, desc, off) and supports several keys;
        // the table only tells us which header was clicked.
        if let Some(meta) = self.data_index(col_ix).and_then(|i| self.metas.get(i)) {
            *self.pending_sort.borrow_mut() = Some(meta.name.clone());
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
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                *this.delegate().pending_sort.borrow_mut() = Some(name.clone());
                cx.notify();
            }))
            .into_any_element()
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
            return div()
                .id(SharedString::from(format!("row-number-{row_ix}")))
                .test_support()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .font_family(theme.mono_font_family.clone())
                .text_size(super::cells::GRID_TEXT_SIZE)
                .when(selected, |d| d.bg(theme.primary.opacity(0.12)))
                .text_color(if selected {
                    theme.foreground
                } else {
                    theme.muted_foreground.opacity(0.7)
                })
                .child((row_ix + 1).to_string())
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
    /// Something that is remembered per table but needs no new data changed (hidden columns).
    ViewChanged,
    /// The user asked to commit the staged changes (`⌘S`). The owner runs them and answers with
    /// [`TableTab::commit_finished`].
    CommitRequested,
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
    pending_filter: Option<Filter>,
    query: GridQuery,
    loading: bool,
    error: Option<SharedString>,
    elapsed_ms: Option<u64>,
    loaded_rows: usize,
    generation: u64,
    /// A commit is running; a second `⌘S` is ignored until it answers.
    committing: bool,
    _edit_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TableTabEvent> for TableTab {}

impl TableTab {
    pub fn new(path: ObjectPath, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_query(GridQuery::new(path, DEFAULT_PAGE_SIZE), window, cx)
    }

    pub fn with_query(query: GridQuery, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pending: SortRequest = Rc::new(RefCell::new(None));
        let pending_jump: JumpRequest = Rc::new(RefCell::new(None));
        let delegate = GridDelegate::new(Rc::clone(&pending), Rc::clone(&pending_jump));
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .row_header(false)
        });
        let drawer = cx.new(|cx| FilterDrawer::new(window, cx));
        let on_apply = cx.subscribe(&drawer, |this, _, event: &FilterDrawerEvent, cx| {
            let FilterDrawerEvent::Apply(applied) = event;
            this.query
                .set_filters(applied.filters.clone(), applied.raw_where.clone());
            cx.emit(TableTabEvent::Reload);
            cx.notify();
        });
        let observer = cx.observe(&table, move |this, table, cx| {
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
            if let Some(column) = clicked {
                this.toggle_sort(&column, cx);
            }
            let jump = pending_jump.borrow_mut().take();
            if let Some((row, col)) = jump {
                this.follow_foreign_key(row, col, cx);
            }
            cx.notify();
        });
        // A double click opens the cell for editing, like `Enter` does.
        let on_table =
            cx.subscribe_in(&table, window, |this, _, event: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(row, col) = event {
                    this.begin_edit_at(*row, *col, window, cx);
                }
            });
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
            _edit_subscription: None,
            _subscriptions: vec![observer, on_apply, on_table],
        }
    }

    /// Replace the query (restoring what was saved for this table) before the first load.
    pub fn restore_query(&mut self, query: GridQuery, cx: &mut Context<Self>) {
        let mut query = query;
        self.restored = true;
        // A foreign-key jump that arrived first wins over whatever filters were saved.
        if let Some(filter) = self.pending_filter.take() {
            query.set_filters(vec![filter], None);
        }
        let draft = FilterDraft::from_applied(&query.filters, query.raw_where.as_deref());
        self.drawer.update(cx, |d, cx| d.load(draft, cx));
        self.query = query;
        self.sync_sort(cx);
        cx.notify();
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
            self.begin_edit_at(row, col, window, cx);
        }
    }

    fn begin_edit_at(
        &mut self,
        row: usize,
        screen_col: usize,
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
        let text = match d.shown_value(row, data_col) {
            Value::Null => "NULL".to_owned(),
            Value::Other { type_name, text } if type_name.is_empty() && text == "DEFAULT" => {
                String::new()
            }
            other => copy_text(&other),
        };
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(text, window, cx);
            state
        });
        self._edit_subscription =
            Some(
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.commit_edit(window, cx);
                    }
                }),
            );
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
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
    pub fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.table.read(cx).delegate();
        let Some(editor) = d.editing.as_ref() else {
            return;
        };
        let (row, screen_col) = (editor.row_ix, editor.screen_col);
        let text = editor.input.read(cx).value().to_string();
        let Some(data_col) = d.data_index(screen_col) else {
            return;
        };
        let category = d.metas[data_col].data_type.category;
        let value = match parse_cell_text(&text, category) {
            Ok(v) => v,
            Err(message) => {
                self.fail_edit(message, cx);
                return;
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
                self.focus_table(window, cx);
                cx.notify();
            }
            Err(message) => self.fail_edit(message, cx),
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

    /// Throw every staged change away (undoable).
    pub fn discard_changes(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.editing = None;
            if let Some(cs) = d.changeset.as_mut() {
                cs.discard_all();
            }
            cx.notify();
        });
        self.error = None;
        cx.notify();
    }

    /// Ask the owner to commit (`⌘S`). An open editor is staged first, so what is on screen is
    /// what gets committed.
    pub fn request_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_editing(cx) {
            self.commit_edit(window, cx);
            if self.is_editing(cx) {
                return; // the value was rejected; the message is showing
            }
        }
        if self.committing || self.pending(cx).total() == 0 {
            return;
        }
        self.committing = true;
        self.error = None;
        cx.emit(TableTabEvent::CommitRequested);
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
                // Rows were added or removed: the count has to be asked for again.
                self.query.page.set_total(None);
                cx.emit(TableTabEvent::Reload);
            }
            Err(message) => self.error = Some(message.into()),
        }
        cx.notify();
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

    pub fn toggle_sort(&mut self, column: &str, cx: &mut Context<Self>) {
        self.query.toggle_sort(column, false);
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
        if !self.restored {
            self.pending_filter = Some(filter);
            return;
        }
        self.query.set_filters(vec![filter], None);
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

impl Render for TableTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // A segment of the Data | Structure | Index switch.
        let segment = |label: &'static str, active: bool| {
            div()
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
            .on_action(cx.listener(|this, _: &UndoEdit, _, cx| this.undo_edit(cx)))
            .on_action(cx.listener(|this, _: &RedoEdit, _, cx| this.redo_edit(cx)))
            .on_action(cx.listener(|this, _: &BeginEdit, window, cx| this.begin_edit(window, cx)))
            .on_action(
                cx.listener(|this, _: &CancelCellEdit, window, cx| this.cancel_edit(window, cx)),
            )
            .on_action(cx.listener(|this, _: &InsertRow, _, cx| this.insert_row(cx)))
            .on_action(cx.listener(|this, _: &ToggleDeleteRow, _, cx| this.toggle_delete_row(cx)))
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
                div().flex_1().min_h_0().child(
                    DataTable::new(&self.table)
                        .with_size(px(22.))
                        .bordered(false)
                        .fixed_cols_border(false)
                        .header_row_height(px(24.))
                        .stripe(true),
                ),
            )
            .when(self.drawer_open, |d| d.child(self.drawer.clone()))
            .children(column_panel)
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
                                    .child(segment("Data", true))
                                    .child(segment("Structure", false))
                                    .child(div().w_px().h(px(12.)).bg(theme.border))
                                    .child(segment("Index", false)),
                            )
                            .child(
                                Button::new("add-row")
                                    .icon(hugeicon(Hugeicon::Plus))
                                    .label(if is_zh { "行" } else { "Row" })
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| this.insert_row(cx))),
                            )
                            .when(pending.total() > 0, |d| {
                                d.child(
                                    Button::new("commit-changes")
                                        .label(if is_zh {
                                            format!("提交 ({})", pending.total())
                                        } else {
                                            format!("Commit ({})", pending.total())
                                        })
                                        .primary()
                                        .small()
                                        .disabled(self.committing)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.request_commit(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("discard-changes")
                                        .label(if is_zh { "放弃" } else { "Discard" })
                                        .small()
                                        .disabled(self.committing)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.discard_changes(cx)),
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
