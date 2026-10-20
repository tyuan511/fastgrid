//! A connection's window: title bar, sidebar with the object tree, tab strip and the active tab.
//!
//! The view owns the UI state (tabs, tree, filter). Anything that needs the database goes through
//! events ([`WorkspaceEvent`]) answered by [`crate::controller`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use fg_core::{
    ConnectionConfig, ObjectInfo, ObjectKind, ObjectPath, TabId, TabKind, TableViewMode,
    TableViewState, WorkspaceState,
};
use fg_service::{OpenConnection, Service};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    CloseTab, NextTab, PrevTab, QuickOpen, RefreshSchema, ToggleFilters, ToggleSidebar,
    WORKSPACE_CONTEXT,
};
use super::grid::{TableTab, TableTabEvent};
use super::style::environment_color;
use crate::model::{
    GridQuery, NodeKind, QuickItem, TabSet, TreeNode, build_tree, quick_items, quick_search,
    table_title,
};

/// What the workspace asks its owner to do.
pub enum WorkspaceEvent {
    /// A table tab was created: the controller attaches its data loader.
    TableOpened {
        path: ObjectPath,
        tab: Entity<TableTab>,
    },
    /// Reload the schema (the refresh button).
    RefreshSchema,
    /// Tabs or layout changed: worth saving.
    SessionChanged,
}

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
    /// Tree node id -> the table it stands for, so a click can be resolved.
    paths: HashMap<SharedString, ObjectPath>,
    collapsed: HashSet<SharedString>,
    tabs: TabSet,
    tab_views: HashMap<TabId, Entity<TableTab>>,
    sidebar_visible: bool,
    focus: FocusHandle,
    quick: Option<QuickOpenState>,
    _subscriptions: Vec<Subscription>,
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
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables, views…"));
        let tree = cx.new(|cx| TreeState::new(cx));
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
        Self {
            config,
            schema: SchemaState::Loading,
            namespaces: Vec::new(),
            objects: BTreeMap::new(),
            filter,
            tree,
            paths: HashMap::new(),
            collapsed: HashSet::new(),
            tabs: TabSet::new(),
            tab_views: HashMap::new(),
            sidebar_visible: true,
            focus: cx.focus_handle(),
            quick: None,
            _subscriptions: vec![on_filter, on_tree],
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
        let Some(tab) = self.tabs.active().and_then(|t| self.tab_views.get(&t.id)) else {
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
        self.tab_views.get(id)
    }

    /// The tables and views currently listed in the sidebar (after filtering), in display order.
    pub fn visible_tables(&self) -> Vec<ObjectPath> {
        let mut shown: Vec<_> = self.paths.values().cloned().collect();
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
        self.paths.clear();
        collect_paths(&nodes, &mut self.paths);
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
            let on_tab = cx.subscribe(&tab, |_, _, _: &TableTabEvent, cx| {
                cx.emit(WorkspaceEvent::SessionChanged);
            });
            self._subscriptions.push(on_tab);
            self.tab_views.insert(id, tab.clone());
            cx.emit(WorkspaceEvent::TableOpened {
                path: path.clone(),
                tab,
            });
        }
        cx.emit(WorkspaceEvent::SessionChanged);
        cx.notify();
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

    pub fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    /// What to save so the next launch can pick up where this one left off.
    pub fn session(&self) -> WorkspaceState {
        self.tabs.to_workspace(WorkspaceState {
            sidebar_visible: self.sidebar_visible,
            ..WorkspaceState::default()
        })
    }

    /// Reopen tabs from a saved session. Tables that no longer exist are skipped. Call after the
    /// schema is loaded.
    pub fn restore(&mut self, saved: &WorkspaceState, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = saved.sidebar_visible;
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

fn collect_paths(nodes: &[TreeNode], out: &mut HashMap<SharedString, ObjectPath>) {
    for n in nodes {
        if let Some(p) = &n.path {
            out.insert(n.id.clone().into(), p.clone());
        }
        collect_paths(&n.children, out);
    }
}

fn to_items(
    nodes: &[TreeNode],
    collapsed: &HashSet<SharedString>,
    filtering: bool,
) -> Vec<TreeItem> {
    nodes
        .iter()
        .map(|n| {
            let label = match (n.kind, n.count) {
                (NodeKind::Object(_), _) | (_, None) => n.label.clone(),
                (_, Some(count)) => format!("{} ({count})", n.label),
            };
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
        let pill = environment_color(cx, self.config.environment, self.config.color);
        let view = cx.entity().downgrade();

        let title = TitleBar::new().child(
            h_flex()
                .w_full()
                .gap_3()
                .items_center()
                .child(
                    Button::new("toggle-sidebar")
                        .icon(IconName::PanelLeft)
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                )
                .child(
                    h_flex()
                        .id("connection-pill")
                        .test_support()
                        .aria_label(self.config.name.clone())
                        .gap_2()
                        .px_2p5()
                        .py_0p5()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(pill)
                        .text_sm()
                        .child(div().size_2().rounded_full().bg(pill))
                        .child(self.config.name.clone())
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(self.config.kind.display_name()),
                        ),
                ),
        );

        let sidebar = self.render_sidebar(&view, cx);
        let main = self.render_main(window, cx);

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
                    .when(self.sidebar_visible, |d| d.child(sidebar))
                    .child(main),
            )
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
                IconName::Eye
            } else {
                IconName::LayoutDashboard
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
                let paths = self.paths.clone();
                tree(&self.tree, move |ix, entry, selected, _, cx| {
                    let item = entry.item();
                    let id = item.id.clone();
                    let theme = cx.theme().clone();
                    let target = paths.get(&id).cloned();
                    let is_folder = entry.is_folder();
                    let view = view.clone();
                    let icon = if let Some(p) = &target {
                        // The icon depends on the kind; views get the eye.
                        let _ = p;
                        IconName::LayoutDashboard
                    } else if entry.is_expanded() {
                        IconName::FolderOpen
                    } else {
                        IconName::FolderClosed
                    };
                    ListItem::new(ix)
                        .selected(selected)
                        .pl(px(8.0 + 14.0 * entry.depth() as f32))
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_sm()
                                .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
                                .child(item.label.clone()),
                        )
                        .on_click(move |event, window, cx| {
                            if event.click_count() >= 2
                                && !is_folder
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
            .w(px(240.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar)
            .child(
                div().p_2().child(
                    Input::new(&self.filter)
                        .id("sidebar-filter")
                        .cleanable(true),
                ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_main(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let active_index = self.tabs.active_index().unwrap_or(0);
        let tabs: Vec<Tab> = self
            .tabs
            .tabs()
            .iter()
            .map(|t| {
                let title = match &t.kind {
                    TabKind::Table { path, .. } => table_title(path),
                    _ => "Query".to_owned(),
                };
                let (select_id, close_id) = (t.id.clone(), t.id.clone());
                Tab::new()
                    .label(title)
                    .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(&select_id, cx)))
                    .suffix(
                        Button::new(SharedString::from(format!("close-tab-{}", t.id)))
                            .icon(IconName::Close)
                            .ghost()
                            .xsmall()
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.close_tab(&close_id, cx)),
                            ),
                    )
            })
            .collect();

        let content: AnyElement = match self.tabs.active().and_then(|t| self.tab_views.get(&t.id)) {
            Some(tab) => tab.clone().into_any_element(),
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
            .when(!tabs.is_empty(), |d| {
                d.child(
                    TabBar::new("tabs")
                        .selected_index(active_index)
                        .children(tabs),
                )
            })
            .child(div().flex_1().min_h_0().child(content))
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
