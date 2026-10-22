//! A query tab: the SQL editor on top, the results of the last run below.
//!
//! Like every other view here this renders data and emits events ([`QueryTabEvent`]); the service
//! calls live in [`crate::controller`]. The editor itself is GPUI Kit's `EditorState` with the
//! Tree-sitter SQL grammar (`language("sql")`; `tests/editor_grammar.rs` pins that the grammar is
//! registered), so line numbers, multi-cursor, undo, search and the mono font come from the
//! library rather than from us.
//!
//! What we do own: which statement to run (`model::run_target`), how a refusal or a confirmation
//! is shown, and how `QueryError.position` becomes a wavy underline.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use fg_core::{
    ColumnMeta, DangerReason, DbKind, DocumentId, Error as CoreError, ObjectPath, Value,
};
use fg_driver::Event;
use fg_service::{PolicyRefusal, RunPlan};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, TextDecoration, TextDecorationCollection,
};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::model::{
    self, DEFAULT_RESULT_PAGE_SIZE, PageState, QueryDocumentState, QueryResults, RESULT_PAGE_SIZES,
    RunMode, RunStatus, RunTarget, align_for, run_target,
};

/// What the tab asks its owner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryTabEvent {
    /// Run something. The target is read from the buffer when the event is handled, so what runs
    /// is exactly what the editor holds at that moment.
    Run { mode: RunMode },
    /// Stop the statement that is running.
    Cancel,
    /// The buffer changed: the draft should be persisted (debounced by the controller).
    DraftChanged,
    /// The user accepted the confirmation prompt for `script`. The controller must run **this**
    /// string: the service binds a confirmation to a digest of the exact SQL.
    ConfirmedRun { script: String },
    /// The user refused the confirmation prompt.
    DismissedConfirmation,
    /// Explain what the current statement would do, instead of running it.
    Explain,
}

/// A dangerous statement waiting for an explicit click.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingConfirmation {
    /// The SQL that will run if the user confirms. The service binds its confirmation to a digest
    /// of this exact text, so it must be handed back unchanged.
    pub script: String,
    /// The statement that triggered the prompt, shown so the user reads it before clicking.
    pub statement: String,
    /// Where that statement sits in the buffer: an error in the confirmed run must be underlined
    /// at its real position, not at the start of the substring.
    pub target: RunTarget,
    pub reason: Option<DangerReason>,
    /// A production-looking connection gets a louder prompt.
    pub production: bool,
}

/// A read-only result grid. Unlike the browse grid this has no paging, sorting or filtering: it
/// shows what one run returned. Deliberately separate from `views::grid`, whose delegate carries
/// sort, hidden-column and foreign-key state that an ad-hoc result must not inherit.
pub struct ResultDelegate {
    metas: Vec<ColumnMeta>,
    columns: Vec<Column>,
    /// Only the rows of the page on screen. A result set can hold a hundred thousand rows and the
    /// table must not be asked to lay all of them out.
    rows: Vec<Vec<Value>>,
    loading: bool,
    empty_message: Option<String>,
}

impl Default for ResultDelegate {
    fn default() -> Self {
        Self::new()
    }
}

impl ResultDelegate {
    fn new() -> Self {
        Self {
            metas: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            loading: false,
            empty_message: None,
        }
    }

    /// New columns for a new result set; the rows belong to the previous one.
    fn replace(&mut self, metas: Vec<ColumnMeta>) {
        self.columns = metas
            .iter()
            .map(|m| {
                let mut col = Column::new(m.name.clone(), m.name.clone())
                    .width(px(result_width(m)))
                    .p_0()
                    // Marked sortable only so the header reserves the same space for the arrow as
                    // the table grid's headers do; clicking it does nothing (a result is not
                    // re-sorted client-side).
                    .sortable();
                if align_for(m.data_type.category) == model::Align::Right {
                    col.align = TextAlign::Right;
                }
                col
            })
            .collect();
        self.rows.clear();
        self.metas = metas;
    }
}

/// A starting width per type, so the first paint is usable without a measurement pass.
fn result_width(meta: &ColumnMeta) -> f32 {
    use fg_core::TypeCategory::*;
    let by_type: f32 = match meta.data_type.category {
        Bool => 70.0,
        Integer => 90.0,
        Float | Decimal => 110.0,
        Date => 110.0,
        Time => 100.0,
        Timestamp | TimestampTz => 170.0,
        Uuid => 280.0,
        Json | Binary => 200.0,
        _ => 160.0,
    };
    by_type
        .max(meta.name.chars().count() as f32 * 8.0 + 60.0)
        .min(360.0)
}

impl TableDelegate for ResultDelegate {
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

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(meta) = self.metas.get(col_ix) else {
            return div().into_any_element();
        };
        // The same header the table grid draws: the column name and nothing else. No type badge -
        // a result's header is the driver's column name, and a type next to it is noise the user
        // did not ask for (the table grid does not show one either).
        let header = h_flex()
            .gap_1p5()
            .items_center()
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .child(meta.name.clone()),
            )
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
        let value = self
            .rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .unwrap_or(&Value::Null);
        super::cells::td_frame(
            super::cells::cell_element(value, cx),
            super::cells::CellSelection::None,
            cx,
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let message = self
            .empty_message
            .clone()
            .unwrap_or_else(|| "No results yet.".to_owned());
        div()
            .id("result-empty")
            .test_support()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.muted_foreground)
            .child(message)
    }
}

/// SQL completion for the editor, backed by `fg-lsp`.
///
/// The analysis is the server's: it resolves which relations are in scope at the caret (aliases,
/// joins, CTEs) and offers their columns, so `i.` means `items` because `i` was declared as its
/// alias, not because a name happened to match a prefix. The provider only adapts that answer to
/// the editor's menu types.
pub struct SqlCompletion {
    catalog: Rc<RefCell<CompletionCatalog>>,
    kind: DbKind,
}

/// The schema the analysis draws on. Owned by the tab and shared with the provider, so refreshing
/// it does not rebuild the editor.
#[derive(Debug, Clone, Default)]
pub struct CompletionCatalog {
    objects: Vec<ObjectPath>,
    columns: Vec<(ObjectPath, Vec<ColumnMeta>)>,
    /// Columns of CTE bodies the caller managed to analyse. Empty for now: the body is a query of
    /// its own, and resolving it is the next step (the CTE then falls back to "no columns known").
    ctes: Vec<(String, Vec<ColumnMeta>)>,
}

impl CompletionCatalog {
    pub fn with_objects(objects: Vec<ObjectPath>) -> Self {
        Self {
            objects,
            ..Self::default()
        }
    }

    pub fn set_columns(&mut self, path: ObjectPath, columns: Vec<ColumnMeta>) {
        match self.columns.iter_mut().find(|(p, _)| *p == path) {
            Some(slot) => slot.1 = columns,
            None => self.columns.push((path, columns)),
        }
    }

    pub fn objects(&self) -> &[ObjectPath] {
        &self.objects
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

impl fg_lsp::Catalog for CompletionCatalog {
    fn columns(&self, path: &ObjectPath) -> Option<&[ColumnMeta]> {
        self.columns
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.as_slice())
    }

    fn resolve(&self, name: &str) -> Option<ObjectPath> {
        self.objects
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    fn cte_columns(&self, name: &str) -> Option<Vec<ColumnMeta>> {
        self.ctes
            .iter()
            .find(|(cte, _)| cte.eq_ignore_ascii_case(name))
            .map(|(_, columns)| columns.clone())
    }
}

impl SqlCompletion {
    fn new(catalog: Rc<RefCell<CompletionCatalog>>, kind: DbKind) -> Self {
        Self { catalog, kind }
    }
}

impl gpui_kit::component::input::CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        text: &gpui_kit::component::input::Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let text = text.to_string();
        let catalog = self.catalog.borrow();
        let completions =
            fg_lsp::complete_at(&text, offset, self.kind, &*catalog, catalog.objects());
        let items = completions
            .into_iter()
            .map(|completion| {
                let kind = match completion.kind {
                    fg_lsp::CompletionKind::Column => lsp_types::CompletionItemKind::FIELD,
                    fg_lsp::CompletionKind::Relation => lsp_types::CompletionItemKind::VARIABLE,
                    fg_lsp::CompletionKind::Table => lsp_types::CompletionItemKind::CLASS,
                    fg_lsp::CompletionKind::Keyword => lsp_types::CompletionItemKind::KEYWORD,
                };
                // `text_edit`, not `insert_text`: the editor inserts a bare `insert_text` **at the
                // caret** (`range.end..range.end`), so accepting `select` after typing `se` gave
                // `seselect`. A text edit says which bytes the completion replaces - the word the
                // user already typed - which is what makes it a completion rather than an
                // insertion.
                let (start_line, start_character) =
                    fg_lsp::position_of(&text, completion.replace.start);
                let (end_line, end_character) = fg_lsp::position_of(&text, completion.replace.end);
                let edit = lsp_types::TextEdit {
                    range: lsp_types::Range {
                        start: lsp_types::Position {
                            line: start_line,
                            character: start_character,
                        },
                        end: lsp_types::Position {
                            line: end_line,
                            character: end_character,
                        },
                    },
                    new_text: completion.text,
                };
                lsp_types::CompletionItem {
                    label: completion.label,
                    kind: Some(kind),
                    detail: completion.detail,
                    text_edit: Some(lsp_types::CompletionTextEdit::Edit(edit)),
                    ..Default::default()
                }
            })
            .collect();
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }

    /// Offer completions while a name is being typed. Whitespace and punctuation are not completion
    /// positions, and popping up there would fight the user.
    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    }
}

/// One query: an editor, its draft identity, and the results of the last run.
pub struct QueryTab {
    document: QueryDocumentState,
    kind: DbKind,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultDelegate>>,
    results: QueryResults,
    /// The target and script of the run in flight, for mapping `QueryError.position` back onto the
    /// buffer: positions are relative to the statement, and only that run knows where it started.
    running_target: Option<(RunTarget, String)>,
    confirmation: Option<PendingConfirmation>,
    record_error: Option<String>,
    /// A draft written by a newer version: shown, read-only, never re-saved over.
    unavailable: Option<String>,
    /// What the buffer held the last time we set or saved it. A `Change` that puts the text back
    /// to this is not an edit: restoring a draft writes through the editor and the library emits a
    /// change event afterwards, which would otherwise mark a freshly loaded draft unsaved.
    content_baseline: String,
    /// Byte range underlined as the failing position. Kept so a later run can replace it.
    decoration: Option<TextDecorationCollection>,
    active_set: usize,
    /// The page shown for the active result set. A run can return far more rows than the table
    /// should be asked to lay out at once, so the delegate only ever holds one page.
    page: PageState,
    /// Schema snapshot the completion provider reads. Shared with the provider, so refreshing it
    /// does not need to rebuild the editor.
    catalog: Rc<RefCell<CompletionCatalog>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<QueryTabEvent> for QueryTab {}

impl QueryTab {
    pub fn new(
        document: QueryDocumentState,
        kind: DbKind,
        script: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let catalog: Rc<RefCell<CompletionCatalog>> = Rc::default();
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(script);
            state.lsp_mut().completion_provider =
                Some(Rc::new(SqlCompletion::new(Rc::clone(&catalog), kind)));
            state
        });
        let on_edit = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = this.editor.read(cx).value().to_string();
                if text == this.content_baseline {
                    // Our own restore, not the user's keystroke.
                    return;
                }
                this.content_baseline = text;
                // An underline that survived an edit points at the wrong bytes. The decoration
                // ranges follow edits, but the failure itself does not: drop it on the first
                // keystroke after a failed run.
                if this.decoration.is_some() {
                    this.clear_underline(cx);
                }
                this.document.mark_dirty();
                cx.emit(QueryTabEvent::DraftChanged);
                cx.notify();
            }
        });
        let self_script_baseline = editor.read(cx).value().to_string();
        let table = cx.new(|cx| TableState::new(ResultDelegate::new(), window, cx));
        Self {
            document,
            kind,
            editor,
            table,
            results: QueryResults::new(),
            running_target: None,
            confirmation: None,
            record_error: None,
            unavailable: None,
            content_baseline: self_script_baseline,
            decoration: None,
            active_set: 0,
            page: PageState::new(DEFAULT_RESULT_PAGE_SIZE),
            catalog,
            _subscriptions: vec![on_edit],
        }
    }

    // ---- what the controller reads ------------------------------------------------------------

    pub fn document_id(&self) -> &DocumentId {
        self.document.id()
    }

    pub fn document(&self) -> &QueryDocumentState {
        &self.document
    }

    pub fn editor_state(&self) -> &Entity<EditorState> {
        &self.editor
    }

    /// The whole buffer. This is the exact string the service plans and runs, so there is no second
    /// copy that could drift out of sync with the confirmation digest.
    pub fn script(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    pub fn cursor(&self, cx: &App) -> usize {
        self.editor.read(cx).cursor()
    }

    /// The selection, if it spans something. A caret alone is not a selection.
    pub fn selection(&self, cx: &App) -> Option<Range<usize>> {
        let range = self.editor.read(cx).selected_range();
        (range.start != range.end).then_some(range)
    }

    /// What `mode` means for the current buffer. `None` is "nothing to run", not an error.
    pub fn target(&self, mode: RunMode, cx: &App) -> Option<RunTarget> {
        let script = self.script(cx);
        let cursor = self.cursor(cx).min(script.len());
        run_target(&script, cursor, self.selection(cx), self.kind, mode)
    }

    pub fn results(&self) -> &QueryResults {
        &self.results
    }

    pub fn is_dirty(&self) -> bool {
        self.document.dirty()
    }

    pub fn is_unavailable(&self) -> bool {
        self.unavailable.is_some()
    }

    pub fn title(&self) -> String {
        self.document.title().to_owned()
    }

    pub fn pending_confirmation(&self) -> Option<&PendingConfirmation> {
        self.confirmation.as_ref()
    }

    /// Which result set the panel is showing (sub-tab index into `results().sets()`).
    pub fn active_set(&self) -> usize {
        self.active_set
    }

    /// The byte ranges currently underlined as a failure. Exposed because the ranges are the
    /// behaviour that matters: an underline in the wrong place is worse than none.
    pub fn underline_ranges(&self, cx: &App) -> Vec<Range<usize>> {
        self.decoration
            .as_ref()
            .map(|collection| collection.get_ranges(cx))
            .unwrap_or_default()
    }

    /// What to run after the user confirmed: the exact statement text and the buffer offsets it
    /// came from. Read (not take) because a failing run leaves the prompt gone but the target still
    /// needed for error mapping, which `begin_run` has already recorded.
    pub fn confirmation_target(&self) -> Option<(RunTarget, String)> {
        self.confirmation
            .as_ref()
            .map(|p| (p.target.clone(), p.script.clone()))
    }

    // ---- what the controller writes -----------------------------------------------------------

    /// Replace the buffer without making the draft dirty (used when restoring a session).
    pub fn set_content(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.content_baseline = text.to_owned();
        self.editor.update(cx, |editor, cx| {
            editor.replace_all(text, window, cx);
        });
        self.document.mark_clean();
        cx.notify();
    }

    /// Re-indent the buffer. Only the text changes; the draft stays dirty, as an edit should.
    ///
    /// The whole buffer is formatted rather than the statement under the caret: a formatter that
    /// reformats half a script leaves the other half inconsistent, and the user asked for a
    /// formatted script.
    pub fn format_buffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.script(cx);
        let after = fg_sql::format_sql(&before, self.kind);
        if after == before || after.is_empty() {
            return;
        }
        // Keep the caret where it was in the text, clamped: a reformat that jumps the caret to the
        // start is disorienting.
        let caret = self.cursor(cx).min(after.len());
        self.editor.update(cx, |editor, cx| {
            editor.replace_all(after.clone(), window, cx);
            editor.set_selected_range(caret..caret, cx);
        });
        cx.notify();
    }

    /// Toggle `--` on the lines the selection touches (or the caret's line).
    pub fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.script(cx);
        let selection = self
            .selection(cx)
            .unwrap_or_else(|| self.cursor(cx)..self.cursor(cx));
        let edit = fg_sql::toggle_line_comments(&text, selection.clone(), self.kind);
        if edit.text == text[selection.clone()] {
            return;
        }
        let mut next = String::with_capacity(text.len() + 8);
        next.push_str(&text[..selection.start.min(text.len())]);
        next.push_str(&edit.text);
        next.push_str(&text[selection.end.min(text.len())..]);
        // Select the same lines again, shifted by what the markers added or removed.
        let delta = edit.delta_per_line;
        let shift = |offset: usize| -> usize {
            if offset <= selection.start {
                offset
            } else if delta >= 0 {
                offset + delta as usize
            } else {
                offset.saturating_sub((-delta) as usize)
            }
        };
        let start = selection.start;
        let end = shift(selection.end).max(start);
        self.editor.update(cx, |editor, cx| {
            editor.replace_all(next, window, cx);
            editor.set_selected_range(start..end, cx);
        });
        cx.notify();
    }

    /// A draft this version cannot read. Rendered read-only so saving cannot destroy the newer
    /// record (docs/data-model.md: unreadable records are shown, never overwritten).
    pub fn set_unavailable(&mut self, reason: String, cx: &mut Context<Self>) {
        self.unavailable = Some(reason);
        cx.notify();
    }

    /// Called after `Service::save_document` succeeds. An unreadable draft is never clean: there
    /// is nothing we could save that the newer version would accept.
    pub fn mark_saved(&mut self, cx: &mut Context<Self>) {
        if self.unavailable.is_none() {
            self.content_baseline = self.editor.read(cx).value().to_string();
            self.document.mark_clean();
            cx.notify();
        }
    }

    pub fn set_title(&mut self, title: impl Into<String>, cx: &mut Context<Self>) {
        self.document.set_title(title);
        cx.notify();
    }

    /// Start a run: fresh results, no banners, no underline. Returns the generation that every
    /// later update for this run must carry, so a stale stream cannot paint the new one.
    pub fn begin_run(&mut self, target: RunTarget, script: String, cx: &mut Context<Self>) -> u64 {
        self.confirmation = None;
        self.record_error = None;
        self.running_target = Some((target, script));
        self.clear_underline(cx);
        let generation = self.results.begin_run();
        self.active_set = 0;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().replace(Vec::new());
            t.delegate_mut().loading = true;
            t.delegate_mut().empty_message = None;
            t.refresh(cx);
        });
        cx.notify();
        generation
    }

    /// How many statements the service's plan found (shown as `2 of 3 statements`).
    pub fn apply_plan(&mut self, generation: u64, plan: &RunPlan, cx: &mut Context<Self>) {
        self.results
            .set_planned_statements(generation, plan.statements.len());
        cx.notify();
    }

    /// One streamed event from the run.
    pub fn apply_event(&mut self, generation: u64, event: Event, cx: &mut Context<Self>) {
        let columns = match &event {
            Event::ResultStart { columns, .. } => Some(columns.clone()),
            _ => None,
        };
        let rows = match &event {
            Event::Rows(rows) => Some(rows.clone()),
            _ => None,
        };
        if !self.results.apply_event(generation, event) {
            return;
        }
        if let Some(columns) = columns {
            self.active_set = self.results.sets().len().saturating_sub(1);
            self.table.update(cx, |t, cx| {
                t.delegate_mut().replace(columns);
                t.delegate_mut().loading = false;
                t.refresh(cx);
            });
            // A new set starts at its first page.
            self.page.goto(0, self.active_rows().len() as u64);
            self.refresh_page(cx);
        }
        if rows.is_some() {
            // The rows live in the model; the delegate only ever holds one page of them.
            self.page
                .goto(self.page.number(), self.active_rows().len() as u64);
            let total = self.active_rows().len();
            let window = self.page.window(total);
            let page_rows = self
                .active_rows()
                .get(window)
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            self.table.update(cx, |t, cx| {
                t.delegate_mut().rows = page_rows;
                t.delegate_mut().loading = false;
                cx.notify();
            });
        }
        cx.notify();
    }

    pub fn finish_run(&mut self, generation: u64, cx: &mut Context<Self>) {
        if !self.results.finish(generation) {
            return;
        }
        self.table.update(cx, |t, cx| {
            t.delegate_mut().loading = false;
            if t.delegate().columns_count(cx) == 0 {
                t.delegate_mut().empty_message = Some("No rows returned.".to_owned());
            }
            cx.notify();
        });
        cx.notify();
    }

    /// The run failed. `Error::Cancelled` is not a failure: the model records it as cancelled and
    /// no underline is drawn for it.
    pub fn fail_run(&mut self, generation: u64, error: &CoreError, cx: &mut Context<Self>) {
        let Some((target, script)) = self.running_target.clone() else {
            return;
        };
        // How many statements produced a result set before the failure: with `fg-lsp`'s arithmetic
        // that is enough to say which statement failed, even in a script. This is what makes a
        // multi-statement error underlineable instead of a bare message.
        let started = self.results.statements_run();
        let diagnostic = match error {
            CoreError::Query(query) => fg_lsp::diagnostic_for(&script, self.kind, started, query),
            _ => None,
        };
        if !self.results.fail(generation, error, &target, &script) {
            return;
        }
        self.stop_loading(cx);
        if let Some(diagnostic) = diagnostic {
            self.underline(diagnostic.range, cx);
        }
        cx.notify();
    }

    /// The service refused the script before touching the database.
    pub fn refuse(
        &mut self,
        generation: u64,
        plan: &RunPlan,
        refusal: PolicyRefusal,
        cx: &mut Context<Self>,
    ) {
        if !self.results.refuse(generation, plan, refusal) {
            return;
        }
        self.stop_loading(cx);
        cx.notify();
    }

    /// The user cancelled. The service records the outcome; the tab only shows it.
    pub fn mark_cancelled(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.results.cancel(generation) {
            self.stop_loading(cx);
            cx.notify();
        }
    }

    /// A statement ran but its history or audit row could not be written. That is the user's data
    /// losing its audit trail, so it is shown rather than logged.
    pub fn set_record_error(
        &mut self,
        generation: u64,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.results.generation() {
            return;
        }
        self.record_error = message;
        cx.notify();
    }

    /// Show the prompt for a statement the safe mode wants a human to look at.
    pub fn ask_confirmation(&mut self, prompt: PendingConfirmation, cx: &mut Context<Self>) {
        self.confirmation = Some(prompt);
        self.stop_loading(cx);
        cx.notify();
    }

    /// Which page of the active result set is on screen.
    pub fn paging(&self) -> &PageState {
        &self.page
    }

    /// The text of each result header cell, in column order.
    ///
    /// Exposed so a test can pin what a header shows without reaching into the library's rendered
    /// tree: a header is the column name and nothing else, and a type badge creeping in is exactly
    /// the kind of change that should fail a test rather than be noticed in a screenshot.
    pub fn result_header_text(&self, cx: &App) -> Vec<String> {
        let table = self.table.read(cx);
        let delegate = table.delegate();
        (0..delegate.columns_count(cx))
            .map(|ix| delegate.column(ix, cx).name.to_string())
            .collect()
    }

    /// The column definitions the result table is rendering.
    ///
    /// Exposed because the header's appearance is decided by them: a `sort` makes the library
    /// reserve space for the sort arrow, which is what keeps a result header aligned with a table
    /// header.
    pub fn result_columns(&self, cx: &App) -> Vec<gpui_kit::component::table::Column> {
        let table = self.table.read(cx);
        let delegate = table.delegate();
        (0..delegate.columns_count(cx))
            .map(|ix| delegate.column(ix, cx))
            .collect()
    }

    /// Replace the schema snapshot completion draws on. Cheap: the editor keeps the same provider.
    pub fn set_catalog(&mut self, catalog: CompletionCatalog, cx: &mut Context<Self>) {
        *self.catalog.borrow_mut() = catalog;
        cx.notify();
    }

    /// Describe one table for completion. Called as the schema loads, so the first keystroke after
    /// opening a tab already knows the columns of the tables in it.
    pub fn set_table_columns(
        &mut self,
        path: fg_core::ObjectPath,
        columns: Vec<ColumnMeta>,
        cx: &mut Context<Self>,
    ) {
        self.catalog.borrow_mut().set_columns(path, columns);
        cx.notify();
    }

    pub fn set_page_size(&mut self, size: u32, cx: &mut Context<Self>) {
        if size != self.page.size() {
            self.page.set_size(size);
            self.refresh_page(cx);
        }
    }

    /// Paging is local: the rows are already here, so this never touches the database.
    fn navigate(&mut self, nav: super::cells::PageNav, cx: &mut Context<Self>) {
        let total = self.active_rows().len() as u64;
        let page = self.page.number();
        let target = match nav {
            super::cells::PageNav::First => 0,
            super::cells::PageNav::Prev => page.saturating_sub(1),
            super::cells::PageNav::Next => page + 1,
            super::cells::PageNav::Last => u64::MAX,
        };
        if target != page {
            self.page.goto(target, total);
            self.refresh_page(cx);
        }
    }

    /// The rows of the active result set (all of them, not only the visible page).
    fn active_rows(&self) -> &[Vec<Value>] {
        self.results
            .sets()
            .get(self.active_set)
            .map(|set| set.rows.as_slice())
            .unwrap_or(&[])
    }

    /// Push the current page of the active set into the table delegate.
    fn refresh_page(&mut self, cx: &mut Context<Self>) {
        let total = self.active_rows().len() as u64;
        self.page.goto(self.page.number(), total);
        let window = self.page.window(total as usize);
        let rows = self
            .active_rows()
            .get(window)
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        self.table.update(cx, |t, cx| {
            t.delegate_mut().rows = rows;
            t.refresh(cx);
        });
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    // ---- internals ----------------------------------------------------------------------------

    fn stop_loading(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            t.delegate_mut().loading = false;
            cx.notify();
        });
    }

    /// A wavy underline over the failing bytes. Decorations (byte ranges that follow edits) rather
    /// than diagnostics (line/column, and wiped by the engine on every edit) because our offsets
    /// are bytes and the user usually fixes the statement right away.
    fn underline(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range.is_empty() {
            return;
        }
        let theme = cx.theme().clone();
        let style = HighlightStyle {
            underline: Some(UnderlineStyle {
                color: Some(theme.danger),
                thickness: px(1.),
                wavy: true,
            }),
            ..Default::default()
        };
        let existing = self.decoration.clone();
        self.editor.update(cx, |editor, cx| {
            let collection = match existing {
                Some(collection) => {
                    collection.set(vec![TextDecoration::new(range.clone(), style)], cx);
                    collection
                }
                None => editor.create_decorations_collection(
                    vec![TextDecoration::new(range.clone(), style)],
                    cx,
                ),
            };
            self.decoration = Some(collection);
        });
    }

    fn clear_underline(&mut self, cx: &mut Context<Self>) {
        if let Some(collection) = self.decoration.take() {
            collection.clear(cx);
        }
    }
}

impl Render for QueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let running = self.results.status() == RunStatus::Running;
        let set_count = self.results.sets().len();
        let sets: Vec<AnyElement> = (0..set_count)
            .filter_map(|ix| {
                let set = self.results.sets().get(ix)?;
                let label = if set.returns_rows() {
                    format!("Result {} · {} rows", set.index + 1, set.row_count())
                } else {
                    format!(
                        "Result {} · {} affected",
                        set.index + 1,
                        set.affected_rows.unwrap_or(0)
                    )
                };
                // The cap belongs in the label, not only in a badge: a truncated result must be
                // noticeable to anyone reading the tab, including a screen reader.
                let label = if set.truncated {
                    format!("{label} · truncated")
                } else {
                    label
                };
                let index = set.index;
                let active = ix == self.active_set;
                Some(
                    div()
                        .id(SharedString::from(format!("result-tab-{index}")))
                        .test_support()
                        .aria_label(label.clone())
                        .px_2()
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .when(active, |d| {
                            d.text_color(theme.foreground).bg(theme.muted.opacity(0.6))
                        })
                        .when(!active, |d| d.text_color(theme.muted_foreground))
                        .when(set.truncated, |d| {
                            d.child(div().text_xs().text_color(theme.warning).child("truncated"))
                        })
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.active_set = ix;
                            if let Some(set) = this.results.sets().get(ix) {
                                let metas = set.columns.clone();
                                this.table.update(cx, |t, cx| {
                                    t.delegate_mut().replace(metas);
                                    t.refresh(cx);
                                });
                                this.page.goto(0, this.active_rows().len() as u64);
                                this.refresh_page(cx);
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            })
            .collect();

        v_flex()
            .id("query-tab")
            .test_support()
            .size_full()
            .on_action(cx.listener(|_, _: &crate::views::RunStatement, _, cx| {
                cx.emit(QueryTabEvent::Run {
                    mode: RunMode::CurrentStatement,
                });
            }))
            .on_action(cx.listener(|_, _: &crate::views::RunAll, _, cx| {
                cx.emit(QueryTabEvent::Run { mode: RunMode::All });
            }))
            .on_action(
                cx.listener(|this, _: &crate::views::FormatSql, window, cx| {
                    this.format_buffer(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::views::ToggleComment, window, cx| {
                    this.toggle_comment(window, cx);
                }),
            )
            .on_action(cx.listener(|_, _: &crate::views::ExplainQuery, _, cx| {
                cx.emit(QueryTabEvent::Explain);
            }))
            .on_action(cx.listener(|this, _: &crate::views::CancelQuery, _, cx| {
                // Reflect it at once rather than waiting for the controller: the user pressed the
                // key, the stream is being cancelled, and `Running…` must stop immediately. The
                // controller calls the same method with the same generation, where it is a no-op.
                let generation = this.results.generation();
                this.mark_cancelled(generation, cx);
                cx.emit(QueryTabEvent::Cancel);
            }))
            .child(
                v_flex()
                    .id("query-editor")
                    .h_1_2()
                    .min_h_0()
                    .flex_shrink_0()
                    .px_2()
                    .pt_2()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_shrink_0()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(self.document.title().to_owned()),
                                    )
                                    .when(self.document.dirty(), |d| {
                                        d.child(
                                            div()
                                                .id("draft-dirty")
                                                .test_support()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child("unsaved"),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        Button::new("run-statement")
                                            .label("Run")
                                            .primary()
                                            .xsmall()
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(QueryTabEvent::Run {
                                                    mode: RunMode::CurrentStatement,
                                                });
                                            })),
                                    )
                                    .child(
                                        Button::new("run-all")
                                            .label("Run all")
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(QueryTabEvent::Run { mode: RunMode::All });
                                            })),
                                    )
                                    .when(running, |d| {
                                        d.child(
                                            Button::new("cancel-run")
                                                .label("Cancel")
                                                .danger()
                                                .xsmall()
                                                .on_click(cx.listener(|_, _, _, cx| {
                                                    cx.emit(QueryTabEvent::Cancel);
                                                })),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        Editor::new(&self.editor)
                            .appearance(false)
                            .bordered(false)
                            .readonly(self.unavailable.is_some())
                            .flex_1()
                            .min_h_0(),
                    ),
            )
            .when_some(self.unavailable.clone(), |d, reason| {
                d.child(
                    div()
                        .id("draft-unavailable")
                        .test_support()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(theme.warning)
                        .aria_label(unavailable_text(&reason))
                        .child(unavailable_text(&reason)),
                )
            })
            .when_some(self.confirmation.clone(), |d, prompt| {
                d.child(
                    v_flex()
                        .id("confirm-run")
                        .test_support()
                        .gap_1()
                        .p_2()
                        .border_y_1()
                        .border_color(theme.danger.opacity(0.5))
                        .bg(theme
                            .danger
                            .opacity(if prompt.production { 0.14 } else { 0.08 }))
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .id("confirm-title")
                                        .test_support()
                                        .aria_label(confirm_title(&prompt))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.danger)
                                        .child(confirm_title(&prompt)),
                                )
                                .when(prompt.production, |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.danger)
                                            .child("PRODUCTION"),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .id("confirm-statement")
                                .test_support()
                                .aria_label(prompt.statement.clone())
                                .font_family(theme.mono_font_family.clone())
                                .text_sm()
                                .child(prompt.statement),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("confirm-run-yes")
                                        .label("Run anyway")
                                        .danger()
                                        .small()
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(QueryTabEvent::ConfirmedRun {
                                                script: prompt.script.clone(),
                                            });
                                        })),
                                )
                                .child(
                                    Button::new("confirm-run-no")
                                        .label("Cancel")
                                        .ghost()
                                        .small()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            // The prompt is the tab's own state: clearing it here
                                            // is what makes "declined" visible immediately, whatever
                                            // the controller does next.
                                            this.confirmation = None;
                                            cx.emit(QueryTabEvent::DismissedConfirmation);
                                        })),
                                ),
                        ),
                )
            })
            .when_some(self.results.refusal().cloned(), |d, refusal| {
                d.child(
                    div()
                        .id("run-refused")
                        .test_support()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .text_color(theme.danger)
                        .aria_label(refusal_text(&refusal))
                        .child(refusal_text(&refusal)),
                )
            })
            .when_some(self.results.error().cloned(), |d, failure| {
                d.child(
                    div()
                        .id("run-error")
                        .test_support()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .text_color(theme.danger)
                        .aria_label(error_text(&failure))
                        .child(error_text(&failure)),
                )
            })
            .when_some(self.record_error.clone(), |d, message| {
                d.child(
                    div()
                        .id("record-error")
                        .test_support()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .text_color(theme.warning)
                        .aria_label(record_error_text(&message))
                        .child(record_error_text(&message)),
                )
            })
            .child(
                h_flex()
                    .id("result-tabs")
                    .test_support()
                    .h(px(26.))
                    .flex_shrink_0()
                    .items_center()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.tab_bar)
                    .px_1()
                    .children(sets),
            )
            .child(
                div()
                    .id("result-grid")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .child(
                        // The same geometry as the table grid: same row height, same header
                        // height, same padding. A result and a table should look like the same
                        // widget, because they are.
                        DataTable::new(&self.table)
                            .with_size(px(30.))
                            .bordered(false)
                            .header_row_height(px(34.))
                            .stripe(false),
                    ),
            )
            .child(
                h_flex()
                    .id("result-status-bar")
                    .test_support()
                    // The table grid's status bar is 44px; the two must match or switching between a
                    // table and a query moves the grid up and down.
                    .h(px(44.))
                    .px_3()
                    .gap_3()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.status_bar)
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            // `Data | Structure | Index | + Row` in the table grid; a result has no
                            // structure to show, so the left slot holds the page sizes and the row
                            // range instead, in the same place.
                            .child(super::cells::page_size_buttons(
                                cx,
                                &RESULT_PAGE_SIZES,
                                self.page.size(),
                                |this, n, cx| this.set_page_size(n, cx),
                            ))
                            .child(
                                div()
                                    .id("result-range")
                                    .test_support()
                                    .aria_label(self.page.label())
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.page.label()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .id("run-status")
                                    .test_support()
                                    .aria_label(status_text(&self.results, running))
                                    .child(status_text(&self.results, running)),
                            )
                            .child(super::cells::pager(
                                cx,
                                "result",
                                super::cells::PagerState {
                                    has_prev: self.page.has_prev(),
                                    has_next: self.page.has_next(),
                                    has_total: true,
                                },
                                |this, nav, cx| this.navigate(nav, cx),
                            )),
                    ),
            )
            .when(!self.results.notices().is_empty(), |d| {
                d.child(
                    v_flex()
                        .id("run-messages")
                        .test_support()
                        .max_h(px(80.))
                        .overflow_y_scroll()
                        .px_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .children(
                            self.results
                                .notices()
                                .iter()
                                .map(|n| div().child(n.clone())),
                        ),
                )
            })
    }
}

/// The danger prompt's headline. Kept as a plain sentence so the view stays a renderer.
fn confirm_title(prompt: &PendingConfirmation) -> String {
    match prompt.reason {
        Some(reason) => format!("This {}. Run it anyway?", danger_phrase(reason)),
        // Level 2 asks about every write, not only the ones with a specific danger.
        None => "This writes to the database. Run it anyway?".to_owned(),
    }
}

/// Why a statement is dangerous, in one clause. English only: the view layer owns localization.
pub fn danger_phrase(reason: DangerReason) -> &'static str {
    match reason {
        DangerReason::DropObject => "drops a database object",
        DangerReason::Truncate => "empties a table with TRUNCATE",
        DangerReason::DeleteWithoutWhere => "deletes every row (no WHERE)",
        DangerReason::UpdateWithoutWhere => "updates every row (no WHERE)",
        DangerReason::AlterDestructive => "alters a table in a destructive way",
        // `Other` and any reason a future version adds: say less rather than something wrong.
        _ => "is considered dangerous",
    }
}

fn unavailable_text(reason: &str) -> String {
    format!(
        "This draft was written by a newer version and cannot be read ({reason}). It is left untouched."
    )
}

fn error_text(failure: &model::RunFailure) -> String {
    match &failure.code {
        Some(code) => format!("{} ({code})", failure.message),
        None => failure.message.clone(),
    }
}

fn record_error_text(message: &str) -> String {
    format!("The statement ran, but fastgrid could not record it: {message}")
}

fn refusal_text(refusal: &model::RefusalInfo) -> String {
    let index = refusal.statement_index() + 1;
    match &refusal.refusal {
        PolicyRefusal::ReadOnly { .. } => {
            format!("Refused: this connection is read-only, so statement {index} cannot run.",)
        }
        PolicyRefusal::NeedsConfirmation { .. } => {
            format!("Statement {index} needs confirmation.")
        }
    }
}

fn status_text(results: &QueryResults, running: bool) -> String {
    if running {
        return "Running…".to_owned();
    }
    let planned = results.planned_statements().max(results.statements_run());
    let counts = format!(
        "{} rows · {} of {} statement{}",
        results.total_rows(),
        results.statements_run(),
        planned,
        if planned == 1 { "" } else { "s" }
    );
    match results.status() {
        RunStatus::Idle => "Idle.".to_owned(),
        RunStatus::Refused => "Nothing ran.".to_owned(),
        RunStatus::Cancelled => format!("Cancelled · {counts}"),
        _ => {
            let elapsed = results
                .elapsed_ms()
                .map(|ms| format!("{ms} ms"))
                .unwrap_or_else(|| "—".to_owned());
            let mut text = format!("{counts} · {elapsed}");
            if results.truncated() {
                text.push_str(" · truncated by the row limit");
            }
            text
        }
    }
}
