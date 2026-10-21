//! A connection's window: title bar, sidebar with the object tree, tab strip and the active tab.
//!
//! The view owns the UI state (tabs, tree, filter). Anything that needs the database goes through
//! events ([`WorkspaceEvent`]) answered by [`crate::controller`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use fg_core::{
    ConnectionConfig, ObjectInfo, ObjectKind, ObjectPath, Outcome, QueryHistoryEntry, SafeMode,
    TabId, TabKind, TableViewMode, TableViewState, WorkspaceState,
};
use fg_service::{OpenConnection, Service};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
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
    CloseTab, HISTORY_CONTEXT, HistoryDismiss, HistoryNext, HistoryOpen, HistoryPrev, NewQuery,
    NextTab, PrevTab, QuickOpen, RefreshSchema, SNIPPETS_CONTEXT, SaveSnippet, SnippetDismiss,
    SnippetOpen, ToggleFilters, ToggleHistory, ToggleSidebar, ToggleSnippets, WORKSPACE_CONTEXT,
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
    /// A query tab was created: the controller supplies its draft and streams its runs.
    QueryOpened { tab: Entity<QueryTab> },
    /// `⌘T`: the controller creates the draft (it owns ids and timestamps) and opens the tab.
    NewQueryRequested,
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
    /// `⌘Y` / the history button: load the recent statements of this connection.
    HistoryRequested { search: String },
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
    sidebar_visible: bool,
    details_visible: bool,
    focus: FocusHandle,
    quick: Option<QuickOpenState>,
    /// The `⌘Y` panel: recent statements of this connection, newest first.
    history: Option<HistoryPanel>,
    /// The snippets panel (`⌘⇧S`): saved fragments, per connection plus global ones.
    snippets: Option<SnippetsPanel>,
    /// What the connection is set to, as far as this window knows. `None` until the controller
    /// tells us; the controller is the one that persists a change.
    safe_mode: SafeMode,
    _subscriptions: Vec<Subscription>,
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

/// How many matches the palette lists.
const QUICK_RESULTS: usize = 50;

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
            sidebar_visible: true,
            details_visible: true,
            focus: cx.focus_handle(),
            quick: None,
            history: None,
            snippets: None,
            safe_mode: config_safe_mode,
            _subscriptions: vec![on_filter, on_tree, on_details],
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
            let tab = cx.new(|cx| TableTab::new(query_path, window, cx));
            // Persist view state when the user changes the query, and let the controller load.
            let on_tab = cx.subscribe_in(
                &tab,
                window,
                |this, _, event: &TableTabEvent, window, cx| match event {
                    TableTabEvent::Reload => cx.emit(WorkspaceEvent::SessionChanged),
                    // Saved by the controller together with the table's other view state.
                    TableTabEvent::ViewChanged => {}
                    TableTabEvent::FollowForeignKey { table, filter } => {
                        this.open_table_filtered(table, filter.clone(), window, cx);
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
        filter: fg_core::Filter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_table(path, window, cx);
        let Some(id) = self.tabs.active_id().cloned() else {
            return;
        };
        if let Some(TabContent::Table(tab)) = self.tab_views.get(&id).cloned() {
            tab.update(cx, |t, cx| t.apply_filter(filter, cx));
        }
    }

    pub fn activate_tab(&mut self, id: &TabId, cx: &mut Context<Self>) {
        if self.tabs.activate(id) {
            cx.emit(WorkspaceEvent::SessionChanged);
            cx.notify();
        }
    }

    pub fn close_tab(&mut self, id: &TabId, cx: &mut Context<Self>) {
        self.tabs.close(id);
        self.tab_views.remove(id);
        cx.emit(WorkspaceEvent::SessionChanged);
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

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
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
        let breadcrumb_text = match &active_table_name {
            Some(table) => format!("{} / {}", db_name, table),
            None => db_name.clone(),
        };

        let env_color =
            super::style::environment_color(cx, self.config.environment, self.config.color);
        let capsule_bg = gpui::transparent_black();
        let capsule = move || {
            h_flex()
                .items_center()
                .gap_0p5()
                .p_0p5()
                .rounded_lg()
                .bg(capsule_bg)
        };
        let divider = {
            let color = theme.border;
            move || div().w(px(2.)).h(px(14.)).bg(color.opacity(0.0))
        };

        let title = TitleBar::new().child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            capsule()
                                .child(
                                    Button::new("toggle-sidebar")
                                        .icon(hugeicon(Hugeicon::SidebarLeft))
                                        .ghost()
                                        .small()
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                                        ),
                                )
                                .child(divider())
                                .child(
                                    Button::new("connections-btn")
                                        .icon(hugeicon(Hugeicon::Plug))
                                        .ghost()
                                        .small(),
                                ),
                        )
                        .child(
                            capsule()
                                .child(
                                    Button::new("stop-btn")
                                        .icon(hugeicon(Hugeicon::CircleX))
                                        .ghost()
                                        .small()
                                        .tooltip("Cancel the running statement")
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
                                    Button::new("preview-btn")
                                        .icon(hugeicon(Hugeicon::View))
                                        .ghost()
                                        .small(),
                                )
                                .child(divider())
                                .child(
                                    Button::new("history-btn")
                                        .icon(hugeicon(Hugeicon::History))
                                        .ghost()
                                        .small()
                                        .tooltip("Query history")
                                        .when(self.history.is_some(), |b| b.primary())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_history(window, cx)
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
                                        .small()
                                        .tooltip(format!(
                                            "Safe mode: {}",
                                            safe_mode_label(self.safe_mode)
                                        ))
                                        .when(self.safe_mode != SafeMode::Normal, |b| b.primary())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.emit(WorkspaceEvent::SafeModeChanged {
                                                mode: next_safe_mode(this.safe_mode),
                                            })
                                        })),
                                )
                                .child(divider())
                                .child(
                                    Button::new("database-btn")
                                        .icon(hugeicon(Hugeicon::Database))
                                        .ghost()
                                        .small(),
                                )
                                .child(divider())
                                .child(
                                    Button::new("sql-console")
                                        .ghost()
                                        .small()
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(WorkspaceEvent::NewQueryRequested)
                                        }))
                                        .child(
                                            div()
                                                .text_size(px(14.))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(theme.foreground)
                                                .child("SQL"),
                                        ),
                                ),
                        )
                        .child(
                            h_flex()
                                .id("connection-pill")
                                .test_support()
                                .aria_label(self.config.name.clone())
                                .gap_2()
                                .ml_1()
                                .px_3()
                                .py_1()
                                .items_center()
                                .rounded_full()
                                .bg(theme.muted)
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .child(div().size_2().rounded_full().bg(env_color))
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.foreground)
                                        .child(self.config.name.clone()),
                                )
                                .child(div().opacity(0.5).child("/"))
                                .child(breadcrumb_text),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            capsule()
                                .child(
                                    Button::new("info-btn")
                                        .icon(hugeicon(Hugeicon::Alert).text_color(theme.warning))
                                        .ghost()
                                        .small(),
                                )
                                .child(divider())
                                .child(
                                    Button::new("chart-btn")
                                        .icon(hugeicon(Hugeicon::Chart))
                                        .ghost()
                                        .small(),
                                )
                                .child(divider())
                                .child(
                                    Button::new("refresh-schema")
                                        .icon(hugeicon(Hugeicon::Refresh))
                                        .ghost()
                                        .small()
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
                                        .small()
                                        .tooltip(format!(
                                            "Theme: {}",
                                            crate::theme_control::label(
                                                crate::theme_control::chosen(cx)
                                            )
                                        ))
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(WorkspaceEvent::CycleTheme)
                                        })),
                                ),
                        )
                        .child(
                            capsule()
                                .child(
                                    Button::new("toggle-bottom")
                                        .icon(hugeicon(Hugeicon::PanelBottom))
                                        .ghost()
                                        .small(),
                                )
                                .child(divider())
                                .child(
                                    Button::new("toggle-details")
                                        .icon(hugeicon(Hugeicon::SidebarRight))
                                        .ghost()
                                        .small()
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
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.close_active_tab(cx)))
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
            .children(snippets)
            .children(overlay)
    }
}

impl WorkspaceView {
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

    /// The `⌘Y` panel: recent statements, newest first, with their outcome. Drawn as an overlay
    /// like Quick Open so it does not steal space from the editor.
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
                                .xsmall()
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
                                .xsmall()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_current_as_snippet(cx)),
                                ),
                        )
                        .child(
                            Button::new("snippet-delete")
                                .icon(hugeicon(Hugeicon::Trash))
                                .ghost()
                                .xsmall()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_selected_snippet(cx)),
                                ),
                        )
                        .child(
                            Button::new("snippets-close")
                                .icon(hugeicon(Hugeicon::Close))
                                .ghost()
                                .xsmall()
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
                    // 16px of indent per level, rows 24px tall.
                    let indent = px(8.0 + 16.0 * entry.depth() as f32);

                    if entry.is_folder() {
                        // Section header: TABLES (18)
                        let chevron = if entry.is_expanded() {
                            Hugeicon::ArrowDown
                        } else {
                            Hugeicon::ArrowRight
                        };
                        return ListItem::new(ix).h(px(28.)).py_0().pl(indent).child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    hugeicon(chevron)
                                        .size_5()
                                        .text_color(theme.muted_foreground),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.muted_foreground)
                                        .child(item.label.to_uppercase()),
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
                    ListItem::new(ix)
                        .h(px(28.))
                        .py_0()
                        .selected(selected)
                        .pl(indent)
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .text_sm()
                                        .child(hugeicon(icon).size_5().text_color(icon_color))
                                        .child(highlighted(&item.label, &matched)),
                                )
                                .when_some(estimate, |d, e| {
                                    d.child(
                                        div()
                                            .pr_2()
                                            .text_xs()
                                            .text_color(if selected {
                                                gpui::white().opacity(0.8)
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
                div().p_3().pb_2().child(
                    h_flex()
                        .p_1()
                        .gap_1()
                        .rounded_lg()
                        .bg(theme.muted)
                        .children(["Items", "Queries", "History"].map(|label| {
                            let active = label == "Items";
                            div()
                                .flex_1()
                                .text_center()
                                .py_1()
                                .rounded_md()
                                .text_size(px(13.))
                                .cursor_pointer()
                                .when(active, |d| {
                                    d.bg(theme.background)
                                        .shadow_sm()
                                        .text_color(theme.foreground)
                                        .font_weight(FontWeight::MEDIUM)
                                })
                                .when(!active, |d| d.text_color(theme.muted_foreground))
                                .child(label)
                        })),
                ),
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

    /// The tab strip, 32px high: icon, title and close button per tab, active tab raised, tabs
    /// draggable to reorder. `None` when nothing is open.
    fn render_tab_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.tabs.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
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
                let is_active = active.as_ref() == Some(&t.id);
                let (select_id, close_id, drag_id) = (t.id.clone(), t.id.clone(), t.id.clone());
                let drag_title = title.clone();
                let hover_bg = theme.list_hover;
                let hover_fg = theme.foreground;
                let tab_icon = match &t.kind {
                    TabKind::Table { .. } => Hugeicon::Table,
                    TabKind::Query { .. } => Hugeicon::Terminal,
                };
                h_flex()
                    .id(SharedString::from(format!("tab-{}", t.id)))
                    .test_support()
                    .aria_label(title.clone())
                    .group("tab")
                    .relative()
                    .h_full()
                    .min_w(px(120.))
                    .max_w(px(240.))
                    .flex_shrink_0()
                    .pl_4()
                    .pr_8()
                    .gap_2()
                    .items_center()
                    .border_b_2()
                    .text_size(px(14.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.border_color(theme.primary)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                    })
                    .when(!is_active, |d| {
                        d.border_color(gpui::transparent_black())
                            .text_color(theme.muted_foreground)
                            .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
                    })
                    .child(hugeicon(tab_icon).size_4().text_color(if is_active {
                        theme.primary
                    } else {
                        theme.muted_foreground
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(title),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_1()
                            .opacity(0.)
                            .group_hover("tab", |s| s.opacity(1.))
                            .child(
                                Button::new(SharedString::from(format!("close-tab-{}", t.id)))
                                    .icon(hugeicon(Hugeicon::Close))
                                    .ghost()
                                    .small()
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

        Some(
            h_flex()
                .id("tab-strip")
                .test_support()
                .h(px(44.))
                .w_full()
                .flex_shrink_0()
                .overflow_hidden()
                .px_2()
                .gap_1()
                .items_end()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.tab_bar)
                .child(
                    h_flex()
                        .gap_2()
                        .pr_2()
                        .text_color(theme.muted_foreground)
                        .child(hugeicon(Hugeicon::ArrowLeft).size_5())
                        .child(hugeicon(Hugeicon::ArrowRight).size_5()),
                )
                .children(tabs)
                .child(
                    div()
                        .pl_2()
                        .text_color(theme.muted_foreground)
                        .child(hugeicon(Hugeicon::SidebarRight).size_5()),
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

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(strip)
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
                        v_flex()
                            .gap_1()
                            .px_3()
                            .py_1p5()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .text_color(theme.muted_foreground)
                                            .child(f.name),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .text_color(theme.muted_foreground.opacity(0.8))
                                            .child(f.data_type.to_lowercase()),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .items_start()
                                    .justify_between()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .text_size(px(14.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .when(f.is_null, |d| {
                                                d.text_color(theme.muted_foreground.opacity(0.65))
                                                    .italic()
                                            })
                                            .when(!f.is_null, |d| d.text_color(theme.foreground))
                                            .child(if f.is_null {
                                                "NULL".to_string()
                                            } else {
                                                f.value_text
                                            }),
                                    )
                                    .child(
                                        hugeicon(Hugeicon::ArrowDown)
                                            .size_5()
                                            .text_color(theme.muted_foreground),
                                    ),
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
            .w(px(300.))
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

/// Open a connection's window. Returns the window and its view so the caller can wire a
/// controller to the view.
pub fn open_workspace_window(
    cx: &mut App,
    config: ConnectionConfig,
) -> Result<(AnyWindowHandle, Entity<WorkspaceView>), String> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1280.), px(800.)),
            cx,
        ))),
        window_min_size: Some(size(px(800.), px(500.))),
        ..TitleBar::window_options()
    };
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
