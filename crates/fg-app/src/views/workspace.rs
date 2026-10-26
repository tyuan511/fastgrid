//! A connection's window: title bar, sidebar with the object tree, tab strip and the active tab.
//!
//! The view owns the UI state (tabs, tree, filter). Anything that needs the database goes through
//! events ([`WorkspaceEvent`]) answered by [`crate::controller`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use fg_core::{
    ConnectionConfig, ConnectionParams, ObjectInfo, ObjectKind, ObjectPath, Outcome,
    QueryHistoryEntry, SafeMode, TabId, TabKind, TableQuery, TableViewMode, TableViewState,
    WorkspaceState,
};
use fg_service::{
    ColumnMapping, CsvImportOptions, CsvImportReport, CsvPreview, ExportFormat, ExportOptions,
    ExportReport, ExportScope, ImportErrorPolicy, OpenConnection, Service, preview_csv,
};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    CloseTab, CommandPalette, CycleSafeMode, CycleTheme, DismissTopmost, ExportWizard,
    HISTORY_CONTEXT, HistoryDismiss, HistoryNext, HistoryOpen, HistoryPrev, ImportWizard, NewQuery,
    NextTab, PrevTab, QuickOpen, RefreshSchema, SNIPPETS_CONTEXT, SaveSnippet, SnippetDismiss,
    SnippetOpen, SwitchDatabase, ToggleAudit, ToggleDetails, ToggleFilters, ToggleHistory,
    ToggleSidebar, ToggleSnippets, WORKSPACE_CONTEXT,
};
use super::grid::{TableTab, TableTabEvent};
use super::icons::{Hugeicon, hugeicon};
use super::query_tab::{QueryTab, QueryTabEvent};
use crate::model::{
    GridQuery, NodeKind, QueryDocumentState, QuickItem, RunMode, TabSet, TreeNode, build_tree,
    compact_count, quick_items, quick_search, table_title,
};

/// A tab being dragged; also the little label that follows the pointer.
#[derive(Clone)]
struct DraggedTab {
    id: TabId,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.tab_active)
            .text_size(px(14.))
            .shadow_md()
            .child(self.title.clone())
    }
}

/// What the workspace asks its owner to do.
pub enum WorkspaceEvent {
    /// A table tab was created: the controller attaches its data loader.
    TableOpened {
        path: ObjectPath,
        tab: Entity<TableTab>,
    },
    /// Reload the schema (the refresh button).
    RefreshSchema,
    /// `⌘K`: the controller loads the server's databases into the switcher.
    SwitchDatabaseRequested,
    /// The user picked a database: the controller reconnects to it.
    SwitchDatabase { database: String },
    /// A query tab was created: the controller supplies its draft and streams its runs.
    QueryOpened { tab: Entity<QueryTab> },
    /// `⌘T`: the controller creates the draft (it owns ids and timestamps) and opens the tab.
    NewQueryRequested,
    /// The export wizard's Run: the controller owns the connection, so it
    /// runs the export and reports back through `set_export_finished`.
    ExportRequested {
        source: ExportSource,
        options: ExportOptions,
        path: String,
    },
    /// The export wizard's Cancel: the controller flags the run's control.
    ExportCancel,
    /// The import wizard's Run: the controller owns the connection, so
    /// it runs the import and reports back through `set_import_finished`.
    ImportRequested {
        table: ObjectPath,
        mappings: Vec<ColumnMapping>,
        options: CsvImportOptions,
        path: String,
    },
    /// The import wizard's Cancel: the controller flags the run's control.
    ImportCancel,
    /// Answer an agent write waiting in the approvals panel. The
    /// controller resolves it through the service; the panel refreshes
    /// from the remaining pending list.
    McpWriteResolve { id: u64, approved: bool },
    /// Run `mode` in the query tab. The target is read from the buffer when this is handled, so
    /// what runs is what the editor holds at that moment.
    QueryRun {
        tab: Entity<QueryTab>,
        mode: RunMode,
    },
    /// Stop the statement a query tab is running.
    QueryCancel { tab: Entity<QueryTab> },
    /// A query buffer changed: debounce and persist the draft.
    QueryDraftChanged { tab: Entity<QueryTab> },
    /// The user confirmed a dangerous statement; `script` is the exact text to run.
    QueryConfirmed {
        tab: Entity<QueryTab>,
        script: String,
    },
    /// `⌘⇧S` / the snippets button: load the saved fragments.
    SnippetsRequested,
    /// Save the current statement (or the selection) as a snippet under `title`.
    SnippetSave { title: String, sql: String },
    /// Delete a snippet.
    SnippetDelete { id: fg_core::SnippetId },
    /// Explain the statement in a query tab instead of running it.
    QueryExplain { tab: Entity<QueryTab> },
    /// Commit the staged edits of a query tab's editable result set.
    QueryResultCommit { tab: Entity<QueryTab> },
    /// `⌘Y` / the history button: load the recent statements of this connection.
    HistoryRequested { search: String },
    /// The audit button: load the newest audit rows for this connection.
    AuditRequested,
    /// The audit panel's Export: the controller writes the loaded rows
    /// as CSV and reports back through `set_audit_export_status`.
    AuditExportRequested { path: String },
    /// Put a history entry into the active query tab (the user picked it).
    HistoryPicked { sql: String },
    /// The lock button: switch the connection's guard level, and persist it.
    SafeModeChanged { mode: SafeMode },
    /// The theme button: system, light, dark.
    CycleTheme,
    /// Tabs or layout changed: worth saving.
    SessionChanged,
}

/// What the tab strip is showing. A tab is either a table's grid or a query editor; both live in
/// the same strip and the same saved session, but their views share no state.
#[derive(Clone)]
pub enum TabContent {
    Table(Entity<TableTab>),
    Query(Entity<QueryTab>),
}

/// What the sidebar needs to draw and activate one tree row.
struct NodeMeta {
    kind: NodeKind,
    path: Option<ObjectPath>,
    row_estimate: Option<u64>,
    /// Matched character positions in the name, for bold highlighting.
    matched: Vec<usize>,
}

/// Sidebar width: default, and the range the drag handle allows.
const SIDEBAR_DEFAULT: f32 = 220.0;
const SIDEBAR_MIN: f32 = 140.0;
const SIDEBAR_MAX: f32 = 450.0;

enum SchemaState {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct WorkspaceView {
    config: ConnectionConfig,
    schema: SchemaState,
    namespaces: Vec<String>,
    objects: BTreeMap<String, Vec<ObjectInfo>>,
    filter: Entity<InputState>,
    tree: Entity<TreeState>,
    details_filter: Entity<InputState>,
    /// Tree node id -> the table it stands for, so a click can be resolved.
    meta: Rc<HashMap<SharedString, NodeMeta>>,
    split: Entity<ResizableState>,
    sidebar_width: f32,
    collapsed: HashSet<SharedString>,
    tabs: TabSet,
    tab_views: HashMap<TabId, TabContent>,
    /// A tab whose close was requested while it still had staged edits, and
    /// is waiting for the second click in the confirmation banner.
    confirm_close: Option<TabId>,
    sidebar_visible: bool,
    details_visible: bool,
    focus: FocusHandle,
    quick: Option<QuickOpenState>,
    /// The `⌘⇧P` palette: workspace commands by name. Only workspace-level
    /// commands are listed: confirming an item dispatches its action from
    /// the palette overlay, which bubbles to this view, so grid- and
    /// editor-scoped actions (they live in other branches) cannot be
    /// reached from here and keep their keys instead.
    commands: Option<CommandPaletteState>,
    /// The `⌘K` panel: databases on this server, for switching without reopening.
    switcher: Option<SwitchDatabaseState>,
    /// The `⌘Y` panel: recent statements of this connection, newest first.
    history: Option<HistoryPanel>,
    /// The snippets panel (`⌘⇧S`): saved fragments, per connection plus global ones.
    snippets: Option<SnippetsPanel>,
    /// The audit panel: agent and GUI calls, newest first, filterable
    /// locally. Same overlay shape as history.
    audit: Option<AuditPanel>,
    /// The export wizard (`⇧⌘E`): a panel above the tabs, not a tab.
    export_wizard: Option<ExportWizardState>,
    /// The import wizard (`⇧⌘I`): same shape, for a table tab.
    import_wizard: Option<ImportWizardState>,
    /// Agent writes waiting for a person. The panel shows while this
    /// is non-empty; every entry must be answered, never dismissed
    /// unseen, because the agent's run is parked on it.
    mcp_approvals: Vec<fg_service::McpWriteRequest>,
    /// What the connection is set to, as far as this window knows. `None` until the controller
    /// tells us; the controller is the one that persists a change.
    safe_mode: SafeMode,
    _subscriptions: Vec<Subscription>,
}

/// What an export reads: a table's current query (filters and sort
/// apply, paging does not) or one statement from a query tab.
#[derive(Debug, Clone)]
pub enum ExportSource {
    Table(TableQuery),
    Sql(String),
}

/// The export wizard's state. The panel lives above the tabs; the
/// controller runs the export and reports back, so this state only
/// holds the form and the latest status.
struct ExportWizardState {
    source: ExportSource,
    path: Entity<InputState>,
    format: ExportFormat,
    scope: ExportScope,
    gzip: bool,
    status: ExportWizardStatus,
    /// Rows written so far, refreshed while running.
    rows: u64,
    /// The file of the last finished run, for the Done message.
    last_path: Option<String>,
}

#[derive(Debug, Clone)]
enum ExportWizardStatus {
    Idle,
    Running,
    Done { rows: u64, bytes: u64 },
    Failed(String),
}

/// The import wizard's state. Preview and mapping are workspace-local
/// (the preview parses only the file's head); the run itself goes
/// through the controller, which owns the connection.
struct ImportWizardState {
    table: ObjectPath,
    /// The table's columns, snapshotted at open time for the mapping
    /// prefill (case-insensitive match against the CSV header).
    table_columns: Vec<String>,
    path: Entity<InputState>,
    has_header: bool,
    preview: Option<CsvPreview>,
    preview_error: Option<String>,
    /// One target-column input per CSV column, in CSV order. Empty
    /// means "skip this CSV column".
    mapping_inputs: Vec<Entity<InputState>>,
    policy: ImportErrorPolicy,
    status: ImportWizardStatus,
}

#[derive(Debug, Clone)]
enum ImportWizardStatus {
    Idle,
    Running,
    Done(CsvImportReport),
    Failed(String),
}

/// The open audit panel. The search filters the loaded rows locally
/// (actor, connection, statement substring); the export writes those
/// same rows, so what you see is what the file holds.
struct AuditPanel {
    search: Entity<InputState>,
    entries: Vec<fg_core::AuditEntry>,
    loading: bool,
    export_path: Entity<InputState>,
    export_status: Option<String>,
}

/// The open snippets panel. Same shape as the history one: a search box and a list.
struct SnippetsPanel {
    search: Entity<InputState>,
    entries: Vec<fg_core::Snippet>,
    /// Ids of records this version cannot read, listed but not editable.
    unreadable: Vec<fg_core::SnippetId>,
    loading: bool,
    selected: usize,
}

/// The open history panel: its search box and the entries the controller loaded.
struct HistoryPanel {
    search: Entity<InputState>,
    entries: Vec<QueryHistoryEntry>,
    loading: bool,
    /// Which entry is highlighted; Enter inserts it into the active query tab.
    selected: usize,
}

/// The open `⌘P` palette: its search state and the objects it can jump to.
struct QuickOpenState {
    palette: Entity<CommandState>,
    all: Vec<QuickItem>,
    query: String,
}

/// The open `⌘⇧P` palette: just its widget. The command list is static,
/// so the widget's own substring filter is enough and no query state is
/// kept here (unlike Quick Open, which ranks its own matches).
struct CommandPaletteState {
    palette: Entity<CommandState>,
}

/// How many matches the palette lists.
const QUICK_RESULTS: usize = 50;

/// The open `⌘K` palette: its search state, the databases the controller
/// loaded, and any error from loading or switching.
struct SwitchDatabaseState {
    palette: Entity<CommandState>,
    all: Vec<String>,
    query: String,
    loading: bool,
    /// A switch is in flight: Enter is ignored until it resolves.
    switching: bool,
    error: Option<SharedString>,
}

impl SwitchDatabaseState {
    /// Best matches first, using our fuzzy ranking.
    fn shown(&self) -> Vec<String> {
        crate::model::rank(&self.query, &self.all)
            .into_iter()
            .map(|(i, _)| self.all[i].clone())
            .collect()
    }
}

impl QuickOpenState {
    /// Best matches first, using our fuzzy ranking (the palette's own filter is substring-only).
    fn shown(&self) -> Vec<QuickItem> {
        quick_search(&self.all, &self.query, QUICK_RESULTS)
            .into_iter()
            .map(|(item, _)| item)
            .collect()
    }
}

impl EventEmitter<WorkspaceEvent> for WorkspaceView {}

impl WorkspaceView {
    pub fn new(config: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search for item..."));
        let tree = cx.new(|cx| TreeState::new(cx));
        let details_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search for field..."));
        let on_filter = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.rebuild_tree(cx);
            }
        });
        let on_tree = cx.subscribe(&tree, |this, _, event: &TreeEvent, cx| {
            match event {
                TreeEvent::Collapsed(id) => this.collapsed.insert(id.clone()),
                TreeEvent::Expanded(id) => this.collapsed.remove(id),
            };
            cx.notify();
        });
        let on_details = cx.subscribe(&details_filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let config_safe_mode = config.effective_safe_mode();
        Self {
            config,
            schema: SchemaState::Loading,
            namespaces: Vec::new(),
            objects: BTreeMap::new(),
            filter,
            tree,
            details_filter,
            meta: Rc::new(HashMap::new()),
            split: cx.new(|_| ResizableState::default()),
            sidebar_width: SIDEBAR_DEFAULT,
            collapsed: HashSet::new(),
            tabs: TabSet::new(),
            tab_views: HashMap::new(),
            confirm_close: None,
            sidebar_visible: true,
            details_visible: true,
            focus: cx.focus_handle(),
            quick: None,
            commands: None,
            switcher: None,
            history: None,
            snippets: None,
            audit: None,
            export_wizard: None,
            import_wizard: None,
            mcp_approvals: Vec::new(),
            safe_mode: config_safe_mode,
            _subscriptions: vec![on_filter, on_tree, on_details],
        }
    }

    // ---- export wizard ---------------------------------------------------------------

    /// Open the wizard for the active tab: a table tab exports its query,
    /// a query tab exports its script. Anything else opens nothing.
    pub fn open_export_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = match self.tabs.active().and_then(|t| self.tab_views.get(&t.id)) {
            Some(TabContent::Table(tab)) => {
                ExportSource::Table(tab.read(cx).query().to_table_query())
            }
            Some(TabContent::Query(tab)) => ExportSource::Sql(tab.read(cx).script(cx)),
            None => return,
        };
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("File path…"));
        self.export_wizard = Some(ExportWizardState {
            source,
            path,
            format: ExportFormat::Csv,
            scope: ExportScope::Data,
            gzip: false,
            status: ExportWizardStatus::Idle,
            rows: 0,
            last_path: None,
        });
        cx.notify();
    }

    pub fn close_export_wizard(&mut self, cx: &mut Context<Self>) {
        self.export_wizard = None;
        cx.notify();
    }

    /// Replace the approvals panel with the service's current pending
    /// list. An empty list hides the panel.
    pub fn set_mcp_approvals(
        &mut self,
        list: Vec<fg_service::McpWriteRequest>,
        cx: &mut Context<Self>,
    ) {
        self.mcp_approvals = list;
        cx.notify();
    }

    /// Validate the form and hand the run to the controller.
    fn run_export(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.export_wizard.as_mut() else {
            return;
        };
        if matches!(wizard.status, ExportWizardStatus::Running) {
            return;
        }
        let path = wizard.path.read(cx).value().trim().to_owned();
        if path.is_empty() {
            wizard.status = ExportWizardStatus::Failed("Enter a file path.".into());
            cx.notify();
            return;
        }
        let options = ExportOptions {
            format: wizard.format,
            scope: wizard.scope,
            gzip: wizard.gzip,
            sql_batch: 100,
        };
        wizard.status = ExportWizardStatus::Running;
        wizard.rows = 0;
        wizard.last_path = Some(path.clone());
        cx.notify();
        cx.emit(WorkspaceEvent::ExportRequested {
            source: wizard.source.clone(),
            options,
            path,
        });
    }

    /// Rows written so far, refreshed by the controller while running.
    pub fn set_export_rows(&mut self, rows: u64, cx: &mut Context<Self>) {
        if let Some(wizard) = self.export_wizard.as_mut()
            && matches!(wizard.status, ExportWizardStatus::Running)
        {
            wizard.rows = rows;
            cx.notify();
        }
    }

    /// The controller's report for the finished run.
    pub fn set_export_finished(
        &mut self,
        result: std::result::Result<ExportReport, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(wizard) = self.export_wizard.as_mut() {
            wizard.status = match result {
                Ok(report) => ExportWizardStatus::Done {
                    rows: report.rows,
                    bytes: report.bytes,
                },
                Err(message) => ExportWizardStatus::Failed(message),
            };
            cx.notify();
        }
    }

    // ---- import wizard ---------------------------------------------------------------

    /// Open the wizard for the active table tab. Query tabs cannot
    /// import: there is no target table.
    pub fn open_import_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TabContent::Table(tab)) =
            self.tabs.active().and_then(|t| self.tab_views.get(&t.id))
        else {
            return;
        };
        let table = tab.read(cx).query().path().clone();
        let table_columns = tab.read(cx).column_names(cx);
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("CSV file path…"));
        self.import_wizard = Some(ImportWizardState {
            table,
            table_columns,
            path,
            has_header: true,
            preview: None,
            preview_error: None,
            mapping_inputs: Vec::new(),
            policy: ImportErrorPolicy::Abort,
            status: ImportWizardStatus::Idle,
        });
        cx.notify();
    }

    pub fn close_import_wizard(&mut self, cx: &mut Context<Self>) {
        self.import_wizard = None;
        cx.notify();
    }

    /// Read the file's head and build one mapping input per CSV column,
    /// prefilled by case-insensitive match against the table's columns.
    fn load_import_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(wizard) = self.import_wizard.as_mut() else {
            return;
        };
        let path = wizard.path.read(cx).value().trim().to_owned();
        if path.is_empty() {
            wizard.preview_error = Some("Enter a CSV file path.".into());
            cx.notify();
            return;
        }
        let has_header = wizard.has_header;
        match std::fs::read_to_string(&path).and_then(|text| {
            preview_csv(&text, has_header)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
        }) {
            Ok(preview) => {
                let mut inputs = Vec::new();
                for csv_name in &preview.columns {
                    let prefill = wizard
                        .table_columns
                        .iter()
                        .find(|c| c.eq_ignore_ascii_case(csv_name))
                        .cloned()
                        .unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut state = InputState::new(window, cx);
                        if !prefill.is_empty() {
                            state.set_value(&prefill, window, cx);
                        }
                        state
                    });
                    inputs.push(input);
                }
                wizard.mapping_inputs = inputs;
                wizard.preview = Some(preview);
                wizard.preview_error = None;
            }
            Err(e) => {
                wizard.preview = None;
                wizard.mapping_inputs = Vec::new();
                wizard.preview_error = Some(format!("Could not preview: {e}"));
            }
        }
        cx.notify();
    }

    /// Validate the form and hand the run to the controller.
    fn run_import(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.import_wizard.as_mut() else {
            return;
        };
        if matches!(wizard.status, ImportWizardStatus::Running) {
            return;
        }
        let path = wizard.path.read(cx).value().trim().to_owned();
        if path.is_empty() {
            wizard.status = ImportWizardStatus::Failed("Enter a CSV file path.".into());
            cx.notify();
            return;
        }
        if wizard.preview.is_none() {
            wizard.status = ImportWizardStatus::Failed("Load a preview first.".into());
            cx.notify();
            return;
        }
        let mappings = wizard
            .mapping_inputs
            .iter()
            .enumerate()
            .map(|(i, input)| {
                let target = input.read(cx).value().trim().to_owned();
                ColumnMapping {
                    csv_index: i,
                    column: if target.is_empty() {
                        None
                    } else {
                        Some(target)
                    },
                }
            })
            .collect();
        let options = CsvImportOptions {
            has_header: wizard.has_header,
            batch_rows: 1000,
            error_policy: wizard.policy,
            cancel: None,
        };
        wizard.status = ImportWizardStatus::Running;
        cx.notify();
        cx.emit(WorkspaceEvent::ImportRequested {
            table: wizard.table.clone(),
            mappings,
            options,
            path,
        });
    }

    /// The controller's report for the finished run.
    pub fn set_import_finished(
        &mut self,
        result: std::result::Result<CsvImportReport, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(wizard) = self.import_wizard.as_mut() {
            wizard.status = match result {
                Ok(report) => ImportWizardStatus::Done(report),
                Err(message) => ImportWizardStatus::Failed(message),
            };
            cx.notify();
        }
    }

    /// Give keyboard focus to the window so shortcuts work straight away.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    // ---- quick open ---------------------------------------------------------------------------

    pub fn quick_open_visible(&self) -> bool {
        self.quick.is_some()
    }

    // ---- query history ------------------------------------------------------------------------

    pub fn history_visible(&self) -> bool {
        self.history.is_some()
    }

    /// Show the panel and ask for the newest entries. Re-opening with the same search keeps the
    /// list the controller already delivered instead of flashing an empty one.
    pub fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.is_some() {
            self.close_history(cx);
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search statements..."));
        // Only `Change` matters here: Enter in the box is the `HistoryOpen` shortcut's job, and
        // handling it here as well would insert the statement twice.
        let on_search = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = this
                    .history
                    .as_ref()
                    .map(|h| h.search.read(cx).value().to_string())
                    .unwrap_or_default();
                if let Some(panel) = this.history.as_mut() {
                    panel.loading = true;
                }
                cx.emit(WorkspaceEvent::HistoryRequested { search: text });
                cx.notify();
            }
        });
        self._subscriptions.push(on_search);
        self.history = Some(HistoryPanel {
            search,
            entries: Vec::new(),
            loading: true,
            selected: 0,
        });
        cx.emit(WorkspaceEvent::HistoryRequested {
            search: String::new(),
        });
        cx.notify();
    }

    pub fn close_history(&mut self, cx: &mut Context<Self>) {
        if self.history.take().is_some() {
            cx.notify();
        }
    }

    // ---- audit log -------------------------------------------------------------------

    pub fn audit_visible(&self) -> bool {
        self.audit.is_some()
    }

    /// Show the panel and ask for the newest rows. The search box
    /// filters locally: typing re-renders, it never reloads, so the
    /// filter cannot hide rows the export would still write — the
    /// export writes exactly the filtered list.
    pub fn open_audit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.audit.is_some() {
            self.close_audit(cx);
            return;
        }
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter actor, connection, statement...")
        });
        let on_search = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        self._subscriptions.push(on_search);
        let export_path =
            cx.new(|cx| InputState::new(window, cx).placeholder("Export CSV path..."));
        self.audit = Some(AuditPanel {
            search,
            entries: Vec::new(),
            loading: true,
            export_path,
            export_status: None,
        });
        cx.emit(WorkspaceEvent::AuditRequested);
        cx.notify();
    }

    pub fn close_audit(&mut self, cx: &mut Context<Self>) {
        if self.audit.take().is_some() {
            cx.notify();
        }
    }

    pub fn set_audit(&mut self, entries: Vec<fg_core::AuditEntry>, cx: &mut Context<Self>) {
        if let Some(panel) = self.audit.as_mut() {
            panel.entries = entries;
            panel.loading = false;
        }
        cx.notify();
    }

    pub fn set_audit_export_status(&mut self, status: String, cx: &mut Context<Self>) {
        if let Some(panel) = self.audit.as_mut() {
            panel.export_status = Some(status);
        }
        cx.notify();
    }

    /// The rows the panel currently shows, after the local filter.
    /// The controller's export writes exactly these.
    pub fn audit_entries(&self, cx: &App) -> Vec<fg_core::AuditEntry> {
        let Some(panel) = self.audit.as_ref() else {
            return Vec::new();
        };
        let filter = panel.search.read(cx).value().to_string();
        filter_audit_entries(&panel.entries, &filter)
    }

    /// The controller's answer. Keeps the selection when the entry is still listed.
    pub fn set_history(&mut self, entries: Vec<QueryHistoryEntry>, cx: &mut Context<Self>) {
        if let Some(panel) = self.history.as_mut() {
            panel.entries = entries;
            panel.loading = false;
            panel.selected = panel.selected.min(panel.entries.len().saturating_sub(1));
        }
        cx.notify();
    }

    pub fn history_entries(&self) -> &[QueryHistoryEntry] {
        self.history
            .as_ref()
            .map(|h| h.entries.as_slice())
            .unwrap_or(&[])
    }

    pub fn select_history(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(panel) = self.history.as_mut()
            && index < panel.entries.len()
        {
            panel.selected = index;
            cx.notify();
        }
    }

    /// Enter in the panel: put the statement into the active query tab.
    pub fn pick_history(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.history.as_ref() else {
            return;
        };
        let Some(entry) = panel.entries.get(panel.selected) else {
            return;
        };
        let sql = entry.sql.clone();
        cx.emit(WorkspaceEvent::HistoryPicked { sql });
        self.close_history(cx);
    }

    fn history_next(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(panel) = self.history.as_mut() else {
            return;
        };
        if panel.entries.is_empty() {
            return;
        }
        let last = panel.entries.len() - 1;
        panel.selected = if delta < 0 {
            panel.selected.saturating_sub(1)
        } else {
            (panel.selected + 1).min(last)
        };
        cx.notify();
    }

    // ---- snippets -----------------------------------------------------------------------------

    pub fn snippets_visible(&self) -> bool {
        self.snippets.is_some()
    }

    /// Toggle the snippets panel and ask for the list.
    pub fn open_snippets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippets.take().is_some() {
            cx.notify();
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search snippets..."));
        let on_search = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        self._subscriptions.push(on_search);
        self.snippets = Some(SnippetsPanel {
            search,
            entries: Vec::new(),
            unreadable: Vec::new(),
            loading: true,
            selected: 0,
        });
        cx.emit(WorkspaceEvent::SnippetsRequested);
        cx.notify();
    }

    /// Ask the controller to reload the list (after a save or a delete).
    pub fn request_snippets(&mut self, cx: &mut Context<Self>) {
        cx.emit(WorkspaceEvent::SnippetsRequested);
        cx.notify();
    }

    pub fn close_snippets(&mut self, cx: &mut Context<Self>) {
        if self.snippets.take().is_some() {
            cx.notify();
        }
    }

    pub fn set_snippets(
        &mut self,
        entries: Vec<fg_core::Snippet>,
        unreadable: Vec<fg_core::SnippetId>,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.snippets.as_mut() {
            panel.entries = entries;
            panel.unreadable = unreadable;
            panel.loading = false;
            panel.selected = panel.selected.min(panel.entries.len().saturating_sub(1));
        }
        cx.notify();
    }

    /// Set the panel's search text (the view owns the input, so a caller cannot reach into it).
    pub fn search_snippets(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.snippets.as_ref() else {
            return;
        };
        let search = panel.search.clone();
        search.update(cx, |input, cx| input.set_value(query, window, cx));
        cx.notify();
    }

    /// Titles of the snippets the panel currently shows, for tests and for the "N shown" label.
    pub fn snippet_titles(&self, cx: &App) -> Vec<String> {
        self.visible_snippets(cx)
            .into_iter()
            .filter_map(|ix| {
                self.snippets
                    .as_ref()?
                    .entries
                    .get(ix)
                    .map(|s| s.title.clone())
            })
            .collect()
    }

    pub fn snippets(&self) -> &[fg_core::Snippet] {
        self.snippets
            .as_ref()
            .map(|p| p.entries.as_slice())
            .unwrap_or(&[])
    }

    /// The snippets matching the panel's search box, in list order.
    fn visible_snippets(&self, cx: &App) -> Vec<usize> {
        let Some(panel) = self.snippets.as_ref() else {
            return Vec::new();
        };
        let needle = panel.search.read(cx).value().to_lowercase();
        panel
            .entries
            .iter()
            .enumerate()
            .filter(|(_, snippet)| {
                needle.is_empty()
                    || snippet.title.to_lowercase().contains(&needle)
                    || snippet.sql.to_lowercase().contains(&needle)
                    || snippet
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(&needle))
            })
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Enter in the panel: put the snippet into the active query tab.
    pub fn pick_snippet(&mut self, cx: &mut Context<Self>) {
        let visible = self.visible_snippets(cx);
        let Some(panel) = self.snippets.as_ref() else {
            return;
        };
        let Some(ix) = visible.get(panel.selected).copied() else {
            return;
        };
        let Some(snippet) = panel.entries.get(ix) else {
            return;
        };
        let sql = snippet.sql.clone();
        cx.emit(WorkspaceEvent::HistoryPicked { sql });
        self.close_snippets(cx);
    }

    fn snippets_next(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.visible_snippets(cx).len();
        let Some(panel) = self.snippets.as_mut() else {
            return;
        };
        if count == 0 {
            return;
        }
        panel.selected = if delta < 0 {
            panel.selected.saturating_sub(1)
        } else {
            (panel.selected + 1).min(count - 1)
        };
        cx.notify();
    }

    /// Save what the active query tab has (the selection, else the whole buffer) as a snippet.
    pub fn save_current_as_snippet(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.query_view_of_active().cloned() else {
            return;
        };
        let (sql, title) = tab.read_with(cx, |t, cx| {
            let sql = t
                .selection(cx)
                .map(|range| t.script(cx)[range].to_owned())
                .unwrap_or_else(|| t.script(cx));
            let title = first_line_title(&sql, t.title());
            (sql, title)
        });
        if sql.trim().is_empty() {
            return;
        }
        cx.emit(WorkspaceEvent::SnippetSave { title, sql });
    }

    /// Delete the highlighted snippet.
    pub fn delete_selected_snippet(&mut self, cx: &mut Context<Self>) {
        let visible = self.visible_snippets(cx);
        let Some(panel) = self.snippets.as_ref() else {
            return;
        };
        let Some(ix) = visible.get(panel.selected).copied() else {
            return;
        };
        let Some(snippet) = panel.entries.get(ix) else {
            return;
        };
        let id = snippet.id.clone();
        cx.emit(WorkspaceEvent::SnippetDelete { id });
    }

    // ---- safe mode ----------------------------------------------------------------------------

    pub fn safe_mode(&self) -> SafeMode {
        self.safe_mode
    }

    /// Hand a schema snapshot to every open query tab. The workspace owns the tabs, so the
    /// controller does not have to enumerate them.
    pub fn set_catalog_everywhere(
        &mut self,
        catalog: crate::views::query_tab::CompletionCatalog,
        cx: &mut Context<Self>,
    ) {
        for tab in self.query_tabs() {
            tab.update(cx, |t, cx| t.set_catalog(catalog.clone(), cx));
        }
    }

    /// Hand one table's columns to every open query tab.
    pub fn set_columns_everywhere(
        &mut self,
        path: ObjectPath,
        columns: Vec<fg_core::ColumnMeta>,
        cx: &mut Context<Self>,
    ) {
        for tab in self.query_tabs() {
            tab.update(cx, |t, cx| {
                t.set_table_columns(path.clone(), columns.clone(), cx)
            });
        }
    }

    fn query_tabs(&self) -> Vec<Entity<QueryTab>> {
        self.tabs
            .tabs()
            .iter()
            .filter_map(|t| self.query_view(&t.id).cloned())
            .collect()
    }

    /// Whether the active query tab has a statement in flight (the stop button's enabled state).
    pub fn active_query_running(&self, cx: &App) -> bool {
        self.query_view_of_active()
            .is_some_and(|tab| tab.read(cx).results().is_running())
    }

    /// The controller persisted the change; reflect it (and the pill colour) immediately.
    pub fn set_safe_mode(&mut self, mode: SafeMode, cx: &mut Context<Self>) {
        self.safe_mode = mode;
        for content in self.tab_views.values() {
            if let TabContent::Table(tab) = content {
                tab.update(cx, |t, cx| t.set_safe_mode(mode, cx));
            }
        }
        cx.notify();
    }

    pub fn open_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let all = quick_items(&self.objects);
        let palette = cx.new(|cx| CommandState::new(window, cx));
        palette.update(cx, |p, cx| p.focus(window, cx));
        self.quick = Some(QuickOpenState {
            palette,
            all,
            query: String::new(),
        });
        cx.notify();
    }

    pub fn close_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    // ---- command palette ---------------------------------------------------------------

    pub fn command_palette_visible(&self) -> bool {
        self.commands.is_some()
    }

    /// Open the `⌘⇧P` palette, or close it when it is already open. The
    /// widget dispatches the confirmed item's action itself; this view
    /// only closes the palette afterwards.
    pub fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.commands.is_some() {
            self.close_command_palette(window, cx);
            return;
        }
        let palette = cx.new(|cx| CommandState::new(window, cx));
        palette.update(cx, |p, cx| p.focus(window, cx));
        self.commands = Some(CommandPaletteState { palette });
        cx.notify();
    }

    pub fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.commands.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// Close the topmost overlay, if any, and report whether one closed.
    /// Order is outermost-in for what the user sees: the palette, then
    /// the jump/switch panels, then the wizards, then the side panels,
    /// then the active tab's filter drawer, then the close-confirmation
    /// banner. Agent approvals are deliberately absent: a parked agent
    /// write must be answered, never dismissed unseen. Tabs are never
    /// closed here, and neither is the app.
    pub fn dismiss_topmost(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.commands.is_some() {
            self.close_command_palette(window, cx);
            return true;
        }
        if self.quick.is_some() {
            self.close_quick_open(window, cx);
            return true;
        }
        if self.switcher.is_some() {
            self.close_database_switcher(window, cx);
            return true;
        }
        if self.export_wizard.is_some() {
            self.close_export_wizard(cx);
            return true;
        }
        if self.import_wizard.is_some() {
            self.close_import_wizard(cx);
            return true;
        }
        if self.history.is_some() {
            self.close_history(cx);
            return true;
        }
        if self.snippets.is_some() {
            self.close_snippets(cx);
            return true;
        }
        if self.audit.is_some() {
            self.close_audit(cx);
            return true;
        }
        let drawer_open = self
            .tabs
            .active()
            .and_then(|t| self.tab_views.get(&t.id))
            .cloned()
            .is_some_and(|tab| match tab {
                TabContent::Table(tab) => tab.read(cx).filters_open(),
                TabContent::Query(_) => false,
            });
        if drawer_open {
            self.toggle_filters_of_active_tab(cx);
            return true;
        }
        if self.confirm_close.is_some() {
            self.confirm_close = None;
            cx.notify();
            return true;
        }
        false
    }

    /// Open the item at `index` of the palette's list.
    fn confirm_quick_open(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .quick
            .as_ref()
            .and_then(|q| q.shown().get(index).map(|i| i.path.clone()));
        self.close_quick_open(window, cx);
        if let Some(path) = target {
            self.open_table(&path, window, cx);
        }
    }

    // ---- database switching ---------------------------------------------------------------

    pub fn database_switcher_visible(&self) -> bool {
        self.switcher.is_some()
    }

    /// The database this window is currently connected to, if it is a network connection.
    fn current_database(&self) -> Option<String> {
        match &self.config.params {
            ConnectionParams::Network { database, .. } => database.clone(),
            ConnectionParams::File { .. } => None,
        }
    }

    /// Open the `⌘K` switcher. File connections have no databases to switch
    /// between, so they get the note instead of a list.
    pub fn open_database_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_database_switcher(window, cx);
            return;
        }
        let palette = cx.new(|cx| CommandState::new(window, cx));
        palette.update(cx, |p, cx| p.focus(window, cx));
        let network = matches!(self.config.params, ConnectionParams::Network { .. });
        self.switcher = Some(SwitchDatabaseState {
            palette,
            all: Vec::new(),
            query: String::new(),
            loading: network,
            switching: false,
            error: None,
        });
        if network {
            cx.emit(WorkspaceEvent::SwitchDatabaseRequested);
        }
        cx.notify();
    }

    pub fn close_database_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// Fill the switcher's list (called by the controller).
    pub fn set_database_list(&mut self, databases: Vec<String>, cx: &mut Context<Self>) {
        if let Some(s) = self.switcher.as_mut() {
            s.all = databases;
            s.loading = false;
            s.error = None;
        }
        cx.notify();
    }

    /// Report a load or switch failure back into the switcher. The switcher
    /// stays open across a switch attempt, so the error lands where the user
    /// is looking; success closes it through [`Self::apply_switched_connection`].
    pub fn set_database_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(s) = self.switcher.as_mut() {
            s.loading = false;
            s.switching = false;
            s.error = Some(message.into());
        }
        cx.notify();
    }

    fn confirm_database_switch(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .switcher
            .as_ref()
            .and_then(|s| s.shown().get(index).cloned());
        let Some(database) = target else {
            return;
        };
        // Picking the database we are already on changes nothing.
        if self.current_database().as_deref() == Some(database.as_str()) {
            self.close_database_switcher(window, cx);
            return;
        }
        if let Some(s) = self.switcher.as_mut() {
            if s.switching {
                return;
            }
            s.switching = true;
            s.error = None;
        }
        cx.emit(WorkspaceEvent::SwitchDatabase { database });
        cx.notify();
    }

    /// Apply a successful switch: the window now shows the new database's
    /// schema, and every open tab is closed — tabs belong to the old database.
    /// Callers check for staged edits first (see [`Self::has_pending_changes`]).
    pub fn apply_switched_connection(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        self.config = config;
        self.tabs = TabSet::new();
        self.tab_views.clear();
        self.confirm_close = None;
        self.switcher = None;
        self.schema = SchemaState::Loading;
        self.namespaces = Vec::new();
        self.objects = std::collections::BTreeMap::new();
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// Any table tab with staged edits. Switching closes tabs, so the
    /// controller refuses while this is true rather than discarding edits.
    pub fn has_pending_changes(&self, cx: &App) -> bool {
        self.tab_views.keys().any(|id| {
            self.tab_view(id)
                .is_some_and(|tab| tab.read(cx).pending(cx).total() > 0)
        })
    }

    pub fn toggle_filters_of_active_tab(&mut self, cx: &mut Context<Self>) {
        let Some(TabContent::Table(tab)) = self
            .tabs
            .active()
            .and_then(|t| self.tab_views.get(&t.id))
            .cloned()
        else {
            return;
        };
        tab.update(cx, |t, cx| t.toggle_filters(cx));
    }

    pub fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.tabs.active_id().cloned() {
            self.close_tab(&id, cx);
        }
    }

    pub fn tabs(&self) -> &TabSet {
        &self.tabs
    }

    pub fn tab_view(&self, id: &TabId) -> Option<&Entity<TableTab>> {
        match self.tab_views.get(id)? {
            TabContent::Table(tab) => Some(tab),
            TabContent::Query(_) => None,
        }
    }

    /// The query editor of a tab, if it is one.
    pub fn query_view(&self, id: &TabId) -> Option<&Entity<QueryTab>> {
        match self.tab_views.get(id)? {
            TabContent::Query(tab) => Some(tab),
            TabContent::Table(_) => None,
        }
    }

    /// The active tab's editor, whatever kind it is (used by the controller after a run).
    pub fn active_tab_view(&self) -> Option<TabContent> {
        self.tabs
            .active()
            .and_then(|t| self.tab_views.get(&t.id))
            .cloned()
    }

    /// The query editor of the active tab, if the active tab is one.
    fn query_view_of_active(&self) -> Option<&Entity<QueryTab>> {
        self.tabs
            .active()
            .and_then(|t| self.tab_views.get(&t.id))
            .and_then(|content| match content {
                TabContent::Query(tab) => Some(tab),
                TabContent::Table(_) => None,
            })
    }

    /// The tables and views currently listed in the sidebar (after filtering), in display order.
    pub fn visible_tables(&self) -> Vec<ObjectPath> {
        let mut shown: Vec<_> = self.meta.values().filter_map(|m| m.path.clone()).collect();
        shown.sort();
        shown
    }

    pub fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).value().to_string()
    }

    // ---- schema -------------------------------------------------------------------------------

    pub fn set_schema(
        &mut self,
        namespaces: Vec<String>,
        objects: BTreeMap<String, Vec<ObjectInfo>>,
        cx: &mut Context<Self>,
    ) {
        self.namespaces = namespaces;
        self.objects = objects;
        self.schema = SchemaState::Ready;
        self.rebuild_tree(cx);
    }

    pub fn set_schema_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.schema = SchemaState::Failed(message.into());
        cx.notify();
    }

    pub fn set_schema_loading(&mut self, cx: &mut Context<Self>) {
        self.schema = SchemaState::Loading;
        cx.notify();
    }

    pub fn object_exists(&self, path: &ObjectPath) -> bool {
        self.objects
            .values()
            .flatten()
            .any(|o| &o.path == path && o.kind.is_relation())
    }

    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().to_string();
        let show_namespaces = self.namespaces.len() > 1;
        let nodes = build_tree(&self.namespaces, &self.objects, &filter, show_namespaces);
        let mut meta = HashMap::new();
        collect_meta(&nodes, &mut meta);
        self.meta = Rc::new(meta);
        let filtering = !filter.is_empty();
        let items = to_items(&nodes, &self.collapsed, filtering);
        self.tree.update(cx, |t, cx| t.set_items(items, cx));
        cx.notify();
    }

    // ---- tabs ---------------------------------------------------------------------------------

    /// Show a table, in an existing tab if it already has one.
    pub fn open_table(&mut self, path: &ObjectPath, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.tabs.open_table(path, TableViewMode::Data);
        if !self.tab_views.contains_key(&id) {
            let query_path = path.clone();
            let mode = self.safe_mode;
            let tab = cx.new(|cx| TableTab::new(query_path, window, cx));
            tab.update(cx, |t, cx| t.set_safe_mode(mode, cx));
            // Persist view state when the user changes the query, and let the controller load.
            let on_tab = cx.subscribe_in(
                &tab,
                window,
                |this, _, event: &TableTabEvent, window, cx| match event {
                    TableTabEvent::Reload => cx.emit(WorkspaceEvent::SessionChanged),
                    // Saved by the controller together with the table's other view state.
                    TableTabEvent::ViewChanged => {}
                    // Run by the controller, which owns the connection.
                    TableTabEvent::CommitRequested => {}
                    // Loaded by the controller, which owns the connection.
                    TableTabEvent::StructureRequested => {}
                    // Applied by the controller, which owns the connection.
                    TableTabEvent::StructureCommitRequested => {}
                    TableTabEvent::FollowForeignKey { table, filter } => {
                        this.open_table_filtered(table, vec![filter.clone()], window, cx);
                    }
                    TableTabEvent::FollowBackReference { table, filters } => {
                        this.open_table_filtered(table, filters.clone(), window, cx);
                    }
                },
            );
            let on_tab_change = cx.observe(&tab, |_, _, cx| cx.notify());
            self._subscriptions.push(on_tab);
            self._subscriptions.push(on_tab_change);
            self.tab_views.insert(id, TabContent::Table(tab.clone()));
            cx.emit(WorkspaceEvent::TableOpened {
                path: path.clone(),
                tab,
            });
        }
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    /// Open a query tab. `document` carries the identity and title; `script` is the draft's text
    /// (empty for a new one). Re-opening the same document activates its tab instead of adding one.
    pub fn open_query(
        &mut self,
        document: QueryDocumentState,
        script: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<QueryTab> {
        let id = self.tabs.open_query(document.id().clone());
        if let Some(TabContent::Query(tab)) = self.tab_views.get(&id).cloned() {
            self.activate_tab(&id, cx);
            return tab;
        }
        let kind = self.config.kind;
        let tab = cx.new(|cx| QueryTab::new(document, kind, script, window, cx));
        let on_tab = cx.subscribe(&tab, |_, tab, event: &QueryTabEvent, cx| match event {
            QueryTabEvent::Run { mode } => cx.emit(WorkspaceEvent::QueryRun {
                tab: tab.clone(),
                mode: *mode,
            }),
            QueryTabEvent::Cancel => cx.emit(WorkspaceEvent::QueryCancel { tab: tab.clone() }),
            QueryTabEvent::DraftChanged => {
                cx.emit(WorkspaceEvent::QueryDraftChanged { tab: tab.clone() })
            }
            QueryTabEvent::ConfirmedRun { script } => cx.emit(WorkspaceEvent::QueryConfirmed {
                tab: tab.clone(),
                script: script.clone(),
            }),
            QueryTabEvent::Explain => cx.emit(WorkspaceEvent::QueryExplain { tab: tab.clone() }),
            QueryTabEvent::CommitResultEdits => {
                cx.emit(WorkspaceEvent::QueryResultCommit { tab: tab.clone() })
            }
            QueryTabEvent::DismissedConfirmation => {}
        });
        let on_tab_change = cx.observe(&tab, |_, _, cx| cx.notify());
        self._subscriptions.push(on_tab);
        self._subscriptions.push(on_tab_change);
        self.tab_views.insert(id, TabContent::Query(tab.clone()));
        cx.emit(WorkspaceEvent::QueryOpened { tab: tab.clone() });
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
        tab
    }

    /// Open `path` showing only the rows matching `filter` (following a foreign key).
    pub fn open_table_filtered(
        &mut self,
        path: &ObjectPath,
        filters: Vec<fg_core::Filter>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_table(path, window, cx);
        let Some(id) = self.tabs.active_id().cloned() else {
            return;
        };
        if let Some(TabContent::Table(tab)) = self.tab_views.get(&id).cloned() {
            tab.update(cx, |t, cx| t.apply_filters(filters, cx));
        }
    }

    pub fn activate_tab(&mut self, id: &TabId, cx: &mut Context<Self>) {
        if self.tabs.activate(id) {
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.notify();
        }
    }

    /// Close a tab. A table tab with staged edits does not close on the
    /// first request: it opens the confirmation banner instead, and only
    /// the second request (the banner's button) actually closes it.
    pub fn close_tab(&mut self, id: &TabId, cx: &mut Context<Self>) {
        let dirty = self
            .tab_view(id)
            .is_some_and(|tab| tab.read(cx).pending(cx).total() > 0);
        if dirty && self.confirm_close.as_ref() != Some(id) {
            self.confirm_close = Some(id.clone());
            cx.notify();
            return;
        }
        self.confirm_close = None;
        self.tabs.close(id);
        self.tab_views.remove(id);
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    pub fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.confirm_close = None;
        cx.notify();
    }

    pub fn next_tab(&mut self, cx: &mut Context<Self>) {
        self.tabs.next();
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    pub fn prev_tab(&mut self, cx: &mut Context<Self>) {
        self.tabs.prev();
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    pub fn toggle_details(&mut self, cx: &mut Context<Self>) {
        self.details_visible = !self.details_visible;
        cx.notify();
    }

    pub fn details_visible(&self) -> bool {
        self.details_visible
    }

    pub fn sidebar_width(&self) -> f32 {
        self.sidebar_width
    }

    /// Set the sidebar width, kept within the range the drag handle allows.
    pub fn set_sidebar_width(&mut self, width: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_width = width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        let target = px(self.sidebar_width);
        // Only possible once the panels have been laid out at least once; until then the
        // panel's initial size (the same value) applies.
        self.split.update(cx, |s, cx| {
            if s.sizes().len() >= 2 {
                s.resize_panel(0, target, window, cx);
            }
        });
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
    }

    pub fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    /// What to save so the next launch can pick up where this one left off.
    pub fn session(&self) -> WorkspaceState {
        self.tabs.to_workspace(WorkspaceState {
            sidebar_visible: self.sidebar_visible,
            sidebar_width: Some(self.sidebar_width),
            ..WorkspaceState::default()
        })
    }

    /// Reopen tabs from a saved session. Tables that no longer exist are skipped. Call after the
    /// schema is loaded.
    pub fn restore(&mut self, saved: &WorkspaceState, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = saved.sidebar_visible;
        if let Some(width) = saved.sidebar_width {
            self.set_sidebar_width(width, window, cx);
        }
        let wanted_active = saved.active_tab.clone();
        let mut active_path = None;
        for tab in &saved.tabs {
            if let TabKind::Table { path, .. } = &tab.kind
                && self.object_exists(path)
            {
                if wanted_active.as_ref() == Some(&tab.id) {
                    active_path = Some(path.clone());
                }
                self.open_table(path, window, cx);
            }
        }
        // Reopening activates each table in turn: end on the one that was active.
        if let Some(path) = active_path {
            self.open_table(&path, window, cx);
        }
    }
}

fn collect_meta(nodes: &[TreeNode], out: &mut HashMap<SharedString, NodeMeta>) {
    for n in nodes {
        out.insert(
            n.id.clone().into(),
            NodeMeta {
                kind: n.kind,
                path: n.path.clone(),
                row_estimate: n.row_estimate,
                matched: n.matched.clone(),
            },
        );
        collect_meta(&n.children, out);
    }
}

/// `text` with the characters at `positions` (character indices) in bold.
fn highlighted(text: &str, positions: &[usize]) -> AnyElement {
    if positions.is_empty() {
        return SharedString::from(text.to_owned()).into_any_element();
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let highlights = positions
        .iter()
        .filter_map(|&p| chars.get(p))
        .map(|&(start, c)| {
            (
                start..start + c.len_utf8(),
                HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
            )
        });
    StyledText::new(text.to_owned())
        .with_highlights(highlights)
        .into_any_element()
}

fn to_items(
    nodes: &[TreeNode],
    collapsed: &HashSet<SharedString>,
    filtering: bool,
) -> Vec<TreeItem> {
    nodes
        .iter()
        .map(|n| {
            let label = n.label.clone();
            let id: SharedString = n.id.clone().into();
            let expanded = filtering || !collapsed.contains(&id);
            TreeItem::new(id, label)
                .expanded(expanded)
                .children(to_items(&n.children, collapsed, filtering))
        })
        .collect()
}

/// Toolbar height. The stock [`TitleBar`] height (34 px) left the toolbar icons cramped;
/// 44 px gives the icon buttons a comfortable hit area.
const TOOLBAR_HEIGHT: Pixels = px(44.);

/// Hit box for the toolbar's icon-only buttons: gpui-kit renders the icon at 75% of the
/// box, so a 24 px box yields the 18 px icons the toolbar settled on (stock `Small` is
/// 14 px, `Medium` 16 px, `Large` 24 px).
const TOOLBAR_ICON_BOX: Pixels = px(24.);

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_zh = crate::views::style::is_zh_locale();
        let view = cx.entity().downgrade();
        let active_table_name = self.tabs.active().map(|t| match &t.kind {
            TabKind::Table { path, .. } => table_title(path),
            TabKind::Query { .. } => self
                .query_view_of_active()
                .map(|tab| tab.read(cx).title())
                .unwrap_or_else(|| "Query".to_owned()),
        });
        let db_name = match &self.config.params {
            fg_core::ConnectionParams::File { path } => std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_owned(),
            fg_core::ConnectionParams::Network { database, host, .. } => {
                database.clone().unwrap_or_else(|| host.clone())
            }
        };
        // TablePlus's connection line: `LOCAL | MySQL 5.7.44 : alpha_new : shop : orders`.
        let mut connection_line = format!(
            "{} : {} : {}",
            self.config.kind.display_name(),
            self.config.name,
            db_name
        );
        if let Some(table) = &active_table_name {
            connection_line.push_str(" : ");
            connection_line.push_str(table);
        }
        let environment_label = match self.config.environment {
            fg_core::Environment::Development => "DEVELOPMENT",
            fg_core::Environment::Staging => "STAGING",
            _ => "PRODUCTION",
        };

        let env_color =
            super::style::environment_color(cx, self.config.environment, self.config.color);
        // A group of related toolbar buttons: spacing only, no box of its own.
        let capsule = move || h_flex().items_center().gap_0p5();
        let divider = {
            let color = theme.border;
            move || div().w(px(2.)).h(px(14.)).bg(color.opacity(0.0))
        };

        let title = TitleBar::new().h(TOOLBAR_HEIGHT).child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .justify_between()
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_3()
                        .items_center()
                        .child(
                            capsule().child(
                                Button::new("toggle-sidebar")
                                    .icon(hugeicon(Hugeicon::SidebarLeft))
                                    .ghost()
                                    .with_size(TOOLBAR_ICON_BOX)
                                    .tooltip(if is_zh {
                                        "切换侧栏"
                                    } else {
                                        "Toggle sidebar"
                                    })
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                                    ),
                            ),
                        )
                        .child(
                            capsule()
                                .child(
                                    Button::new("stop-btn")
                                        .icon(hugeicon(Hugeicon::CircleX))
                                        .ghost()
                                        .with_size(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh {
                                            "取消正在运行的语句"
                                        } else {
                                            "Cancel the running statement"
                                        })
                                        .when(!self.active_query_running(cx), |b| b.disabled(true))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(tab) = this.query_view_of_active().cloned()
                                            {
                                                cx.emit(WorkspaceEvent::QueryCancel { tab });
                                            }
                                        })),
                                )
                                .child(divider())
                                .child(
                                    Button::new("history-btn")
                                        .icon(hugeicon(Hugeicon::History))
                                        .ghost()
                                        .with_size(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh {
                                            "查询历史"
                                        } else {
                                            "Query history"
                                        })
                                        .when(self.history.is_some(), |b| b.primary())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_history(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("audit-btn")
                                        .icon(hugeicon(Hugeicon::Lock))
                                        .ghost()
                                        .with_size(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh { "审计日志" } else { "Audit log" })
                                        .when(self.audit.is_some(), |b| b.primary())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_audit(window, cx)
                                        })),
                                ),
                        )
                        .child(
                            capsule()
                                .child(
                                    Button::new("lock-btn")
                                        .icon(hugeicon(if self.safe_mode == SafeMode::ReadOnly {
                                            Hugeicon::Lock
                                        } else {
                                            Hugeicon::Unlock
                                        }))
                                        .ghost()
                                        .with_size(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh {
                                            format!("安全模式：{}", safe_mode_label(self.safe_mode))
                                        } else {
                                            format!(
                                                "Safe mode: {}",
                                                safe_mode_label(self.safe_mode)
                                            )
                                        })
                                        .when(self.safe_mode != SafeMode::Normal, |b| b.primary())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.emit(WorkspaceEvent::SafeModeChanged {
                                                mode: next_safe_mode(this.safe_mode),
                                            })
                                        })),
                                )
                                .child(divider())
                                .child(
                                    Button::new("sql-console")
                                        .child(
                                            div()
                                                .px_1()
                                                .font_family(theme.mono_font_family.clone())
                                                .text_size(px(11.))
                                                .font_weight(FontWeight::BOLD)
                                                .child("SQL"),
                                        )
                                        .ghost()
                                        .h(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh { "新建查询" } else { "New query" })
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(WorkspaceEvent::NewQueryRequested)
                                        })),
                                ),
                        ),
                )
                // The connection line fills the space between the button groups on a rounded bar,
                // its text left-aligned in monospace as TablePlus prints it.
                .child(
                    h_flex()
                        .id("connection-pill")
                        .test_support()
                        .aria_label(self.config.name.clone())
                        .flex_1()
                        .min_w_0()
                        .mx_2()
                        .h(px(28.))
                        .px_3()
                        .gap_2()
                        .items_center()
                        .rounded_lg()
                        .bg(theme.muted)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(theme.foreground)
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(env_color)
                                .child(environment_label),
                        )
                        .child(div().text_color(theme.muted_foreground).child("|"))
                        .child(div().min_w_0().text_ellipsis().child(connection_line)),
                )
                .child(
                    // Kept off the window's right edge, like the traffic lights on the left.
                    h_flex()
                        .flex_shrink_0()
                        .pr_3()
                        .gap_3()
                        .items_center()
                        .child(
                            capsule()
                                .child(
                                    Button::new("refresh-schema")
                                        .icon(hugeicon(Hugeicon::Refresh))
                                        .ghost()
                                        .with_size(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh {
                                            "刷新结构"
                                        } else {
                                            "Refresh schema"
                                        })
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(WorkspaceEvent::RefreshSchema)
                                        })),
                                )
                                .child(divider())
                                .child(
                                    Button::new("cycle-theme")
                                        .icon(match crate::theme_control::chosen(cx) {
                                            fg_core::ThemeMode::Light => hugeicon(Hugeicon::Sun),
                                            fg_core::ThemeMode::Dark => hugeicon(Hugeicon::Moon),
                                            _ => hugeicon(Hugeicon::Command),
                                        })
                                        .ghost()
                                        .with_size(TOOLBAR_ICON_BOX)
                                        .tooltip(if is_zh {
                                            format!(
                                                "主题：{}",
                                                crate::theme_control::label(
                                                    crate::theme_control::chosen(cx)
                                                )
                                            )
                                        } else {
                                            format!(
                                                "Theme: {}",
                                                crate::theme_control::label(
                                                    crate::theme_control::chosen(cx)
                                                )
                                            )
                                        })
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(WorkspaceEvent::CycleTheme)
                                        })),
                                ),
                        )
                        .child(
                            capsule().child(
                                Button::new("toggle-details")
                                    .icon(hugeicon(Hugeicon::SidebarRight))
                                    .ghost()
                                    .with_size(TOOLBAR_ICON_BOX)
                                    .tooltip(if is_zh {
                                        "切换详情面板"
                                    } else {
                                        "Toggle details panel"
                                    })
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_details(cx)),
                                    ),
                            ),
                        ),
                ),
        );

        let sidebar = self.render_sidebar(&view, cx);
        let main = self.render_main(window, cx);
        let details = self.render_details_panel(cx);

        let snippets = self.render_snippets(cx);
        let history = self.render_history(cx);
        let audit = self.render_audit(cx);
        let overlay = self.render_quick_open(window, cx);
        v_flex()
            .id("workspace")
            .size_full()
            .relative()
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &QuickOpen, window, cx| this.open_quick_open(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| {
                this.open_command_palette(window, cx)
            }))
            .on_action(cx.listener(|this, _: &DismissTopmost, window, cx| {
                this.dismiss_topmost(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchDatabase, window, cx| {
                this.open_database_switcher(window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.close_active_tab(cx)))
            .on_action(cx.listener(|this, _: &ExportWizard, window, cx| {
                this.open_export_wizard(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ImportWizard, window, cx| {
                this.open_import_wizard(window, cx);
            }))
            .on_action(
                cx.listener(|_, _: &NewQuery, _, cx| cx.emit(WorkspaceEvent::NewQueryRequested)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleHistory, window, cx| this.open_history(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleSnippets, window, cx| this.open_snippets(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SaveSnippet, _, cx| this.save_current_as_snippet(cx)))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.next_tab(cx)))
            .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.prev_tab(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleAudit, window, cx| this.open_audit(window, cx)))
            .on_action(cx.listener(|this, _: &CycleSafeMode, _, cx| {
                cx.emit(WorkspaceEvent::SafeModeChanged {
                    mode: next_safe_mode(this.safe_mode),
                })
            }))
            .on_action(cx.listener(|_, _: &CycleTheme, _, cx| cx.emit(WorkspaceEvent::CycleTheme)))
            .on_action(cx.listener(|this, _: &ToggleDetails, _, cx| this.toggle_details(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleFilters, _, cx| this.toggle_filters_of_active_tab(cx)),
            )
            .on_action(
                cx.listener(|_, _: &RefreshSchema, _, cx| cx.emit(WorkspaceEvent::RefreshSchema)),
            )
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                            h_resizable("workspace-split")
                                .with_state(&self.split)
                                .on_resize({
                                    let view = cx.entity().downgrade();
                                    move |state, _, cx| {
                                        let width =
                                            state.read(cx).sizes().first().map(|w| f32::from(*w));
                                        let _ = view.update(cx, |this, cx| {
                                            if let Some(w) = width {
                                                this.sidebar_width = w;
                                                cx.emit(WorkspaceEvent::SessionChanged);
                                            }
                                        });
                                    }
                                })
                                .child(
                                    resizable_panel()
                                        .size(px(self.sidebar_width))
                                        .size_range(px(SIDEBAR_MIN)..px(SIDEBAR_MAX))
                                        .fixed()
                                        .visible(self.sidebar_visible)
                                        .child(sidebar),
                                )
                                .child(resizable_panel().child(main)),
                        ),
                    )
                    .when(self.details_visible, |d| d.child(details)),
            )
            .children(history)
            .children(audit)
            .children(snippets)
            .children(overlay)
            .children(self.render_database_switcher(window, cx))
            .children(self.render_command_palette(window, cx))
    }
}

impl WorkspaceView {
    /// The `⌘⇧P` palette, drawn over the window like Quick Open. The
    /// widget dispatches the confirmed item's action (and shows its
    /// keybinding) itself; closing the palette is all we do afterwards.
    /// Only workspace-level commands are listed, for the reason on the
    /// `commands` field: grid and editor actions keep their keys.
    fn render_command_palette(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.commands.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let (confirm_view, cancel_view) = (view.clone(), view);

        let item = |label: &str, keywords: &[&str], action: Box<dyn gpui_kit::Action>| {
            let mut item = CommandItem::new().label(label.to_owned()).action(action);
            if !keywords.is_empty() {
                item = item.keywords(keywords.iter().map(|k| k.to_string()).collect::<Vec<_>>());
            }
            item
        };
        let items = [
            item(
                "Quick Open…",
                &["open", "table", "view", "jump"],
                Box::new(QuickOpen),
            ),
            item(
                "Switch database…",
                &["database", "schema", "server"],
                Box::new(SwitchDatabase),
            ),
            item(
                "New query tab",
                &["query", "sql", "new"],
                Box::new(NewQuery),
            ),
            item("Close tab", &["close"], Box::new(CloseTab)),
            item("Next tab", &["next"], Box::new(NextTab)),
            item("Previous tab", &["previous"], Box::new(PrevTab)),
            item(
                "Toggle sidebar",
                &["sidebar", "tree"],
                Box::new(ToggleSidebar),
            ),
            item(
                "Toggle details panel",
                &["details"],
                Box::new(ToggleDetails),
            ),
            item("Audit log", &["audit", "log"], Box::new(ToggleAudit)),
            item(
                "Cycle safe mode",
                &["safe", "mode"],
                Box::new(CycleSafeMode),
            ),
            item(
                "Cycle theme",
                &["theme", "dark", "light"],
                Box::new(CycleTheme),
            ),
            item(
                "Query history",
                &["history", "recent", "statements"],
                Box::new(ToggleHistory),
            ),
            item(
                "Snippets",
                &["snippets", "fragments", "saved"],
                Box::new(ToggleSnippets),
            ),
            item(
                "Save current statement as snippet",
                &["save", "snippet"],
                Box::new(SaveSnippet),
            ),
            item(
                "Refresh schema",
                &["refresh", "reload", "schema"],
                Box::new(RefreshSchema),
            ),
            item(
                "Toggle filters",
                &["filter", "drawer"],
                Box::new(ToggleFilters),
            ),
            item(
                "Export…",
                &["export", "csv", "json", "sql"],
                Box::new(ExportWizard),
            ),
            item("Import CSV…", &["import", "csv"], Box::new(ImportWizard)),
        ];

        let palette = Command::new(&state.palette)
            .items(items)
            .placeholder("Run a command…")
            .max_h(px(320.))
            .on_confirm(move |_, window, cx| {
                let _ = confirm_view.update(cx, |this, cx| {
                    this.close_command_palette(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                let _ = cancel_view.update(cx, |this, cx| {
                    this.close_command_palette(window, cx);
                });
            });

        Some(
            div()
                .id("command-palette")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .child(
                    div()
                        .w(px(560.))
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_lg()
                        .child(palette),
                )
                .into_any_element(),
        )
    }

    /// The `⌘P` palette, drawn over the window. The palette widget does the searching, the up and
    /// down keys, Enter and Esc; we turn its answer back into "open this table".
    fn render_quick_open(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let quick = self.quick.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let (confirm_view, cancel_view, query_view) = (view.clone(), view.clone(), view);

        let items = quick.shown().into_iter().map(|i| {
            let icon = if i.kind == ObjectKind::View {
                hugeicon(Hugeicon::View)
            } else {
                hugeicon(Hugeicon::Table)
            };
            let mut item = CommandItem::new().label(i.path.name.clone()).icon(icon);
            if let Some(ns) = &i.path.namespace {
                item = item.keywords([ns.clone()]);
            }
            item
        });

        let palette = Command::new(&quick.palette)
            .items(items)
            .filterable(false)
            .on_query(move |query, _, cx| {
                let _ = query_view.update(cx, |this, cx| {
                    if let Some(q) = this.quick.as_mut() {
                        q.query = query.to_owned();
                        cx.notify();
                    }
                });
            })
            .placeholder("Open table or view…")
            .max_h(px(320.))
            .on_confirm(move |ix, window, cx| {
                let _ = confirm_view.update(cx, |this, cx| {
                    this.confirm_quick_open(ix.row, window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                let _ = cancel_view.update(cx, |this, cx| this.close_quick_open(window, cx));
            });

        Some(
            div()
                .id("quick-open")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .child(
                    div()
                        .w(px(560.))
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_lg()
                        .child(palette),
                )
                .into_any_element(),
        )
    }

    /// The `⌘K` palette, drawn over the window like Quick Open. File
    /// connections get a note: there is no server to switch databases on.
    fn render_database_switcher(
        &self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let switcher = self.switcher.as_ref()?;
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let is_file = matches!(self.config.params, ConnectionParams::File { .. });
        let current = self.current_database();
        let shell = |inner: AnyElement| {
            div()
                .id("database-switcher")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .child(
                    div()
                        .w(px(560.))
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_lg()
                        .child(inner),
                )
                .into_any_element()
        };
        if is_file {
            let close_view = view.clone();
            return Some(shell(
                v_flex()
                    .gap_2()
                    .p_3()
                    .child(
                        div()
                            .id("database-switcher-note")
                            .test_support()
                            .aria_label(
                                "Database switching is only available for server connections.",
                            )
                            .text_sm()
                            .child("Database switching is only available for server connections."),
                    )
                    .child(
                        Button::new("database-switcher-close")
                            .small()
                            .label("Close")
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let _ = close_view.update(cx, |this, cx| {
                                    this.close_database_switcher(window, cx);
                                });
                            })),
                    )
                    .into_any_element(),
            ));
        }
        if switcher.loading {
            return Some(shell(
                div()
                    .id("database-switcher-loading")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading databases…")
                    .into_any_element(),
            ));
        }
        let (confirm_view, cancel_view, query_view) = (view.clone(), view.clone(), view);
        let items = switcher.shown().into_iter().map(|name| {
            let label = if current.as_deref() == Some(name.as_str()) {
                SharedString::from(format!("{name} (current)"))
            } else {
                SharedString::from(name)
            };
            CommandItem::new()
                .label(label)
                .icon(hugeicon(Hugeicon::Database))
        });
        let mut body = v_flex();
        if let Some(error) = &switcher.error {
            body = body.child(
                div()
                    .id("database-switcher-error")
                    .test_support()
                    .aria_label(error.clone())
                    .p_3()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if switcher.switching {
            body = body.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Switching…"),
            );
        }
        let palette = Command::new(&switcher.palette)
            .items(items)
            .filterable(false)
            .on_query(move |query, _, cx| {
                let _ = query_view.update(cx, |this, cx| {
                    if let Some(s) = this.switcher.as_mut() {
                        s.query = query.to_owned();
                        cx.notify();
                    }
                });
            })
            .placeholder("Switch database…")
            .max_h(px(320.))
            .on_confirm(move |ix, window, cx| {
                let _ = confirm_view.update(cx, |this, cx| {
                    this.confirm_database_switch(ix.row, window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                let _ = cancel_view.update(cx, |this, cx| this.close_database_switcher(window, cx));
            });
        body = body.child(palette);
        Some(shell(body.into_any_element()))
    }

    /// The `⌘Y` panel: recent statements, newest first, with their outcome. Drawn as an overlay
    /// like Quick Open so it does not steal space from the editor.
    /// The agent write approvals: one row per parked run, with the
    /// exact SQL the person is being asked to allow. Approve runs it;
    /// Deny refuses it. There is no dismiss: leaving a row unanswered
    /// leaves the agent parked, which is the safe default.
    fn render_mcp_approvals_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.mcp_approvals.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let mut panel = v_flex()
            .id("mcp-approvals")
            .test_support()
            .w_full()
            .flex_shrink_0()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.danger.opacity(0.5))
            .bg(theme.danger.opacity(0.14))
            .child(div().text_size(px(13.)).text_color(theme.foreground).child(
                "An agent asked to write to this database. Approve only the exact statement shown.",
            ));
        for request in &self.mcp_approvals {
            let id = request.id;
            let summary = request
                .statements
                .iter()
                .map(|s| match s.danger {
                    Some(danger) => format!("{:?} · {danger:?}", s.class),
                    None => format!("{:?}", s.class),
                })
                .collect::<Vec<_>>()
                .join(", ");
            panel = panel.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · {} · {summary}",
                                request.client, request.connection_name
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.foreground)
                            .child(request.sql.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(format!("mcp-approve-{id}"))
                                    .label("Approve")
                                    .small()
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(WorkspaceEvent::McpWriteResolve {
                                            id,
                                            approved: true,
                                        });
                                    })),
                            )
                            .child(
                                Button::new(format!("mcp-deny-{id}"))
                                    .label("Deny")
                                    .small()
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(WorkspaceEvent::McpWriteResolve {
                                            id,
                                            approved: false,
                                        });
                                    })),
                            ),
                    ),
            );
        }
        Some(panel.into_any_element())
    }

    fn render_export_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let wizard = self.export_wizard.as_ref()?;
        let theme = cx.theme().clone();
        let running = matches!(wizard.status, ExportWizardStatus::Running);
        let is_sql_source = matches!(wizard.source, ExportSource::Sql(_));
        let status_text = match &wizard.status {
            ExportWizardStatus::Idle => "Choose a file and a format.".to_owned(),
            ExportWizardStatus::Running => {
                format!("Exporting… {} rows written.", wizard.rows)
            }
            ExportWizardStatus::Done { rows, bytes } => format!(
                "Exported {rows} rows ({bytes} bytes) to {}.",
                wizard.last_path.clone().unwrap_or_default()
            ),
            ExportWizardStatus::Failed(message) => message.clone(),
        };
        let format_button =
            |id: &'static str, label: &'static str, active: bool, format: ExportFormat| {
                Button::new(id)
                    .label(label)
                    .small()
                    .when(active, |b| b.primary())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(wizard) = this.export_wizard.as_mut() {
                            wizard.format = format;
                            cx.notify();
                        }
                    }))
            };
        let scope_button =
            |id: &'static str, label: &'static str, active: bool, scope: ExportScope| {
                Button::new(id)
                    .label(label)
                    .small()
                    .when(active, |b| b.primary())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(wizard) = this.export_wizard.as_mut() {
                            wizard.scope = scope;
                            cx.notify();
                        }
                    }))
            };
        let gzip_checked = wizard.gzip;
        let mut row = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_sm().child("Export"))
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&wizard.path).id("export-path")),
            )
            .child(format_button(
                "export-format-csv",
                "CSV",
                wizard.format == ExportFormat::Csv,
                ExportFormat::Csv,
            ))
            .child(format_button(
                "export-format-json",
                "JSON",
                wizard.format == ExportFormat::Json,
                ExportFormat::Json,
            ));
        if !is_sql_source {
            row = row.child(format_button(
                "export-format-sql",
                "SQL",
                wizard.format == ExportFormat::Sql,
                ExportFormat::Sql,
            ));
        }
        if wizard.format == ExportFormat::Sql {
            row = row
                .child(scope_button(
                    "export-scope-data",
                    "Data",
                    wizard.scope == ExportScope::Data,
                    ExportScope::Data,
                ))
                .child(scope_button(
                    "export-scope-structure",
                    "Structure",
                    wizard.scope == ExportScope::Structure,
                    ExportScope::Structure,
                ))
                .child(scope_button(
                    "export-scope-both",
                    "Both",
                    wizard.scope == ExportScope::Both,
                    ExportScope::Both,
                ));
        }
        row = row
            .child(
                Checkbox::new(SharedString::from("export-gzip"))
                    .label("gzip")
                    .checked(gzip_checked)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(wizard) = this.export_wizard.as_mut() {
                            wizard.gzip = !wizard.gzip;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("export-run")
                    .label("Run")
                    .small()
                    .disabled(running)
                    .on_click(cx.listener(|this, _, _, cx| this.run_export(cx))),
            );
        if running {
            row = row.child(
                Button::new("export-cancel")
                    .label("Cancel")
                    .small()
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(WorkspaceEvent::ExportCancel);
                    })),
            );
        }
        row = row.child(
            Button::new("export-close")
                .label("Close")
                .small()
                .on_click(cx.listener(|this, _, _, cx| this.close_export_wizard(cx))),
        );
        Some(
            v_flex()
                .id("export-wizard")
                .test_support()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(row)
                .child(
                    div()
                        .id("export-status")
                        .test_support()
                        .aria_label(status_text.clone())
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(status_text),
                )
                .into_any_element(),
        )
    }

    fn render_import_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let wizard = self.import_wizard.as_ref()?;
        let theme = cx.theme().clone();
        let running = matches!(wizard.status, ImportWizardStatus::Running);
        let status_text = match &wizard.status {
            ImportWizardStatus::Idle => {
                "Choose a CSV file, load its preview, map its columns.".to_owned()
            }
            ImportWizardStatus::Running => "Importing…".to_owned(),
            ImportWizardStatus::Done(report) => {
                if report.cancelled {
                    format!(
                        "Import cancelled after {} rows ({} skipped).",
                        report.inserted, report.skipped_total
                    )
                } else {
                    format!(
                        "Imported {} rows ({} skipped).",
                        report.inserted, report.skipped_total
                    )
                }
            }
            ImportWizardStatus::Failed(message) => message.clone(),
        };
        let has_header = wizard.has_header;
        let mut row = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_sm().child("Import"))
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&wizard.path).id("import-path")),
            )
            .child(
                Checkbox::new(SharedString::from("import-has-header"))
                    .label("Header row")
                    .checked(has_header)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(wizard) = this.import_wizard.as_mut() {
                            wizard.has_header = !wizard.has_header;
                            wizard.preview = None;
                            wizard.mapping_inputs = Vec::new();
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("import-load-preview")
                    .label("Load preview")
                    .small()
                    .disabled(running)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.load_import_preview(window, cx);
                    })),
            )
            .child(
                Button::new("import-policy-abort")
                    .label("Abort on error")
                    .small()
                    .when(wizard.policy == ImportErrorPolicy::Abort, |b| b.primary())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(wizard) = this.import_wizard.as_mut() {
                            wizard.policy = ImportErrorPolicy::Abort;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("import-policy-skip")
                    .label("Skip bad rows")
                    .small()
                    .when(wizard.policy == ImportErrorPolicy::SkipAndRecord, |b| {
                        b.primary()
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(wizard) = this.import_wizard.as_mut() {
                            wizard.policy = ImportErrorPolicy::SkipAndRecord;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("import-run")
                    .label("Run")
                    .small()
                    .disabled(running)
                    .on_click(cx.listener(|this, _, _, cx| this.run_import(cx))),
            );
        if running {
            row = row.child(
                Button::new("import-cancel")
                    .label("Cancel")
                    .small()
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(WorkspaceEvent::ImportCancel);
                    })),
            );
        }
        row = row.child(
            Button::new("import-close")
                .label("Close")
                .small()
                .on_click(cx.listener(|this, _, _, cx| this.close_import_wizard(cx))),
        );
        let mut panel = v_flex()
            .id("import-wizard")
            .test_support()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(row);
        if let Some(error) = &wizard.preview_error {
            panel = panel.child(
                div()
                    .id("import-preview-error")
                    .test_support()
                    .aria_label(error.clone())
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if let Some(preview) = &wizard.preview {
            let header_line = preview.columns.join(",");
            let sample_lines: Vec<String> = preview.rows.iter().map(|r| r.join(",")).collect();
            let preview_label = if sample_lines.is_empty() {
                header_line.clone()
            } else {
                format!("{header_line}\n{}", sample_lines.join("\n"))
            };
            panel = panel.child(
                div()
                    .id("import-preview")
                    .test_support()
                    .aria_label(preview_label)
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(header_line),
            );
            for (i, csv_name) in preview.columns.iter().enumerate() {
                let mut row = h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child(csv_name.clone()));
                if let Some(input) = wizard.mapping_inputs.get(i) {
                    row = row.child(div().flex_1().child(
                        Input::new(input).id(SharedString::from(format!("import-map-{i}"))),
                    ));
                }
                panel = panel.child(row);
            }
        }
        panel = panel.child(
            div()
                .id("import-status")
                .test_support()
                .aria_label(status_text.clone())
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(status_text),
        );
        if let ImportWizardStatus::Done(report) = &wizard.status
            && !report.skipped.is_empty()
        {
            let shown = report.skipped.iter().take(20).map(|(row, reason)| {
                div()
                    .text_sm()
                    .child(format!("row {row}: {reason}"))
                    .into_any_element()
            });
            let mut skipped = v_flex()
                .id("import-skipped")
                .test_support()
                .aria_label(
                    report
                        .skipped
                        .iter()
                        .map(|(row, reason)| format!("row {row}: {reason}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
                .gap_1();
            for line in shown {
                skipped = skipped.child(line);
            }
            if report.skipped_total as usize > report.skipped.len() {
                skipped = skipped.child(div().text_sm().child(format!(
                    "… and {} more skipped rows.",
                    report.skipped_total as usize - report.skipped.len()
                )));
            }
            panel = panel.child(skipped);
        }
        Some(panel.into_any_element())
    }

    fn render_audit(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.audit.as_ref()?;
        let theme = cx.theme().clone();
        let entries = self.audit_entries(cx);
        let rows: Vec<AnyElement> = if panel.loading && panel.entries.is_empty() {
            vec![
                div()
                    .id("audit-loading")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading…")
                    .into_any_element(),
            ]
        } else if entries.is_empty() {
            vec![
                div()
                    .id("audit-empty")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No audit rows match.")
                    .into_any_element(),
            ]
        } else {
            entries
                .iter()
                .enumerate()
                .map(|(ix, entry)| {
                    let (ok, colour, badge) = match entry.outcome {
                        fg_core::Outcome::Ok => (true, theme.success, "ok"),
                        fg_core::Outcome::Error => (false, theme.danger, "error"),
                        fg_core::Outcome::Cancelled => (false, theme.warning, "cancelled"),
                        fg_core::Outcome::NotRun => (false, theme.muted_foreground, "not run"),
                        _ => (false, theme.muted_foreground, "?"),
                    };
                    let _ = ok;
                    let actor = match &entry.actor {
                        fg_core::Actor::Mcp { client } => client.clone(),
                        fg_core::Actor::User => "user".to_owned(),
                        _ => "system".to_owned(),
                    };
                    let statement = entry.statement.replace('\n', " ");
                    let when = relative_time(entry.at);
                    let detail = match entry.duration_ms {
                        Some(ms) => format!("{badge} · {ms} ms · {when}"),
                        None => format!("{badge} · {when}"),
                    };
                    let mut detail = detail;
                    if let Some(error) = &entry.error {
                        detail.push_str(&format!(" · {error}"));
                    }
                    div()
                        .id(SharedString::from(format!("audit-{ix}")))
                        .test_support()
                        .aria_label(statement.clone())
                        .px_3()
                        .py_1p5()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{actor} · {}", entry.connection_name)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .font_family(theme.mono_font_family.clone())
                                .text_sm()
                                .text_color(theme.foreground)
                                .child(statement),
                        )
                        .child(div().text_xs().text_color(colour).child(detail))
                        .into_any_element()
                })
                .collect()
        };
        let export_status = panel.export_status.clone();
        Some(
            div()
                .id("audit-panel")
                .test_support()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(420.))
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_lg()
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("Audit log"),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("audit-close")
                                .icon(hugeicon(Hugeicon::Close))
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.close_audit(cx))),
                        ),
                )
                .child(Input::new(&panel.search).id("audit-search"))
                .child(
                    v_flex()
                        .id("audit-list")
                        .test_support()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(rows)
                        .into_any_element(),
                )
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(Input::new(&panel.export_path).id("audit-export-path"))
                        .child(
                            Button::new("audit-export")
                                .label("Export CSV")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let path = this
                                        .audit
                                        .as_ref()
                                        .map(|p| p.export_path.read(cx).value().to_string())
                                        .unwrap_or_default();
                                    cx.emit(WorkspaceEvent::AuditExportRequested { path });
                                })),
                        ),
                )
                .child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(export_status.unwrap_or_default()),
                )
                .into_any_element(),
        )
    }

    fn render_history(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.history.as_ref()?;
        let theme = cx.theme().clone();
        let rows: Vec<AnyElement> = if panel.loading && panel.entries.is_empty() {
            vec![
                div()
                    .id("history-loading")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading…")
                    .into_any_element(),
            ]
        } else if panel.entries.is_empty() {
            vec![
                div()
                    .id("history-empty")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No statements yet.")
                    .into_any_element(),
            ]
        } else {
            panel
                .entries
                .iter()
                .enumerate()
                .map(|(ix, entry)| {
                    let selected = ix == panel.selected;
                    let (ok, colour, badge) = match entry.outcome {
                        Outcome::Ok => (true, theme.success, "ok"),
                        Outcome::Error => (false, theme.danger, "error"),
                        Outcome::Cancelled => (false, theme.warning, "cancelled"),
                        Outcome::NotRun => (false, theme.muted_foreground, "not run"),
                        _ => (false, theme.muted_foreground, "?"),
                    };
                    let sql = entry.sql.replace('\n', " ");
                    let when = relative_time(entry.executed_at);
                    let detail = match entry.duration_ms {
                        Some(ms) => format!("{badge} · {ms} ms · {when}"),
                        None => format!("{badge} · {when}"),
                    };
                    div()
                        .id(SharedString::from(format!("history-{ix}")))
                        .test_support()
                        .aria_label(sql.clone())
                        .px_3()
                        .py_1p5()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(theme.muted.opacity(0.6)))
                        .when(!selected, |d| d.hover(|s| s.bg(theme.muted.opacity(0.35))))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .font_family(theme.mono_font_family.clone())
                                .text_sm()
                                .text_color(if ok { theme.foreground } else { colour })
                                .child(sql),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if ok { theme.muted_foreground } else { colour })
                                .child(detail),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_history(ix, cx);
                            this.pick_history(cx);
                        }))
                        .into_any_element()
                })
                .collect()
        };

        Some(
            div()
                .id("history-panel")
                .test_support()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(420.))
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_lg()
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("Query history"),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("history-close")
                                .icon(hugeicon(Hugeicon::Close))
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.close_history(cx))),
                        ),
                )
                .child(div().px_3().py_2().child(Input::new(&panel.search).small()))
                .child(
                    div()
                        .id("history-list")
                        .test_support()
                        .key_context(HISTORY_CONTEXT)
                        .track_focus(&self.focus)
                        .on_action(
                            cx.listener(|this, _: &HistoryOpen, _, cx| this.pick_history(cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &HistoryNext, _, cx| this.history_next(1, cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &HistoryPrev, _, cx| this.history_next(-1, cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &HistoryDismiss, _, cx| this.close_history(cx)),
                        )
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// The snippets panel: saved fragments, searchable, insertable, deletable.
    fn render_snippets(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.snippets.as_ref()?;
        let theme = cx.theme().clone();
        let visible = self.visible_snippets(cx);
        let rows: Vec<AnyElement> = if panel.loading && panel.entries.is_empty() {
            vec![
                div()
                    .id("snippets-loading")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading…")
                    .into_any_element(),
            ]
        } else if visible.is_empty() {
            vec![
                div()
                    .id("snippets-empty")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(if panel.entries.is_empty() {
                        "No snippets yet. Save the current statement to make one."
                    } else {
                        "No snippet matches that search."
                    })
                    .into_any_element(),
            ]
        } else {
            visible
                .iter()
                .enumerate()
                .filter_map(|(row, &ix)| {
                    let snippet = panel.entries.get(ix)?;
                    let selected = row == panel.selected;
                    let sql = snippet.sql.replace('\n', " ");
                    Some(
                        div()
                            .id(SharedString::from(format!("snippet-{row}")))
                            .test_support()
                            .aria_label(snippet.title.clone())
                            .px_3()
                            .py_1p5()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(theme.muted.opacity(0.6)))
                            .when(!selected, |d| d.hover(|s| s.bg(theme.muted.opacity(0.35))))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(snippet.title.clone()),
                                    )
                                    .child(
                                        div().text_xs().text_color(theme.muted_foreground).child(
                                            if snippet.connection_id.is_some() {
                                                "this connection"
                                            } else {
                                                "global"
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(sql),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(panel) = this.snippets.as_mut() {
                                    panel.selected = row;
                                }
                                this.pick_snippet(cx);
                            }))
                            .into_any_element(),
                    )
                })
                .collect()
        };

        Some(
            div()
                .id("snippets-panel")
                .test_support()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(420.))
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_lg()
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("Snippets"),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("snippet-save")
                                .icon(hugeicon(Hugeicon::Plus))
                                .label("Save current")
                                .ghost()
                                .small()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_current_as_snippet(cx)),
                                ),
                        )
                        .child(
                            Button::new("snippet-delete")
                                .icon(hugeicon(Hugeicon::Trash))
                                .ghost()
                                .small()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_selected_snippet(cx)),
                                ),
                        )
                        .child(
                            Button::new("snippets-close")
                                .icon(hugeicon(Hugeicon::Close))
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.close_snippets(cx))),
                        ),
                )
                .child(div().px_3().py_2().child(Input::new(&panel.search).small()))
                .when(!panel.unreadable.is_empty(), |d| {
                    d.child(
                        div()
                            .id("snippets-unreadable")
                            .test_support()
                            .px_3()
                            .pb_2()
                            .text_xs()
                            .text_color(theme.warning)
                            .child(format!(
                                "{} snippet(s) were written by a newer version and cannot be read.",
                                panel.unreadable.len()
                            )),
                    )
                })
                .child(
                    div()
                        .id("snippets-list")
                        .test_support()
                        .key_context(SNIPPETS_CONTEXT)
                        .track_focus(&self.focus)
                        .on_action(
                            cx.listener(|this, _: &SnippetOpen, _, cx| this.pick_snippet(cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &HistoryNext, _, cx| this.snippets_next(1, cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &HistoryPrev, _, cx| this.snippets_next(-1, cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &SnippetDismiss, _, cx| this.close_snippets(cx)),
                        )
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    fn render_sidebar(&self, view: &WeakEntity<Self>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let view = view.clone();
        let body = match &self.schema {
            SchemaState::Loading => div()
                .id("schema-loading")
                .test_support()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Loading…")
                .into_any_element(),
            SchemaState::Failed(message) => div()
                .id("schema-error")
                .test_support()
                .aria_label(message.clone())
                .p_3()
                .text_sm()
                .text_color(theme.danger)
                .child(message.clone())
                .into_any_element(),
            SchemaState::Ready => {
                let meta = Rc::clone(&self.meta);
                tree(&self.tree, move |ix, entry, selected, _, cx| {
                    let item = entry.item();
                    let theme = cx.theme().clone();
                    let info = meta.get(&item.id);
                    // 16px of indent per level, rows 22px tall, as dense as TablePlus's list.
                    let indent = px(8.0 + 16.0 * entry.depth() as f32);

                    if entry.is_folder() {
                        // Section header: `⌄ Tables`, in the list's own type like TablePlus.
                        // Headers highlight their own match against the filter, same as objects;
                        // a header shown only because its children match stays plain.
                        let chevron = if entry.is_expanded() {
                            Hugeicon::ArrowDown
                        } else {
                            Hugeicon::ArrowRight
                        };
                        let matched = meta
                            .get(&item.id)
                            .map(|m| m.matched.clone())
                            .unwrap_or_default();
                        return ListItem::new(ix).h(px(22.)).py_0().pl(indent).child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    hugeicon(chevron)
                                        .size_4()
                                        .text_color(theme.muted_foreground),
                                )
                                .child(
                                    div()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_size(px(13.))
                                        .text_color(theme.foreground)
                                        .child(highlighted(&item.label, &matched)),
                                ),
                        );
                    }

                    let icon = match info.map(|m| m.kind) {
                        Some(NodeKind::Object(ObjectKind::View)) => Hugeicon::View,
                        _ => Hugeicon::Table,
                    };
                    let icon_color = if icon == Hugeicon::View {
                        hsla(270.0 / 360.0, 0.80, 0.60, 1.0)
                    } else {
                        hsla(205.0 / 360.0, 0.90, 0.48, 1.0)
                    };
                    let target = info.and_then(|m| m.path.clone());
                    let estimate = info.and_then(|m| m.row_estimate).map(compact_count);
                    let matched = info.map(|m| m.matched.clone()).unwrap_or_default();
                    let view = view.clone();
                    // TablePlus marks the selected object with a solid accent bar and white text.
                    ListItem::new(ix)
                        .h(px(22.))
                        .py_0()
                        .selected(selected)
                        .pl(indent)
                        .rounded_md()
                        .when(selected, |d| {
                            d.bg(theme.primary).text_color(theme.primary_foreground)
                        })
                        .child(
                            h_flex()
                                .group("tree-row")
                                .w_full()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_size(px(13.))
                                        .child(hugeicon(icon).size_4().text_color(if selected {
                                            theme.primary_foreground
                                        } else {
                                            icon_color
                                        }))
                                        .child(highlighted(&item.label, &matched)),
                                )
                                // TablePlus shows no counts; the estimate appears on hover only.
                                .when_some(estimate, |d, e| {
                                    d.child(
                                        div()
                                            .pr_2()
                                            .text_xs()
                                            .opacity(0.)
                                            .group_hover("tree-row", |s| s.opacity(1.))
                                            .text_color(if selected {
                                                theme.primary_foreground.opacity(0.8)
                                            } else {
                                                theme.muted_foreground
                                            })
                                            .child(e),
                                    )
                                }),
                        )
                        .on_click(move |event, window, cx| {
                            if event.click_count() >= 2
                                && let Some(path) = target.clone()
                            {
                                let _ =
                                    view.update(cx, |this, cx| this.open_table(&path, window, cx));
                            }
                        })
                })
                .into_any_element()
            }
        };

        v_flex()
            .id("sidebar")
            .test_support()
            .size_full()
            .bg(theme.sidebar)
            .child(
                // TablePlus's switch: plain labels, the active one on a grey pill.
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1p5()
                    .gap_1()
                    .justify_center()
                    .children(["Items", "Queries", "History"].map(|label| {
                        let active = label == "Items";
                        div()
                            .px_3()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_size(px(13.))
                            .cursor_pointer()
                            .when(active, |d| {
                                d.bg(theme.secondary_active).text_color(theme.foreground)
                            })
                            .when(!active, |d| {
                                d.text_color(theme.muted_foreground)
                                    .hover(|d| d.text_color(theme.foreground))
                            })
                            .child(label)
                    })),
            )
            .child(
                h_flex()
                    .px_3()
                    .pb_2()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.filter)
                                .id("sidebar-filter")
                                .small()
                                .prefix(
                                    hugeicon(Hugeicon::Search)
                                        .size_4()
                                        .text_color(theme.muted_foreground),
                                )
                                .cleanable(true),
                        ),
                    )
                    .child(
                        hugeicon(Hugeicon::Sliders)
                            .size_5()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .child(
                h_flex()
                    .h_7()
                    .px_2()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        Button::new("sidebar-add")
                            .icon(hugeicon(Hugeicon::Plus))
                            .ghost()
                            .small(),
                    )
                    .child(
                        Button::new("sidebar-menu")
                            .icon(hugeicon(Hugeicon::ArrowDown))
                            .ghost()
                            .small(),
                    ),
            )
            .into_any_element()
    }

    /// The tab strip, as TablePlus draws it: back / forward arrows, then the tabs sharing the width,
    /// each title centered, the active one on a grey pill, the close button on hover. Tabs drag to
    /// reorder. `None` when nothing is open.
    fn render_tab_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.tabs.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let is_zh = crate::views::style::is_zh_locale();
        let active = self.tabs.active_id().cloned();
        let tabs: Vec<AnyElement> = self
            .tabs
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, t)| {
                let title = match &t.kind {
                    TabKind::Table { path, .. } => table_title(path),
                    TabKind::Query { .. } => self
                        .query_view(&t.id)
                        .map(|tab| tab.read(cx).title())
                        .unwrap_or_else(|| "Query".to_owned()),
                };
                // A table tab with staged edits shows a dot, so unsaved work is visible from any tab.
                let dirty = matches!(t.kind, TabKind::Table { .. })
                    && self
                        .tab_view(&t.id)
                        .is_some_and(|tab| tab.read(cx).pending(cx).total() > 0);
                let is_active = active.as_ref() == Some(&t.id);
                let (select_id, close_id, drag_id) = (t.id.clone(), t.id.clone(), t.id.clone());
                let drag_title = title.clone();
                let hover_bg = theme.list_hover;
                let hover_fg = theme.foreground;
                h_flex()
                    .id(SharedString::from(format!("tab-{}", t.id)))
                    .test_support()
                    .aria_label(title.clone())
                    .group("tab")
                    .relative()
                    .h(px(24.))
                    .flex_1()
                    .min_w(px(80.))
                    .px_7()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_size(px(13.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(theme.secondary).text_color(theme.foreground)
                    })
                    .when(!is_active, |d| {
                        d.text_color(theme.muted_foreground)
                            .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
                    })
                    .when(dirty, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("tab-dirty-{}", t.id)))
                                .test_support()
                                .aria_label("Unsaved changes")
                                .mr_1p5()
                                .text_color(theme.warning)
                                .child("●"),
                        )
                    })
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(title),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_1()
                            .opacity(0.)
                            .group_hover("tab", |s| s.opacity(1.))
                            .child(
                                Button::new(SharedString::from(format!("close-tab-{}", t.id)))
                                    .icon(hugeicon(Hugeicon::Close))
                                    .ghost()
                                    .small()
                                    .tooltip(if is_zh {
                                        "关闭标签页"
                                    } else {
                                        "Close tab"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.close_tab(&close_id, cx);
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(&select_id, cx)))
                    // Dragging a tab onto another puts it in that tab's place.
                    .on_drag(
                        DraggedTab {
                            id: drag_id,
                            title: drag_title.into(),
                        },
                        |dragged, _, _, cx| cx.new(|_| dragged.clone()),
                    )
                    .drag_over::<DraggedTab>(move |style, _, _, cx| {
                        style.bg(cx.theme().drop_target)
                    })
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                        this.move_tab_to(&dragged.id, index, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        let several = self.tabs.tabs().len() > 1;
        Some(
            h_flex()
                .id("tab-strip")
                .test_support()
                .h(px(32.))
                .w_full()
                .flex_shrink_0()
                .overflow_hidden()
                .px_1p5()
                .gap_1()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.tab_bar)
                .child(
                    Button::new("tab-prev")
                        .icon(hugeicon(Hugeicon::ArrowLeft))
                        .ghost()
                        .small()
                        .tooltip(if is_zh {
                            "上一个标签页"
                        } else {
                            "Previous tab"
                        })
                        .disabled(!several)
                        .on_click(cx.listener(|this, _, _, cx| this.prev_tab(cx))),
                )
                .child(
                    Button::new("tab-next")
                        .icon(hugeicon(Hugeicon::ArrowRight))
                        .ghost()
                        .small()
                        .tooltip(if is_zh {
                            "下一个标签页"
                        } else {
                            "Next tab"
                        })
                        .disabled(!several)
                        .on_click(cx.listener(|this, _, _, cx| this.next_tab(cx))),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .items_center()
                        .children(tabs),
                )
                .into_any_element(),
        )
    }

    /// Move a tab to another tab's position (drag and drop).
    pub fn move_tab_to(&mut self, id: &TabId, to: usize, cx: &mut Context<Self>) {
        if let Some(from) = self.tabs.tabs().iter().position(|t| &t.id == id) {
            self.tabs.move_tab(from, to);
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.notify();
        }
    }

    fn render_main(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let strip = self.render_tab_strip(cx);
        let confirm_banner = self.confirm_close.clone().and_then(|id| {
            let pending = self
                .tab_view(&id)
                .map(|tab| tab.read(cx).pending(cx).total())
                .unwrap_or(0);
            if pending == 0 {
                return None;
            }
            let is_zh = crate::views::style::is_zh_locale();
            let close_id = id.clone();
            Some(
                h_flex()
                    .id("confirm-close-tab")
                    .test_support()
                    .w_full()
                    .flex_shrink_0()
                    .gap_3()
                    .px_3()
                    .py_1p5()
                    .items_center()
                    .border_b_1()
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
                                format!("关闭此标签页将丢失 {pending} 项未保存的更改，确认关闭吗？")
                            } else {
                                format!(
                                    "Closing this tab will lose {pending} unsaved changes. Close anyway?"
                                )
                            }),
                    )
                    .child(
                        Button::new("confirm-close-yes")
                            .label(if is_zh { "确认关闭" } else { "Close tab" })
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_tab(&close_id, cx);
                            })),
                    )
                    .child(
                        Button::new("confirm-close-no")
                            .label(if is_zh { "保留" } else { "Keep tab" })
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_close(cx);
                            })),
                    )
                    .into_any_element(),
            )
        });

        let content: AnyElement = match self.tabs.active().and_then(|t| self.tab_views.get(&t.id)) {
            Some(TabContent::Table(tab)) => tab.clone().into_any_element(),
            Some(TabContent::Query(tab)) => tab.clone().into_any_element(),
            None => div()
                .id("no-tab")
                .test_support()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Double-click a table in the sidebar to open it.")
                .into_any_element(),
        };

        let export_panel = self.render_export_panel(cx);
        let import_panel = self.render_import_panel(cx);
        let approvals_panel = self.render_mcp_approvals_panel(cx);
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(strip)
            .children(confirm_banner)
            .children(approvals_panel)
            .children(export_panel)
            .children(import_panel)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    fn render_details_panel(&self, cx: &App) -> AnyElement {
        let theme = cx.theme().clone();
        let active_tab = self
            .tabs
            .active()
            .and_then(|t| self.tab_views.get(&t.id))
            .and_then(|content| match content {
                // The inspector shows the selected row of a grid; a query tab has no row context.
                TabContent::Table(tab) => Some(tab),
                TabContent::Query(_) => None,
            });
        let row_data = active_tab.and_then(|tab| tab.read(cx).selected_row_detail(cx));
        let query = self.details_filter.read(cx).value().to_lowercase();

        let content = match row_data {
            Some(data) => {
                let fields: Vec<AnyElement> = data
                    .fields
                    .into_iter()
                    .filter(|f| query.is_empty() || f.name.to_lowercase().contains(&query))
                    .map(|f| {
                        // TablePlus's field: a small grey name over a boxed monospace value.
                        v_flex()
                            .gap_0p5()
                            .px_2()
                            .py_1()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(div().child(f.name))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .opacity(0.8)
                                            .child(f.data_type.to_lowercase()),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .min_h(px(24.))
                                    .px_1p5()
                                    .py(px(3.))
                                    .rounded(px(5.))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(13.))
                                    .when(f.is_null, |d| {
                                        d.text_color(theme.muted_foreground.opacity(0.65))
                                    })
                                    .when(!f.is_null, |d| d.text_color(theme.foreground))
                                    .child(if f.is_null {
                                        "NULL".to_string()
                                    } else {
                                        f.value_text
                                    }),
                            )
                            .into_any_element()
                    })
                    .collect();
                v_flex()
                    .id("details-fields")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .children(fields)
                    .into_any_element()
            }
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No row selected")
                .into_any_element(),
        };

        v_flex()
            .id("details-panel")
            .test_support()
            .w(px(320.))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .px_2()
                    .py_2()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.details_filter)
                                .id("details-filter")
                                .small()
                                .prefix(
                                    hugeicon(Hugeicon::Search)
                                        .size_4()
                                        .text_color(theme.muted_foreground),
                                )
                                .cleanable(true),
                        ),
                    )
                    .child(
                        hugeicon(Hugeicon::Sliders)
                            .size_5()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .child(content)
            .into_any_element()
    }
}

/// Window options for a connection window: the toolbar is taller than the stock title bar
/// ([`TOOLBAR_HEIGHT`]), so the macOS traffic lights are re-centered to match.
pub fn workspace_window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1280.), px(800.)),
            cx,
        ))),
        window_min_size: Some(size(px(800.), px(500.))),
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(point(px(9.0), (TOOLBAR_HEIGHT - px(12.)) / 2.0)),
        }),
        ..TitleBar::window_options()
    }
}

/// Open a connection's window. Returns the window and its view so the caller can wire a
/// controller to the view.
pub fn open_workspace_window(
    cx: &mut App,
    config: ConnectionConfig,
) -> Result<(AnyWindowHandle, Entity<WorkspaceView>), String> {
    let options = workspace_window_options(cx);
    gpui_kit::open_window(options, cx, |window, cx| {
        crate::theme_control::follow_system(window);
        crate::theme_control::compact_type(cx);
        let view = cx.new(|cx| WorkspaceView::new(config, window, cx));
        // Shortcuts only reach a window that has focus somewhere inside it.
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        view
    })
    .map_err(|e| e.to_string())
}

/// Used by the controller to start a table tab from saved state.
pub fn restore_query(
    path: ObjectPath,
    default_page_size: u32,
    saved: &TableViewState,
) -> GridQuery {
    GridQuery::restore(path, default_page_size, saved)
}

#[allow(dead_code)]
fn _kinds_in_use(_: ObjectKind, _: Option<Arc<OpenConnection>>, _: Option<Service>) {}

/// Guard levels in increasing strictness. The lock button walks this order, so the next click
/// always makes the connection safer before it wraps around to `Normal`.
const SAFE_MODE_ORDER: [SafeMode; 4] = [
    SafeMode::Normal,
    SafeMode::Alert,
    SafeMode::Strict,
    SafeMode::ReadOnly,
];

/// The next level after `mode`. Unknown collapses to the strictest known level first, matching
/// `SafeMode::effective`.
pub fn next_safe_mode(mode: SafeMode) -> SafeMode {
    let current = mode.effective();
    let index = SAFE_MODE_ORDER
        .iter()
        .position(|m| *m == current)
        .unwrap_or(SAFE_MODE_ORDER.len() - 1);
    SAFE_MODE_ORDER[(index + 1) % SAFE_MODE_ORDER.len()]
}

/// What the lock button's tooltip says. Levels are named, not numbered: "Level 1" means nothing
/// to a user deciding whether to run a DELETE.
pub fn safe_mode_label(mode: SafeMode) -> &'static str {
    match mode.effective() {
        SafeMode::Normal => "Normal (run everything)",
        SafeMode::Alert => "Alert (confirm dangerous statements)",
        SafeMode::Strict => "Strict (confirm every write)",
        SafeMode::ReadOnly => "Read-only (no writes)",
        _ => "Read-only (no writes)",
    }
}

/// "3 min ago", "2 h ago", "12 Oct" - enough to tell two similar statements apart without a
/// date library. Future timestamps (clock skew) read as "just now".
/// The audit panel's local filter: actor, connection, or statement
/// contains the text, case-insensitive. Empty means everything.
fn filter_audit_entries(entries: &[fg_core::AuditEntry], filter: &str) -> Vec<fg_core::AuditEntry> {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return entries.to_vec();
    }
    entries
        .iter()
        .filter(|e| {
            let actor = match &e.actor {
                fg_core::Actor::Mcp { client } => client.clone(),
                fg_core::Actor::User => "user".to_owned(),
                _ => "system".to_owned(),
            };
            format!("{} {} {}", actor, e.connection_name, e.statement)
                .to_lowercase()
                .contains(&filter)
        })
        .cloned()
        .collect()
}

fn relative_time(at: fg_core::UnixMillis) -> String {
    let now = fg_core::UnixMillis::now().0;
    let delta = now.saturating_sub(at.0);
    let minutes = delta / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        1440..=10079 => format!("{} d ago", minutes / 1440),
        _ => {
            // Older than a week: a date is more useful than a big number of days.
            let seconds = at.0.div_euclid(1000);
            let days = seconds.div_euclid(86_400);
            let (year, month, day) = civil_from_days(days);
            format!("{day:02} {month} {year}")
        }
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn month_name_from(month: u32) -> &'static str {
    MONTHS
        .get(usize::try_from(month.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or("")
}

/// Days since 1970-01-01 to a civil date. Howard Hinnant's algorithm: exact for every date the
/// i64 millisecond range can express, with no dependency and no timezone.
fn civil_from_days(days: i64) -> (i64, &'static str, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let month = u32::try_from(m).unwrap_or(1);
    (year, month_name_from(month), d)
}

/// A snippet's default title: the first non-blank line of the SQL, shortened. Falls back to the
/// document's title when the SQL is only punctuation.
fn first_line_title(sql: &str, fallback: String) -> String {
    let line = sql
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("--"))
        .unwrap_or("");
    let cleaned = line.trim_end_matches(';').trim();
    if cleaned.is_empty() {
        return fallback;
    }
    // Enough to recognise it in a list, not so much that the row wraps.
    let mut title: String = cleaned.chars().take(60).collect();
    if cleaned.chars().count() > 60 {
        title.push('…');
    }
    title
}
