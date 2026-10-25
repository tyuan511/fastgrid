//! The state behind one SQL query tab: what a run sends, where an engine error points, and what
//! came back.
//!
//! Pure Rust on purpose (see `docs/ui-testing.md`): the GPUI view renders these values, and the
//! controller feeds [`fg_driver::Event`]s in, so a whole query tab can be exercised without a
//! window. Three pieces:
//!
//! * [`RunTarget`] turns "run this" plus a cursor/selection into the exact SQL to send and the
//!   byte range it occupies in the script, so a highlight and an error underline can be drawn on
//!   the text the user still has in front of them.
//! * [`QueryResults`] accumulates one run behind a generation number, so a slow, outdated stream
//!   cannot overwrite a newer one.
//! * [`QueryDocumentState`] carries the identity autosave needs and converts to and from the
//!   persisted [`QueryDocument`].
//!
//! Nothing here formats a user-facing string: the view localizes, this module only describes.

use std::ops::Range;

use fg_core::{
    ColumnMeta, ConnectionId, DangerReason, DbKind, DocumentId, Error, QueryDocument,
    StatementClass, UnixMillis, Value,
};
use fg_driver::Event;
use fg_service::{PolicyRefusal, RunPlan};
use fg_sql::{split_statements, statement_at};

/// What a run shortcut should send. The view maps its keybindings onto one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// The statement the cursor is in. The default, because that is what a SQL editor's primary
    /// run key means.
    #[default]
    CurrentStatement,
    /// The whole script.
    All,
    /// The selection; falls back to [`RunMode::CurrentStatement`] when nothing (or nothing that
    /// contains a statement) is selected.
    Selection,
}

/// Exactly what one run sends, and where it sits in the editor text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTarget {
    sql: String,
    range: Range<usize>,
    statement_index: Option<usize>,
}

impl RunTarget {
    /// The text to hand to `OpenConnection::run`.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Byte range of [`Self::sql`] inside the script the user is looking at.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub fn start(&self) -> usize {
        self.range.start
    }

    /// Bytes in the script covered by this target.
    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    /// A target always carries a statement, so this is only here to satisfy `clippy`.
    pub fn is_empty(&self) -> bool {
        self.range.start >= self.range.end
    }

    /// Index of the statement in the script's span list where this target begins. A view needs
    /// it to line a [`RunMode::All`] plan entry up with the script; for
    /// [`RunMode::CurrentStatement`] the plan has exactly one statement, so it is always `Some(0)`
    /// there. `None` only if a selection covered text that no statement owns.
    pub fn statement_index(&self) -> Option<usize> {
        self.statement_index
    }

    /// Turn an engine-reported, statement-relative `position` into a range in the script.
    ///
    /// Without a position the whole target is underlined (the engine said "this statement is
    /// wrong" and nothing finer). With one, the range covers the token that starts at that byte:
    /// engines report the start of the offending token, and underlining a single character would
    /// read as a stray dot. A position that is not on a char boundary (or past the end) is moved
    /// onto one: slicing must never panic on the text the user is still typing.
    pub fn error_span(&self, script: &str, position: Option<usize>) -> ErrorSpan {
        match position {
            Some(offset) => {
                let start = floor_boundary(
                    script,
                    self.range.start.saturating_add(offset).min(script.len()),
                );
                ErrorSpan {
                    start,
                    len: token_len(script, start, floor_boundary(script, self.range.end)),
                }
            }
            None => {
                let start = floor_boundary(script, self.range.start.min(script.len()));
                let end = floor_boundary(script, self.range.end.min(script.len()));
                ErrorSpan {
                    start,
                    len: end.saturating_sub(start),
                }
            }
        }
    }
}

/// A byte range in the script to underline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorSpan {
    pub start: usize,
    /// Bytes covered: the offending token for an engine position, the whole statement when the
    /// engine gave no position, `0` when the position is past the end of the text.
    pub len: usize,
}

impl ErrorSpan {
    pub fn end(&self) -> usize {
        self.start + self.len
    }
}

/// Decide what to send for `mode`.
///
/// Returns `None` when there is nothing to run: blank text, only comments, or a selection that
/// contains no statement. Sending an empty string would make the service plan nothing and the
/// tab report a run that did not happen.
///
/// A [`RunMode::CurrentStatement`] target is the statement's own text, without its `;`, which is
/// the same slice `fg_service::plan` reports in `PlannedStatement::offset`. [`RunMode::All`] sends
/// the script verbatim, trailing semicolon and all.
pub fn run_target(
    script: &str,
    cursor: usize,
    selection: Option<Range<usize>>,
    kind: DbKind,
    mode: RunMode,
) -> Option<RunTarget> {
    let spans = split_statements(script, kind);
    match mode {
        RunMode::Selection => selection
            .and_then(|range| selection_target(script, &spans, range))
            .or_else(|| current_statement_target(script, &spans, cursor)),
        RunMode::CurrentStatement => current_statement_target(script, &spans, cursor),
        RunMode::All => all_target(script, &spans),
    }
}

fn current_statement_target(
    script: &str,
    spans: &[fg_sql::StatementSpan],
    cursor: usize,
) -> Option<RunTarget> {
    let cursor = floor_boundary(script, cursor.min(script.len()));
    let index = statement_at(spans, cursor)?;
    let span = spans.get(index)?;
    if span.sql(script).trim().is_empty() {
        return None;
    }
    Some(RunTarget {
        sql: span.sql(script).to_owned(),
        range: span.text.clone(),
        statement_index: Some(index),
    })
}

fn selection_target(
    script: &str,
    spans: &[fg_sql::StatementSpan],
    range: Range<usize>,
) -> Option<RunTarget> {
    let start = floor_boundary(script, range.start.min(script.len()));
    let end = ceil_boundary(script, range.end.min(script.len()));
    if start >= end {
        return None;
    }
    let sql = script.get(start..end)?;
    // A blank selection, or one that only covers whitespace between statements, is not a target;
    // the caller falls back to the statement under the cursor.
    if sql.trim().is_empty()
        || !spans
            .iter()
            .any(|s| s.text.start < end && start < s.text.end)
    {
        return None;
    }
    Some(RunTarget {
        sql: sql.to_owned(),
        range: start..end,
        statement_index: statement_at(spans, start),
    })
}

fn all_target(script: &str, spans: &[fg_sql::StatementSpan]) -> Option<RunTarget> {
    if spans.is_empty() {
        return None;
    }
    Some(RunTarget {
        sql: script.to_owned(),
        range: 0..script.len(),
        statement_index: Some(0),
    })
}

/// Largest char boundary at or before `offset`.
fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Smallest char boundary at or after `offset`.
fn ceil_boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Length of the token starting at `start`, never crossing `end`.
///
/// A "token" is a run of identifier-ish characters (`abc`, `t.col`, `$1`), which is what every
/// supported engine points at. Punctuation and whitespace stop it. A punctuation byte is
/// underlined on its own, so an engine that reports a stray `(` still gets a visible mark.
fn token_len(text: &str, start: usize, end: usize) -> usize {
    let rest = match text.get(start..end) {
        Some(rest) => rest,
        None => return 0,
    };
    let mut len = 0usize;
    for ch in rest.chars() {
        let continues = ch.is_alphanumeric() || matches!(ch, '_' | '.' | '$');
        if len > 0 && !continues {
            break;
        }
        if len == 0 && !continues {
            return ch.len_utf8();
        }
        len += ch.len_utf8();
    }
    len
}

/// One result set of a run, in arrival order, with the index the driver reported.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSetState {
    /// 0-based statement index within the run, as reported by `Event::ResultStart`. Kept as
    /// reported instead of being renumbered here, so a view's sub-tabs line up with the plan.
    pub index: usize,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    pub affected_rows: Option<u64>,
    pub last_insert_id: Option<u64>,
    pub truncated: bool,
    /// `None` until `ResultEnd` arrived.
    pub elapsed_ms: Option<u64>,
    pub warnings: Vec<String>,
    /// `ResultEnd` was seen, so this set is not growing any more.
    pub complete: bool,
}

impl ResultSetState {
    fn started(index: usize, columns: Vec<ColumnMeta>) -> Self {
        Self {
            index,
            columns,
            rows: Vec::new(),
            affected_rows: None,
            last_insert_id: None,
            truncated: false,
            elapsed_ms: None,
            warnings: Vec::new(),
            complete: false,
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The table this result set reads from, when every column maps to one table. An expression,
    /// an aggregate, a join, or a column the engine could not place has no origin, and a result
    /// like that can never be edited safely: there is no single row to write back to.
    pub fn editable_table(&self) -> Option<fg_core::ObjectPath> {
        if self.columns.is_empty() {
            return None;
        }
        let mut table: Option<(Option<String>, String)> = None;
        for column in &self.columns {
            let origin = column.origin.as_ref()?;
            let this = (origin.namespace.clone(), origin.table.clone());
            match &table {
                None => table = Some(this),
                Some(known) if *known == this => {}
                Some(_) => return None,
            }
        }
        let (namespace, name) = table?;
        Some(fg_core::ObjectPath::new(namespace.as_deref(), name))
    }

    /// A statement that returns rows has columns; DML and DDL report only an affected count.
    pub fn returns_rows(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// What the status line needs, as data instead of a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Nothing has run since the tab opened.
    Idle,
    /// Events are still arriving.
    Running,
    /// The script finished.
    Finished,
    Failed,
    /// The policy refused the script before a session was leased.
    Refused,
    Cancelled,
}

/// Why a run failed, with the underline range already mapped into the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFailure {
    pub message: String,
    /// Native error code or SQLSTATE, when the engine gave one.
    pub code: Option<String>,
    /// Absolute range in the script. `None` for failures that are not about a statement (a lost
    /// connection, a storage error).
    pub span: Option<ErrorSpan>,
}

/// A policy refusal plus the statement it objected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalInfo {
    pub refusal: PolicyRefusal,
    /// The offending statement's text, copied from the plan. Empty only if the plan does not list
    /// that index, which `fg_service::plan` never does; the view must not panic on it.
    pub statement: String,
}

impl RefusalInfo {
    pub fn statement_index(&self) -> usize {
        match self.refusal {
            PolicyRefusal::ReadOnly { statement, .. }
            | PolicyRefusal::NeedsConfirmation { statement, .. } => statement,
        }
    }

    pub fn class(&self) -> StatementClass {
        match self.refusal {
            PolicyRefusal::ReadOnly { class, .. }
            | PolicyRefusal::NeedsConfirmation { class, .. } => class,
        }
    }

    pub fn danger(&self) -> Option<DangerReason> {
        match self.refusal {
            PolicyRefusal::ReadOnly { .. } => None,
            PolicyRefusal::NeedsConfirmation { danger, .. } => danger,
        }
    }
}

/// Everything one run produced.
///
/// Every mutating method takes the generation [`Self::begin_run`] returned and ignores calls that
/// carry an older one: a run whose stream the user already abandoned must not touch the tab.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryResults {
    generation: u64,
    open: bool,
    planned_statements: usize,
    sets: Vec<ResultSetState>,
    notices: Vec<String>,
    error: Option<RunFailure>,
    refusal: Option<RefusalInfo>,
    cancelled: bool,
}

impl QueryResults {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new run: forget the previous one and return the generation every later update must
    /// carry. Bumping before clearing is what makes a late event from the old stream harmless.
    pub fn begin_run(&mut self) -> u64 {
        self.generation += 1;
        self.open = true;
        self.planned_statements = 0;
        self.sets.clear();
        self.notices.clear();
        self.error = None;
        self.refusal = None;
        self.cancelled = false;
        self.generation
    }

    /// How many statements the plan says will run, so the view can say "1 of 3".
    pub fn set_planned_statements(&mut self, generation: u64, count: usize) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.planned_statements = count;
        true
    }

    /// Feed one driver event. Returns whether it was applied; a stale generation, a finished run
    /// or a run that already failed takes nothing.
    pub fn apply_event(&mut self, generation: u64, event: Event) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        match event {
            Event::ResultStart { index, columns } => {
                self.sets.push(ResultSetState::started(index, columns));
                true
            }
            Event::Rows(rows) => match self.sets.last_mut() {
                Some(set) => {
                    set.rows.extend(rows);
                    true
                }
                // Every driver sends `ResultStart` first; dropping is better than inventing a set.
                None => false,
            },
            Event::ResultEnd {
                affected_rows,
                last_insert_id,
                truncated,
                elapsed_ms,
                warnings,
            } => match self.sets.last_mut() {
                Some(set) => {
                    set.affected_rows = affected_rows;
                    set.last_insert_id = last_insert_id;
                    set.truncated = truncated;
                    set.elapsed_ms = Some(elapsed_ms);
                    set.warnings = warnings;
                    set.complete = true;
                    true
                }
                None => false,
            },
            Event::Notice(message) => {
                self.notices.push(message);
                true
            }
            _ => false,
        }
    }

    /// The script finished. Nothing else is accepted for this generation.
    pub fn finish(&mut self, generation: u64) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.open = false;
        true
    }

    /// Record a failure and close the run. A [`Error::Cancelled`] is not a failure: it is
    /// reported through [`Self::cancelled`], which is what the view styles differently.
    pub fn fail(
        &mut self,
        generation: u64,
        error: &Error,
        target: &RunTarget,
        script: &str,
    ) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.open = false;
        match error {
            Error::Cancelled => self.cancelled = true,
            Error::Query(query) => {
                self.error = Some(RunFailure {
                    message: query.message.clone(),
                    code: query.code.clone(),
                    span: Some(target.error_span(script, query.position)),
                });
            }
            // Not a statement problem, so there is nothing to underline.
            other => {
                self.error = Some(RunFailure {
                    message: other.to_string(),
                    code: None,
                    span: None,
                })
            }
        }
        true
    }

    /// The user cancelled this run.
    pub fn cancel(&mut self, generation: u64) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.open = false;
        self.cancelled = true;
        true
    }

    /// The policy refused the script, so nothing ran. The statement text comes from the plan the
    /// service would have enforced.
    pub fn refuse(&mut self, generation: u64, plan: &RunPlan, refusal: PolicyRefusal) -> bool {
        if !self.accepts(generation) {
            return false;
        }
        self.open = false;
        self.planned_statements = plan.statements.len();
        let index = match refusal {
            PolicyRefusal::ReadOnly { statement, .. }
            | PolicyRefusal::NeedsConfirmation { statement, .. } => statement,
        };
        let statement = plan
            .statements
            .get(index)
            .map(|s| s.sql.clone())
            .unwrap_or_default();
        self.refusal = Some(RefusalInfo { refusal, statement });
        true
    }

    fn accepts(&self, generation: u64) -> bool {
        self.open && generation == self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn status(&self) -> RunStatus {
        if self.refusal.is_some() {
            return RunStatus::Refused;
        }
        if self.cancelled {
            return RunStatus::Cancelled;
        }
        if self.error.is_some() {
            return RunStatus::Failed;
        }
        if self.open {
            return RunStatus::Running;
        }
        if self.generation == 0 {
            RunStatus::Idle
        } else {
            RunStatus::Finished
        }
    }

    pub fn is_running(&self) -> bool {
        self.open
    }

    /// Result sets in arrival order, one per statement that started.
    pub fn sets(&self) -> &[ResultSetState] {
        &self.sets
    }

    pub fn set(&self, index: usize) -> Option<&ResultSetState> {
        self.sets.get(index)
    }

    /// Fold committed updates back into a result set's rows, so a committed edit shows without
    /// re-running the query. Only `Set` cells change: a `Default` cell's new value is whatever
    /// the engine picked, which only a re-run would reveal.
    pub fn apply_result_changes(&mut self, set_index: usize, changes: &[fg_sql::RowChange]) {
        let Some(set) = self.sets.get_mut(set_index) else {
            return;
        };
        let columns: Vec<String> = set
            .columns
            .iter()
            .map(|m| {
                m.origin
                    .as_ref()
                    .map(|o| o.column.clone())
                    .unwrap_or_else(|| m.name.clone())
            })
            .collect();
        for change in changes {
            let fg_sql::RowChange::Update { key, cells } = change else {
                continue;
            };
            let key_positions: Vec<usize> = key
                .iter()
                .filter_map(|(name, _)| columns.iter().position(|c| c == name))
                .collect();
            if key_positions.len() != key.len() {
                continue;
            }
            for row in set.rows.iter_mut() {
                let matches = key_positions
                    .iter()
                    .enumerate()
                    .all(|(i, &pos)| row.get(pos) == Some(&key[i].1));
                if !matches {
                    continue;
                }
                for (name, cell) in cells {
                    if let Some(pos) = columns.iter().position(|c| c == name)
                        && let fg_sql::CellChange::Set(value) = cell
                        && let Some(slot) = row.get_mut(pos)
                    {
                        *slot = value.clone();
                    }
                }
            }
        }
    }

    /// Rows delivered across every set.
    pub fn total_rows(&self) -> usize {
        self.sets.iter().map(ResultSetState::row_count).sum()
    }

    /// Sum of the per-statement times the engine reported, once any set finished.
    pub fn elapsed_ms(&self) -> Option<u64> {
        let mut total: Option<u64> = None;
        for set in &self.sets {
            if let Some(ms) = set.elapsed_ms {
                total = Some(total.unwrap_or(0).saturating_add(ms));
            }
        }
        total
    }

    /// True when any set hit the row cap. This is the same aggregation
    /// [`fg_service::RunStream::truncated`] uses (one `true` `ResultEnd` marks the whole run).
    pub fn truncated(&self) -> bool {
        self.sets.iter().any(|set| set.truncated)
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn error(&self) -> Option<&RunFailure> {
        self.error.as_ref()
    }

    pub fn refusal(&self) -> Option<&RefusalInfo> {
        self.refusal.as_ref()
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn planned_statements(&self) -> usize {
        self.planned_statements
    }

    /// Statements that actually started (one result set each), which is less than
    /// [`Self::planned_statements`] when an earlier statement failed.
    pub fn statements_run(&self) -> usize {
        self.sets.len()
    }
}

/// Identity of the draft behind a query tab, and what autosave needs to write it.
///
/// `updated_at` is deliberately not held here: it is the time of the save, so it belongs to the
/// caller of [`Self::to_document`], not to the editor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryDocumentState {
    id: DocumentId,
    connection_id: ConnectionId,
    title: String,
    saved_path: Option<String>,
    dirty: bool,
    cursor_offset: usize,
    created_at: UnixMillis,
}

impl QueryDocumentState {
    /// A fresh, never-saved draft. `dirty` starts false: nothing has been edited yet, and the
    /// editor sets it on the first change.
    pub fn new(connection_id: ConnectionId, title: impl Into<String>, now: UnixMillis) -> Self {
        Self {
            id: DocumentId::new(),
            connection_id,
            title: title.into(),
            saved_path: None,
            dirty: false,
            cursor_offset: 0,
            created_at: now,
        }
    }

    /// Restore the identity of a stored draft. A cursor offset that does not fit this platform
    /// saturates instead of failing: it only decides where the caret lands.
    pub fn from_document(document: &QueryDocument) -> Self {
        Self {
            id: document.id.clone(),
            connection_id: document.connection_id.clone(),
            title: document.title.clone(),
            saved_path: document.saved_path.clone(),
            dirty: document.dirty,
            cursor_offset: document
                .cursor_offset
                .map(|offset| usize::try_from(offset).unwrap_or(usize::MAX))
                .unwrap_or(0),
            created_at: document.created_at,
        }
    }

    /// The record to store. `content` is the editor text (this state does not hold it) and `now`
    /// is the save time, which becomes `updated_at`. `created_at` is carried over untouched.
    pub fn to_document(&self, content: &str, now: UnixMillis) -> QueryDocument {
        QueryDocument {
            id: self.id.clone(),
            connection_id: self.connection_id.clone(),
            title: self.title.clone(),
            content: content.to_owned(),
            saved_path: self.saved_path.clone(),
            dirty: self.dirty,
            cursor_offset: u64::try_from(self.cursor_offset).ok(),
            created_at: self.created_at,
            updated_at: now,
        }
    }

    pub fn id(&self) -> &DocumentId {
        &self.id
    }

    pub fn connection_id(&self) -> &ConnectionId {
        &self.connection_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn saved_path(&self) -> Option<&str> {
        self.saved_path.as_deref()
    }

    pub fn set_saved_path(&mut self, path: Option<String>) {
        self.saved_path = path;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Saved to `saved_path` (or the user dismissed the "unsaved" mark).
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn cursor_offset(&self) -> usize {
        self.cursor_offset
    }

    pub fn set_cursor_offset(&mut self, offset: usize) {
        self.cursor_offset = offset;
    }

    pub fn created_at(&self) -> UnixMillis {
        self.created_at
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{DataType, QueryError, SafeMode, TypeCategory};
    use fg_service::plan;

    use super::*;

    fn kind() -> DbKind {
        DbKind::Sqlite
    }

    fn target(script: &str, cursor: usize, mode: RunMode) -> Option<RunTarget> {
        run_target(script, cursor, None, kind(), mode)
    }

    fn columns() -> Vec<ColumnMeta> {
        vec![ColumnMeta::new(
            "n",
            DataType::new("INTEGER", TypeCategory::Integer),
        )]
    }

    fn row(n: i64) -> Vec<Value> {
        vec![Value::Int(n)]
    }

    fn start(index: usize) -> Event {
        Event::ResultStart {
            index,
            columns: columns(),
        }
    }

    fn start_dml(index: usize) -> Event {
        Event::ResultStart {
            index,
            columns: Vec::new(),
        }
    }

    fn end(truncated: bool, elapsed_ms: u64) -> Event {
        Event::ResultEnd {
            affected_rows: None,
            last_insert_id: None,
            truncated,
            elapsed_ms,
            warnings: Vec::new(),
        }
    }

    fn with_origin(mut meta: ColumnMeta, table: &str, column: &str) -> ColumnMeta {
        meta.origin = Some(fg_core::ColumnOrigin {
            namespace: None,
            table: table.to_owned(),
            column: column.to_owned(),
        });
        meta
    }

    fn editable_columns() -> Vec<ColumnMeta> {
        vec![
            with_origin(
                ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
                "items",
                "id",
            ),
            with_origin(
                ColumnMeta::new("name", DataType::new("TEXT", TypeCategory::Text)),
                "items",
                "name",
            ),
        ]
    }

    #[test]
    fn a_single_table_result_with_origins_is_editable_and_a_join_is_not() {
        let mut set = ResultSetState::started(0, editable_columns());
        set.rows = vec![vec![Value::Int(1), Value::Text("a".into())]];
        assert_eq!(
            set.editable_table(),
            Some(fg_core::ObjectPath::new(None, "items"))
        );
        let mut joined = ResultSetState::started(0, editable_columns());
        joined.columns[1] = with_origin(
            ColumnMeta::new("name", DataType::new("TEXT", TypeCategory::Text)),
            "other",
            "name",
        );
        assert_eq!(joined.editable_table(), None, "a join has no single table");
        let mut expression = ResultSetState::started(0, editable_columns());
        expression.columns[1] =
            ColumnMeta::new("n", DataType::new("INTEGER", TypeCategory::Integer));
        assert_eq!(
            expression.editable_table(),
            None,
            "an expression column has no origin"
        );
    }

    fn query_error(message: &str, position: Option<usize>) -> Error {
        Error::Query(QueryError {
            message: message.into(),
            code: Some("1".into()),
            position,
        })
    }

    // ---- what a run sends -------------------------------------------------------------------

    #[test]
    fn the_statement_under_the_cursor_is_the_target() {
        let script = "SELECT 1;\nSELECT 2;\nSELECT 3";
        let at = script.find("SELECT 2").unwrap() + 3;
        let t = target(script, at, RunMode::CurrentStatement).unwrap();
        assert_eq!(t.sql(), "SELECT 2");
        assert_eq!(t.start(), script.find("SELECT 2").unwrap());
        assert_eq!(t.len(), "SELECT 2".len());
        assert_eq!(t.statement_index(), Some(1));
        assert!(!t.is_empty());
        assert_eq!(&script[t.range()], t.sql());
    }

    #[test]
    fn a_cursor_on_the_semicolon_runs_the_statement_it_terminates() {
        let script = "SELECT 1;\n  \nSELECT 2;";
        let semi = script.find(';').unwrap();
        assert_eq!(
            target(script, semi, RunMode::CurrentStatement)
                .unwrap()
                .sql(),
            "SELECT 1",
            "a cursor on `;` belongs to that statement"
        );
        assert_eq!(
            target(script, semi + 1, RunMode::CurrentStatement)
                .unwrap()
                .sql(),
            "SELECT 1",
            "the byte right after `;` is still the same statement"
        );
        // Further into the gap the splitter attributes the cursor to the statement that follows,
        // which is what a freshly typed next statement needs.
        assert_eq!(
            target(script, semi + 2, RunMode::CurrentStatement)
                .unwrap()
                .sql(),
            "SELECT 2"
        );
    }

    #[test]
    fn a_cursor_in_trailing_whitespace_after_the_last_statement_runs_it() {
        let script = "SELECT 1;\nSELECT 2;   \n\n";
        let t = target(script, script.len(), RunMode::CurrentStatement).unwrap();
        assert_eq!(t.sql(), "SELECT 2");
        assert_eq!(t.statement_index(), Some(1));
    }

    #[test]
    fn semicolons_inside_a_string_do_not_split_a_statement() {
        let script = "SELECT 'a;b' AS x; SELECT 2";
        let inside = script.find("a;b").unwrap() + 1;
        let t = target(script, inside, RunMode::CurrentStatement).unwrap();
        assert_eq!(t.sql(), "SELECT 'a;b' AS x");
        assert_eq!(t.statement_index(), Some(0));
    }

    #[test]
    fn a_semicolon_inside_a_comment_does_not_split_and_keeps_its_statement_together() {
        let script = "SELECT 1; -- tail;\nSELECT 2";
        // The comment belongs to the statement that follows it, so a cursor there runs that one.
        let in_comment = script.find("-- tail").unwrap() + 3;
        let t = target(script, in_comment, RunMode::CurrentStatement).unwrap();
        assert_eq!(t.sql(), "-- tail;\nSELECT 2");
        assert_eq!(t.statement_index(), Some(1));
        assert_eq!(t.start(), script.find("-- tail").unwrap());
    }

    #[test]
    fn a_selection_wins_over_the_statement_under_the_cursor() {
        let script = "SELECT 1;\nSELECT 2;";
        let selected = script.find("SELECT 2").unwrap();
        let cursor = script.find("SELECT 1").unwrap();
        let t = run_target(
            script,
            cursor,
            Some(selected..script.len()),
            kind(),
            RunMode::Selection,
        )
        .unwrap();
        assert_eq!(t.sql(), "SELECT 2;");
        assert_eq!(t.range(), selected..script.len());
        assert_eq!(t.statement_index(), Some(1));
    }

    #[test]
    fn selection_mode_falls_back_to_the_cursor_when_nothing_useful_is_selected() {
        let script = "SELECT 1;\nSELECT 2;";
        let cursor = script.find("SELECT 2").unwrap();
        let none = run_target(script, cursor, None, kind(), RunMode::Selection).unwrap();
        assert_eq!(none.sql(), "SELECT 2");
        let empty = run_target(
            script,
            cursor,
            Some(cursor..cursor),
            kind(),
            RunMode::Selection,
        )
        .unwrap();
        assert_eq!(empty.sql(), "SELECT 2");
        let blank = run_target(
            script,
            cursor,
            Some(script.find("\n").unwrap()..script.find("SELECT 2").unwrap()),
            kind(),
            RunMode::Selection,
        )
        .unwrap();
        assert_eq!(blank.sql(), "SELECT 2", "a blank gap is not a target");
    }

    #[test]
    fn a_selection_is_clamped_to_char_boundaries() {
        let script = "SELECT '世界'; SELECT 2;";
        let wide = script.find('世').unwrap();
        let t = run_target(
            script,
            wide,
            Some(wide + 1..script.len()),
            kind(),
            RunMode::Selection,
        )
        .unwrap();
        assert!(
            t.sql().starts_with('世'),
            "a mid-char start moves back, not forward: {:?}",
            t.sql()
        );
        assert_eq!(t.start(), wide);
        let t = run_target(script, wide, Some(0..wide + 1), kind(), RunMode::Selection).unwrap();
        assert!(t.sql().ends_with('世'), "{:?}", t.sql());
        assert_eq!(t.range().end, wide + '世'.len_utf8());
    }

    #[test]
    fn all_sends_the_whole_script_with_or_without_a_trailing_semicolon() {
        for script in ["SELECT 1; SELECT 2;", "SELECT 1; SELECT 2"] {
            let t = target(script, 0, RunMode::All).unwrap();
            assert_eq!(t.sql(), script);
            assert_eq!(t.range(), 0..script.len());
            assert_eq!(t.statement_index(), Some(0));
        }
        // The cursor does not matter for `All`.
        let script = "  SELECT 1;  ";
        let t = target(script, script.len(), RunMode::All).unwrap();
        assert_eq!(t.sql(), script);
    }

    #[test]
    fn there_is_nothing_to_run_for_blank_or_comment_only_text() {
        for script in ["", "   \n\t ", ";;", "-- only a comment\n/* and another */"] {
            assert!(target(script, 0, RunMode::All).is_none(), "all: {script:?}");
            assert!(
                target(script, 0, RunMode::CurrentStatement).is_none(),
                "current: {script:?}"
            );
            assert!(
                run_target(script, 0, Some(0..script.len()), kind(), RunMode::Selection).is_none(),
                "selection: {script:?}"
            );
        }
    }

    #[test]
    fn a_cursor_past_the_end_of_the_text_is_clamped() {
        let script = "SELECT 1; SELECT 2";
        let t = target(script, 10_000, RunMode::CurrentStatement).unwrap();
        assert_eq!(t.sql(), "SELECT 2");
    }

    // ---- where an engine error points --------------------------------------------------------

    #[test]
    fn an_error_offset_maps_into_the_second_statement() {
        let script = "SELECT 1;\nSELECT nope FROM t;";
        let at = script.find("nope").unwrap();
        let t = target(script, at, RunMode::CurrentStatement).unwrap();
        let position = t.sql().find("nope").unwrap();
        let span = t.error_span(script, Some(position));
        assert_eq!(span.start, at);
        assert_eq!(span.len, 4);
        assert_eq!(&script[span.start..span.end()], "nope");
    }

    #[test]
    fn an_error_offset_never_splits_a_utf8_char() {
        let script = "SELECT 1;\nSELECT '世界' FROM t;";
        let wide = script.find('世').unwrap();
        let t = target(script, wide, RunMode::CurrentStatement).unwrap();
        assert_eq!(t.start(), script.find("SELECT '世").unwrap());
        let inside = wide - t.start();
        let span = t.error_span(script, Some(inside));
        assert_eq!(span.start, wide);
        assert_eq!(span.len, "世界".len(), "the whole token, in whole chars");
        assert_eq!(&script[span.start..span.end()], "世界");

        let mid = t.error_span(script, Some(inside + 1));
        assert_eq!(
            (mid.start, mid.len),
            (wide, "世界".len()),
            "a mid-char offset moves back"
        );

        let past = t.error_span(script, Some(10_000));
        assert_eq!(past.start, script.len());
        assert_eq!(past.len, 0, "past the end there is nothing to underline");
    }

    #[test]
    fn an_error_on_punctuation_still_underlines_something() {
        let script = "SELECT (1 + FROM t;";
        let t = target(script, 0, RunMode::CurrentStatement).unwrap();
        let paren = t.sql().find('(').unwrap();
        let span = t.error_span(script, Some(paren));
        assert_eq!(span.start, paren);
        assert_eq!(span.len, 1, "a lone `(` is underlined on its own");
    }

    #[test]
    fn an_error_offset_in_a_statement_that_starts_with_a_comment() {
        let script = "SELECT 1;\n-- why\nSELECT nope FROM t;";
        let at = script.find("nope").unwrap();
        let t = target(script, at, RunMode::CurrentStatement).unwrap();
        assert_eq!(
            t.start(),
            script.find("-- why").unwrap(),
            "the leading comment belongs to the statement, so the sent SQL starts there"
        );
        let span = t.error_span(script, Some(t.sql().find("nope").unwrap()));
        assert_eq!(span.start, at);
        assert_eq!(span.len, 4);
    }

    #[test]
    fn an_error_without_a_position_underlines_the_whole_statement() {
        let script = "SELECT 1; SELECT nope;";
        let t = target(
            script,
            script.find("nope").unwrap(),
            RunMode::CurrentStatement,
        )
        .unwrap();
        let span = t.error_span(script, None);
        assert_eq!(span.start, t.start());
        assert_eq!(span.len, t.len());
        assert_eq!(&script[span.start..span.end()], "SELECT nope");
    }

    // ---- accumulating one run ----------------------------------------------------------------

    #[test]
    fn each_result_set_keeps_the_index_the_driver_reported() {
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        assert!(results.apply_event(generation, start(0)));
        assert!(results.apply_event(generation, Event::Rows(vec![row(1), row(2)])));
        assert!(results.apply_event(generation, end(false, 3)));
        assert!(results.apply_event(generation, start_dml(1)));
        assert!(results.apply_event(
            generation,
            Event::ResultEnd {
                affected_rows: Some(1),
                last_insert_id: Some(9),
                truncated: false,
                elapsed_ms: 5,
                warnings: vec!["careful".into()],
            }
        ));

        assert_eq!(results.sets().len(), 2);
        assert_eq!(results.sets()[0].index, 0);
        assert_eq!(results.sets()[1].index, 1);
        assert!(results.sets()[0].returns_rows());
        assert!(!results.sets()[1].returns_rows());
        assert_eq!(results.sets()[1].affected_rows, Some(1));
        assert_eq!(results.total_rows(), 2);
        assert_eq!(results.statements_run(), 2);
        assert_eq!(results.elapsed_ms(), Some(8));
        assert!(!results.truncated());
        assert_eq!(results.status(), RunStatus::Running);
        assert!(results.finish(generation));
        assert_eq!(results.status(), RunStatus::Finished);
        assert_eq!(results.set(1).map(|s| s.index), Some(1));
        assert!(results.set(2).is_none());
    }

    #[test]
    fn rows_are_counted_across_batches() {
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        results.apply_event(generation, start(0));
        for batch in [vec![row(1), row(2)], vec![row(3), row(4)], vec![row(5)]] {
            assert!(results.apply_event(generation, Event::Rows(batch)));
        }
        assert_eq!(results.sets()[0].row_count(), 5);
        assert_eq!(results.total_rows(), 5);
        assert!(!results.sets()[0].complete, "no ResultEnd yet");
    }

    #[test]
    fn result_end_records_the_per_set_metadata() {
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        results.apply_event(generation, start(0));
        assert_eq!(results.sets()[0].elapsed_ms, None);
        results.apply_event(
            generation,
            Event::ResultEnd {
                affected_rows: Some(7),
                last_insert_id: Some(42),
                truncated: true,
                elapsed_ms: 12,
                warnings: vec!["w".into()],
            },
        );
        let set = &results.sets()[0];
        assert_eq!(set.affected_rows, Some(7));
        assert_eq!(set.last_insert_id, Some(42));
        assert_eq!(set.elapsed_ms, Some(12));
        assert_eq!(set.warnings, ["w"]);
        assert!(set.complete);
    }

    #[test]
    fn notices_go_into_the_message_log() {
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        assert!(results.apply_event(generation, Event::Notice("relation exists".into())));
        assert!(results.apply_event(generation, start(0)));
        assert!(results.apply_event(generation, Event::Notice("second".into())));
        assert_eq!(results.notices(), ["relation exists", "second"]);
    }

    #[test]
    fn an_error_stops_the_run_and_maps_its_offset() {
        let script = "SELECT 1;\nSELECT nope FROM t;";
        let target = target(
            script,
            script.find("nope").unwrap(),
            RunMode::CurrentStatement,
        )
        .unwrap();
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        results.apply_event(generation, start(0));
        let position = target.sql().find("nope").unwrap();
        assert!(results.fail(
            generation,
            &query_error("no such column: nope", Some(position)),
            &target,
            script,
        ));

        assert!(
            !results.apply_event(generation, Event::Rows(vec![row(1)])),
            "a failed run accepts no more rows"
        );
        assert!(!results.apply_event(generation, start(1)));
        assert!(!results.finish(generation));

        let failure = results.error().unwrap();
        assert_eq!(failure.message, "no such column: nope");
        assert_eq!(failure.code.as_deref(), Some("1"));
        let span = failure.span.unwrap();
        assert_eq!(span.start, script.find("nope").unwrap());
        assert_eq!(&script[span.start..span.end()], "nope");
        assert_eq!(results.status(), RunStatus::Failed);
        assert!(!results.cancelled());
    }

    #[test]
    fn a_failure_that_is_not_about_a_statement_has_no_underline() {
        let script = "SELECT 1;";
        let target = target(script, 0, RunMode::CurrentStatement).unwrap();
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        assert!(results.fail(
            generation,
            &Error::Connection("gone".into()),
            &target,
            script
        ));
        let failure = results.error().unwrap();
        assert!(failure.span.is_none());
        assert!(failure.message.contains("gone"));
        assert_eq!(results.status(), RunStatus::Failed);
    }

    #[test]
    fn truncation_of_one_set_truncates_the_run() {
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        results.apply_event(generation, start(0));
        results.apply_event(generation, end(false, 1));
        results.apply_event(generation, start(1));
        results.apply_event(generation, end(true, 2));
        assert!(results.sets()[1].truncated);
        assert!(results.truncated(), "one truncated set marks the whole run");
        results.apply_event(generation, start(2));
        results.apply_event(generation, end(false, 3));
        assert!(
            results.truncated(),
            "a later complete set does not clear it"
        );
    }

    #[test]
    fn results_from_an_outdated_run_are_ignored() {
        let script = "SELECT 1;";
        let target = target(script, 0, RunMode::CurrentStatement).unwrap();
        let mut results = QueryResults::new();
        let old = results.begin_run();
        let new = results.begin_run(); // the user ran something else before the first answered

        assert!(!results.apply_event(old, start(0)));
        assert!(!results.apply_event(old, Event::Rows(vec![row(1)])));
        assert!(!results.apply_event(old, end(true, 99)));
        assert!(!results.apply_event(old, Event::Notice("stale".into())));
        assert!(!results.fail(old, &query_error("stale", None), &target, script));
        assert!(!results.finish(old));

        assert!(results.apply_event(new, start(0)));
        assert!(results.apply_event(new, Event::Rows(vec![row(2)])));
        assert!(results.apply_event(new, end(false, 1)));
        assert!(results.finish(new));

        assert_eq!(results.sets().len(), 1);
        assert_eq!(results.total_rows(), 1);
        assert!(results.notices().is_empty());
        assert!(results.error().is_none(), "the stale failure did not land");
        assert!(!results.truncated());
        assert_eq!(results.status(), RunStatus::Finished);
    }

    #[test]
    fn a_refusal_carries_the_offending_statement_from_the_plan() {
        let script = "SELECT 1; DROP TABLE t;";
        let planned = plan(script, kind(), SafeMode::ReadOnly);
        let refusal = planned.first_refusal().unwrap();
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        assert!(results.refuse(generation, &planned, refusal));

        let info = results.refusal().unwrap();
        assert_eq!(info.statement, "DROP TABLE t");
        assert_eq!(info.statement_index(), 1);
        assert_eq!(info.class(), StatementClass::Ddl);
        assert!(matches!(info.refusal, PolicyRefusal::ReadOnly { .. }));
        assert_eq!(results.planned_statements(), 2);
        assert_eq!(results.statements_run(), 0);
        assert_eq!(results.status(), RunStatus::Refused);
        assert!(!results.apply_event(generation, start(0)));
        assert!(!results.finish(generation));
    }

    #[test]
    fn a_confirmation_demand_keeps_its_danger_reason() {
        let script = "DELETE FROM t";
        let planned = plan(script, kind(), SafeMode::Alert);
        let refusal = planned.first_refusal().unwrap();
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        results.refuse(generation, &planned, refusal);
        let info = results.refusal().unwrap();
        assert_eq!(info.statement, "DELETE FROM t");
        assert_eq!(info.danger(), Some(DangerReason::DeleteWithoutWhere));
    }

    #[test]
    fn a_cancelled_run_is_reported_as_cancelled_not_failed() {
        let script = "SELECT 1;";
        let target = target(script, 0, RunMode::CurrentStatement).unwrap();
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        assert!(results.fail(generation, &Error::Cancelled, &target, script));
        assert!(results.cancelled());
        assert!(results.error().is_none());
        assert_eq!(results.status(), RunStatus::Cancelled);

        let mut explicit = QueryResults::new();
        let generation = explicit.begin_run();
        assert!(explicit.cancel(generation));
        assert!(explicit.cancelled());
        assert_eq!(explicit.status(), RunStatus::Cancelled);
    }

    #[test]
    fn a_fresh_tab_is_idle_and_accepts_nothing() {
        let mut results = QueryResults::new();
        assert_eq!(results.status(), RunStatus::Idle);
        assert!(!results.is_running());
        assert!(!results.apply_event(0, start(0)));
        assert_eq!(results.total_rows(), 0);
        assert_eq!(results.elapsed_ms(), None);
        let generation = results.begin_run();
        assert_eq!(results.generation(), generation);
        assert!(results.is_running());
        assert_eq!(results.status(), RunStatus::Running);
    }

    #[test]
    fn the_planned_count_is_set_by_the_view_and_kept_by_a_refusal() {
        let mut results = QueryResults::new();
        let generation = results.begin_run();
        assert!(results.set_planned_statements(generation, 3));
        assert_eq!(results.planned_statements(), 3);
        assert!(!results.set_planned_statements(generation + 1, 9), "stale");
        assert_eq!(results.planned_statements(), 3);
    }

    // ---- document identity ------------------------------------------------------------------

    fn document() -> QueryDocument {
        QueryDocument {
            id: DocumentId::from_string("d1"),
            connection_id: ConnectionId::from_string("c1"),
            title: "scratch".into(),
            content: "SELECT 1".into(),
            saved_path: Some("/tmp/q.sql".into()),
            dirty: true,
            cursor_offset: Some(8),
            created_at: UnixMillis(1_000),
            updated_at: UnixMillis(2_000),
        }
    }

    #[test]
    fn a_document_roundtrip_keeps_created_at_and_stamps_the_save_time() {
        let original = document();
        let mut state = QueryDocumentState::from_document(&original);
        assert_eq!(state.id(), &original.id);
        assert_eq!(state.connection_id(), &original.connection_id);
        assert_eq!(state.title(), "scratch");
        assert!(state.dirty());
        assert_eq!(state.cursor_offset(), 8);
        assert_eq!(state.saved_path(), Some("/tmp/q.sql"));
        assert_eq!(state.created_at(), UnixMillis(1_000));

        state.set_cursor_offset(3);
        let saved = state.to_document("SELECT 2", UnixMillis(5_000));
        assert_eq!(saved.id, original.id);
        assert_eq!(saved.connection_id, original.connection_id);
        assert_eq!(
            saved.created_at,
            UnixMillis(1_000),
            "creation time survives"
        );
        assert_eq!(
            saved.updated_at,
            UnixMillis(5_000),
            "updated_at is the save time"
        );
        assert_eq!(saved.content, "SELECT 2");
        assert_eq!(saved.cursor_offset, Some(3));
        assert!(saved.dirty);
        assert_eq!(saved.saved_path.as_deref(), Some("/tmp/q.sql"));
        assert_eq!(saved.title, "scratch");
    }

    #[test]
    fn a_new_document_is_clean_and_keeps_its_identity_across_saves() {
        let mut state =
            QueryDocumentState::new(ConnectionId::from_string("c1"), "Query 1", UnixMillis(7));
        assert!(!state.dirty());
        assert_eq!(state.cursor_offset(), 0);
        assert_eq!(state.saved_path(), None);

        let first = state.to_document("", UnixMillis(9));
        assert_eq!(first.created_at, UnixMillis(7));
        assert_eq!(first.cursor_offset, Some(0));
        assert!(!first.dirty);

        state.mark_dirty();
        assert!(state.dirty());
        state.set_title("renamed");
        state.set_saved_path(Some("/tmp/x.sql".into()));
        let second = state.to_document("SELECT 1", UnixMillis(11));
        assert_eq!(
            second.id, first.id,
            "the same draft is updated, not replaced"
        );
        assert_eq!(second.created_at, UnixMillis(7));
        assert_eq!(second.title, "renamed");
        assert!(second.dirty);

        state.mark_clean();
        assert!(!state.dirty());
    }

    #[test]
    fn a_saved_cursor_offset_that_does_not_fit_saturates() {
        let mut document = document();
        document.cursor_offset = Some(u64::MAX);
        let state = QueryDocumentState::from_document(&document);
        assert_eq!(state.cursor_offset(), usize::MAX);
    }
}
