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
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableSelection, TableState,
};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{CopySelection, GRID_CONTEXT};
use super::cells::CellSelection;
use super::filter_drawer::{FilterDrawer, FilterDrawerEvent};
use super::icons::{Hugeicon, hugeicon};
use crate::model::{
    Align, DEFAULT_PAGE_SIZE, FilterDraft, GridQuery, JumpTarget, PAGE_SIZES, SortState, align_for,
    copy_text, to_tsv,
};

/// A click on a column header, handed from the table delegate to the owning view.
type SortRequest = Rc<RefCell<Option<String>>>;
/// A click on a foreign-key arrow: (row, column) of the cell.
type JumpRequest = Rc<RefCell<Option<(usize, usize)>>>;

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
        }
    }

    /// The data column behind the `ix`-th column on screen.
    fn data_index(&self, ix: usize) -> Option<usize> {
        self.visible.get(ix).copied()
    }

    fn rebuild_columns(&mut self) {
        self.visible = (0..self.metas.len())
            .filter(|&i| !self.hidden.contains(&self.metas[i].name))
            .collect();
        self.columns = self
            .visible
            .iter()
            .map(|&i| &self.metas[i])
            .map(|m| {
                let mut col = Column::new(m.name.clone(), m.name.clone())
                    .width(px(default_width(m)))
                    .p_0()
                    .sortable();
                if align_for(m.data_type.category) == Align::Right {
                    col.align = TextAlign::Right;
                }
                match self.sort.indicator(&m.name) {
                    Some((_, false)) => col.ascending(),
                    Some((_, true)) => col.descending(),
                    None => col,
                }
            })
            .collect();
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
        self.rows.len()
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
        let multiple_keys = self.sort.keys().len() > 1;
        let priority = self
            .sort
            .indicator(&meta.name)
            .filter(|_| multiple_keys)
            .map(|(p, _)| p);
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
            .when_some(priority, |d, p| {
                d.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.muted_foreground)
                        .child(p.to_string()),
                )
            })
            .into_any_element();
        super::cells::th_frame(header, cx)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        // `col_ix` counts the columns on screen; the data may have hidden ones in between.
        let screen_col = col_ix;
        let col_ix = self.data_index(col_ix).unwrap_or(usize::MAX);
        let value = self
            .rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .unwrap_or(&Value::Null);
        let followable = !value.is_null()
            && self
                .metas
                .get(col_ix)
                .is_some_and(|m| self.jumps.contains_key(&m.name));
        let element = super::cells::cell_element(value, cx);
        let selection = match self.selected {
            Some((r, c)) if r == row_ix && c == screen_col => CellSelection::Cell,
            Some((r, _)) if r == row_ix => CellSelection::Row,
            _ => CellSelection::None,
        };
        if !followable {
            return super::cells::td_frame(element, selection, cx);
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
        super::cells::td_frame(linked, selection, cx)
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let col_ix = self.data_index(col_ix).unwrap_or(usize::MAX);
        self.rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .map(copy_text)
            .unwrap_or_default()
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
        let initial = FilterDraft::from_applied(&query.filters, query.raw_where.as_deref());
        drawer.update(cx, |d, cx| d.load(initial, cx));
        Self {
            table,
            drawer,
            drawer_open: false,
            columns_open: false,
            restored: false,
            pending_filter: None,
            query,
            loading: false,
            error: None,
            elapsed_ms: None,
            loaded_rows: 0,
            generation: 0,
            _subscriptions: vec![observer, on_apply],
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

    pub fn select_cell(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.table
            .update(cx, |t, cx| t.set_selected_cell(row, col, cx));
        cx.notify();
    }

    pub fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
        cx.notify();
    }

    pub fn select_column(&mut self, col: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| t.set_selected_col(col, cx));
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
            d.sort = SortState::from_keys(Vec::new());
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

        let filter_label = if is_zh {
            if filter_count > 0 {
                format!("过滤器 ({filter_count})")
            } else {
                "过滤器".to_owned()
            }
        } else if filter_count > 0 {
            format!("Filters ({filter_count})")
        } else {
            "Filter".to_owned()
        };

        let columns_label = if is_zh {
            format!("列 ({shown}/{total})")
        } else {
            format!("Columns ({shown}/{total})")
        };

        let sizes = super::cells::page_size_buttons(cx, &PAGE_SIZES, page_size, |this, n, cx| {
            this.set_page_size(n, cx)
        });

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
                        .with_size(px(30.))
                        .bordered(false)
                        .header_row_height(px(34.))
                        .stripe(false),
                ),
            )
            .when(self.drawer_open, |d| d.child(self.drawer.clone()))
            .children(column_panel)
            .child(
                // Status bar, 28px: Filters, Columns, page size, row range | page buttons.
                h_flex()
                    .h(px(44.))
                    .px_3()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.status_bar)
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    // Left: Data | Structure | Index | + Row | page sizes
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .child(
                                h_flex()
                                    .p_0p5()
                                    .rounded_md()
                                    .bg(theme.muted.opacity(0.6))
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_sm()
                                            .bg(theme.primary)
                                            .text_color(gpui::white())
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Data"),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_sm()
                                            .text_color(theme.muted_foreground)
                                            .cursor_pointer()
                                            .hover(|d| d.text_color(theme.foreground))
                                            .child("Structure"),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_sm()
                                            .text_color(theme.muted_foreground)
                                            .cursor_pointer()
                                            .hover(|d| d.text_color(theme.foreground))
                                            .child("Index"),
                                    ),
                            )
                            .child(
                                Button::new("add-row")
                                    .icon(hugeicon(Hugeicon::Plus))
                                    .label(if is_zh { "行" } else { "Row" })
                                    .ghost()
                                    .small(),
                            )
                            .child(sizes),
                    )
                    // Center: 1-300 of 1,000,000 rows / X of Y rows selected
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
                            .text_color(theme.foreground)
                            .child(display_status),
                    )
                    // Right: Filter | Columns | Pagination
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("filters-toggle")
                                    .icon(hugeicon(Hugeicon::Filter))
                                    .label(filter_label)
                                    .small()
                                    .when(filter_count > 0 || self.drawer_open, |b| b.primary())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_filters(cx)),
                                    ),
                            )
                            .child(
                                Button::new("columns-toggle")
                                    .icon(hugeicon(Hugeicon::Columns))
                                    .label(columns_label)
                                    .small()
                                    .when(shown < total || self.columns_open, |b| b.primary())
                                    .disabled(total == 0)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_columns_panel(cx)),
                                    ),
                            )
                            .child(super::cells::pager(
                                cx,
                                "grid",
                                super::cells::PagerState {
                                    has_prev,
                                    has_next,
                                    has_total: page.total().is_some(),
                                },
                                |this, nav, cx| this.navigate(nav, cx),
                            ))
                            .child(
                                hugeicon(Hugeicon::Sliders)
                                    .size_5()
                                    .text_color(theme.muted_foreground),
                            ),
                    ),
            )
    }
}
