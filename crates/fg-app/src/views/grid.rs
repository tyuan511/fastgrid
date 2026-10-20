//! One table's data grid: the `DataTable` plus a status bar with paging.
//!
//! Like the launcher this view is dumb: it shows what it is given and emits
//! [`TableTabEvent::Reload`] when the user changes something that needs fresh data (page, sort,
//! page size). [`crate::controller`] answers by running the query and feeding results back with a
//! generation number, so a slow, outdated query can never overwrite a newer one.

use std::cell::RefCell;
use std::rc::Rc;

use fg_core::{ColumnMeta, ObjectPath, Value};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableSelection, TableState,
};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{CopySelection, GRID_CONTEXT};
use super::filter_drawer::{FilterDrawer, FilterDrawerEvent};
use crate::model::{
    Align, CellKind, DEFAULT_PAGE_SIZE, FilterDraft, GridQuery, PAGE_SIZES, SortState, align_for,
    copy_text, display, to_tsv,
};

/// A click on a column header, handed from the table delegate to the owning view.
type SortRequest = Rc<RefCell<Option<String>>>;

pub struct GridDelegate {
    metas: Vec<ColumnMeta>,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    sort: SortState,
    pending_sort: SortRequest,
    loading: bool,
}

impl GridDelegate {
    fn new(pending_sort: SortRequest) -> Self {
        Self {
            metas: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            sort: SortState::default(),
            pending_sort,
            loading: false,
        }
    }

    fn rebuild_columns(&mut self) {
        self.columns = self
            .metas
            .iter()
            .map(|m| {
                let mut col = Column::new(m.name.clone(), m.name.clone())
                    .width(px(default_width(m)))
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
        if let Some(meta) = self.metas.get(col_ix) {
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
        let Some(meta) = self.metas.get(col_ix) else {
            return div().into_any_element();
        };
        let multiple_keys = self.sort.keys().len() > 1;
        let priority = self
            .sort
            .indicator(&meta.name)
            .filter(|_| multiple_keys)
            .map(|(p, _)| p);
        h_flex()
            .size_full()
            .gap_1p5()
            .items_center()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(meta.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(short_type(meta)),
            )
            .when_some(priority, |d, p| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(p.to_string()),
                )
            })
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
        let value = self
            .rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .unwrap_or(&Value::Null);
        let cell = display(value);
        match cell.kind {
            CellKind::Null => div()
                .px_1p5()
                .rounded_sm()
                .bg(theme.muted)
                .text_xs()
                .italic()
                .text_color(theme.muted_foreground)
                .child(cell.text)
                .into_any_element(),
            CellKind::Pill => div()
                .px_1p5()
                .rounded_sm()
                .bg(theme.muted)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(cell.text)
                .into_any_element(),
            CellKind::Number => div()
                .w_full()
                .text_right()
                .font_family(theme.mono_font_family.clone())
                .child(cell.text)
                .into_any_element(),
            CellKind::Mono => div()
                .font_family(theme.mono_font_family.clone())
                .child(cell.text)
                .into_any_element(),
            CellKind::Bool => div()
                .font_family(theme.mono_font_family.clone())
                .text_color(if cell.text == "true" {
                    theme.success
                } else {
                    theme.muted_foreground
                })
                .child(cell.text)
                .into_any_element(),
            CellKind::Text => div().child(cell.text).into_any_element(),
        }
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .map(copy_text)
            .unwrap_or_default()
    }
}

/// `varchar(255)` is long; the badge shows the type's base name.
fn short_type(meta: &ColumnMeta) -> String {
    let native = meta.data_type.native.to_lowercase();
    native
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// What the grid asks its owner to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableTabEvent {
    /// The query changed (page, sort, page size...): fetch again.
    Reload,
}

pub struct TableTab {
    table: Entity<TableState<GridDelegate>>,
    drawer: Entity<FilterDrawer>,
    drawer_open: bool,
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
        let delegate = GridDelegate::new(Rc::clone(&pending));
        let table = cx.new(|cx| TableState::new(delegate, window, cx).cell_selectable(true));
        let drawer = cx.new(|cx| FilterDrawer::new(window, cx));
        let on_apply = cx.subscribe(&drawer, |this, _, event: &FilterDrawerEvent, cx| {
            let FilterDrawerEvent::Apply(applied) = event;
            this.query
                .set_filters(applied.filters.clone(), applied.raw_where.clone());
            cx.emit(TableTabEvent::Reload);
            cx.notify();
        });
        let observer = cx.observe(&table, move |this, _, cx| {
            let clicked = pending.borrow_mut().take();
            if let Some(column) = clicked {
                this.toggle_sort(&column, cx);
            }
        });
        let initial = FilterDraft::from_applied(&query.filters, query.raw_where.as_deref());
        drawer.update(cx, |d, cx| d.load(initial, cx));
        Self {
            table,
            drawer,
            drawer_open: false,
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
            TableSelection::Cell(r, c) => d.rows.get(r)?.get(c).map(copy_text),
            TableSelection::Row(r) => {
                let row = d.rows.get(r)?;
                Some(to_tsv(&[row.iter().map(copy_text).collect()]))
            }
            TableSelection::Column(c) => {
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
    }

    pub fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
    }

    pub fn select_column(&mut self, col: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| t.set_selected_col(col, cx));
    }

    /// Put the current selection on the clipboard (what `⌘C` does).
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
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
        let sizes = PAGE_SIZES.iter().map(|&n| {
            Button::new(SharedString::from(format!("page-size-{n}")))
                .label(n.to_string())
                .xsmall()
                .when(n == page_size, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_page_size(n, cx)))
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
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true)),
            )
            .when(self.drawer_open, |d| d.child(self.drawer.clone()))
            .child(
                h_flex()
                    .h_7()
                    .px_3()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.status_bar)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Button::new("filters-toggle")
                                    .label(if filter_count > 0 {
                                        format!("Filters ({filter_count})")
                                    } else {
                                        "Filters".to_owned()
                                    })
                                    .xsmall()
                                    .when(filter_count > 0 || self.drawer_open, |b| b.primary())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_filters(cx)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("grid-status")
                                    .test_support()
                                    .aria_label(status.clone())
                                    .child(status),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Button::new("page-first")
                                    .label("«")
                                    .xsmall()
                                    .disabled(!has_prev)
                                    .on_click(cx.listener(|this, _, _, cx| this.first_page(cx))),
                            )
                            .child(
                                Button::new("page-prev")
                                    .label("‹")
                                    .xsmall()
                                    .disabled(!has_prev)
                                    .on_click(cx.listener(|this, _, _, cx| this.prev_page(cx))),
                            )
                            .child(
                                Button::new("page-next")
                                    .label("›")
                                    .xsmall()
                                    .disabled(!has_next)
                                    .on_click(cx.listener(|this, _, _, cx| this.next_page(cx))),
                            )
                            .child(
                                Button::new("page-last")
                                    .label("»")
                                    .xsmall()
                                    .disabled(page.total().is_none() || !has_next)
                                    .on_click(cx.listener(|this, _, _, cx| this.last_page(cx))),
                            )
                            .child(div().w_2())
                            .children(sizes),
                    ),
            )
    }
}
