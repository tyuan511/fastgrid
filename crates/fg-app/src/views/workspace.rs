//! A connection's window: title bar, sidebar with the object tree, tab strip and the active tab.
//!
//! The view owns the UI state (tabs, tree, filter). Anything that needs the database goes through
//! events ([`WorkspaceEvent`]) answered by [`crate::controller`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
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
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    CloseTab, NextTab, PrevTab, QuickOpen, RefreshSchema, ToggleFilters, ToggleSidebar,
    WORKSPACE_CONTEXT,
};
use super::grid::{TableTab, TableTabEvent};
use super::icons::{Hugeicon, hugeicon};
use crate::model::{
    GridQuery, NodeKind, QuickItem, TabSet, TreeNode, build_tree, compact_count, quick_items,
    quick_search, table_title,
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
            .text_size(px(13.))
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
    /// The theme button: system, light, dark.
    CycleTheme,
    /// Tabs or layout changed: worth saving.
    SessionChanged,
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
    /// Tree node id -> the table it stands for, so a click can be resolved.
    meta: Rc<HashMap<SharedString, NodeMeta>>,
    split: Entity<ResizableState>,
    sidebar_width: f32,
    collapsed: HashSet<SharedString>,
    tabs: TabSet,
    tab_views: HashMap<TabId, Entity<TableTab>>,
    sidebar_visible: bool,
    details_visible: bool,
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
        if let Some(tab) = self.tab_views.get(&id).cloned() {
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
        let view = cx.entity().downgrade();

        let active_table_name = self.tabs.active().map(|t| match &t.kind {
            TabKind::Table { path, .. } => table_title(path),
            _ => "Query".to_owned(),
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
            Some(table) => format!(
                "{} : {} : {} : {}",
                self.config.kind.display_name(),
                self.config.name,
                db_name,
                table
            ),
            None => format!(
                "{} : {} : {}",
                self.config.kind.display_name(),
                self.config.name,
                db_name
            ),
        };

        let is_dark = theme.background.l < 0.5;
        let pill_bg = if is_dark {
            hsla(140.0 / 360.0, 0.40, 0.16, 1.0)
        } else {
            hsla(130.0 / 360.0, 0.45, 0.92, 1.0)
        };
        let pill_border = if is_dark {
            hsla(140.0 / 360.0, 0.50, 0.28, 1.0)
        } else {
            hsla(130.0 / 360.0, 0.45, 0.78, 1.0)
        };
        let pill_text = if is_dark {
            hsla(130.0 / 360.0, 0.60, 0.85, 1.0)
        } else {
            hsla(140.0 / 360.0, 0.70, 0.20, 1.0)
        };

        let title = TitleBar::new().child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .justify_between()
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Button::new("toggle-sidebar")
                                .icon(hugeicon(Hugeicon::SidebarLeft))
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                        )
                        .child(
                            Button::new("refresh-schema")
                                .icon(hugeicon(Hugeicon::Refresh))
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(WorkspaceEvent::RefreshSchema)
                                })),
                        )
                        .child(
                            Button::new("sql-console")
                                .icon(hugeicon(Hugeicon::Terminal))
                                .ghost()
                                .xsmall(),
                        )
                        .child(
                            Button::new("history-btn")
                                .icon(hugeicon(Hugeicon::History))
                                .ghost()
                                .xsmall(),
                        )
                        .child(
                            Button::new("cycle-theme")
                                .icon(match crate::theme_control::chosen(cx) {
                                    fg_core::ThemeMode::Light => hugeicon(Hugeicon::Sun),
                                    fg_core::ThemeMode::Dark => hugeicon(Hugeicon::Moon),
                                    _ => hugeicon(Hugeicon::Sun),
                                })
                                .ghost()
                                .xsmall()
                                .tooltip(format!(
                                    "Theme: {}",
                                    crate::theme_control::label(crate::theme_control::chosen(cx))
                                ))
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(WorkspaceEvent::CycleTheme)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .id("connection-pill")
                        .test_support()
                        .aria_label(self.config.name.clone())
                        .gap_2()
                        .px_3()
                        .py_0p5()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(pill_border)
                        .bg(pill_bg)
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(pill_text)
                        .child(breadcrumb_text),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(div().size_2p5().rounded_full().bg(theme.warning))
                        .child(
                            Button::new("info-btn")
                                .icon(hugeicon(Hugeicon::Info))
                                .ghost()
                                .xsmall(),
                        )
                        .child(
                            Button::new("toggle-details")
                                .icon(hugeicon(Hugeicon::SidebarRight))
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_details(cx))),
                        ),
                ),
        );

        let sidebar = self.render_sidebar(&view, cx);
        let main = self.render_main(window, cx);
        let details = self.render_details_panel(cx);

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
                        return ListItem::new(ix).h(px(24.)).py_0().pl(indent).child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    hugeicon(chevron)
                                        .size_3()
                                        .text_color(theme.muted_foreground),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme.muted_foreground)
                                        .child(item.label.to_uppercase()),
                                ),
                        );
                    }

                    let icon = match info.map(|m| m.kind) {
                        Some(NodeKind::Object(ObjectKind::View)) => Hugeicon::View,
                        _ => Hugeicon::Table,
                    };
                    let icon_color = if selected {
                        gpui::white()
                    } else if icon == Hugeicon::View {
                        hsla(270.0 / 360.0, 0.80, 0.60, 1.0)
                    } else {
                        hsla(205.0 / 360.0, 0.90, 0.48, 1.0)
                    };
                    let target = info.and_then(|m| m.path.clone());
                    let estimate = info.and_then(|m| m.row_estimate).map(compact_count);
                    let matched = info.map(|m| m.matched.clone()).unwrap_or_default();
                    let view = view.clone();
                    ListItem::new(ix)
                        .h(px(24.))
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
                                        .child(hugeicon(icon).size_4().text_color(icon_color))
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
                h_flex()
                    .p_1()
                    .m_2()
                    .rounded_md()
                    .bg(theme.muted.opacity(0.6))
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .py_0p5()
                            .rounded_sm()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .bg(theme.background)
                            .text_color(theme.foreground)
                            .child("Items"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .py_0p5()
                            .rounded_sm()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .child("Queries"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .py_0p5()
                            .rounded_sm()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .child("History"),
                    ),
            )
            .child(
                div().px_2().pb_2().child(
                    Input::new(&self.filter)
                        .id("sidebar-filter")
                        .small()
                        .cleanable(true),
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
                            .xsmall(),
                    )
                    .child(
                        Button::new("sidebar-menu")
                            .icon(hugeicon(Hugeicon::ArrowDown))
                            .ghost()
                            .xsmall(),
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
                    _ => "Query".to_owned(),
                };
                let is_active = active.as_ref() == Some(&t.id);
                let icon = match &t.kind {
                    TabKind::Table { path, .. } if self.is_view(path) => Hugeicon::View,
                    TabKind::Table { .. } => Hugeicon::Table,
                    _ => Hugeicon::Terminal,
                };
                let icon_color = if is_active {
                    theme.tab_active_foreground
                } else if icon == Hugeicon::View {
                    hsla(270.0 / 360.0, 0.80, 0.60, 1.0)
                } else {
                    hsla(205.0 / 360.0, 0.90, 0.48, 1.0)
                };
                let (select_id, close_id, drag_id) = (t.id.clone(), t.id.clone(), t.id.clone());
                let drag_title = title.clone();
                let hover_bg = theme.tab_active;
                h_flex()
                    .id(SharedString::from(format!("tab-{}", t.id)))
                    .test_support()
                    .aria_label(title.clone())
                    .h_full()
                    .min_w(px(110.))
                    .max_w(px(220.))
                    .flex_shrink_0()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_r_1()
                    .border_color(theme.border)
                    .text_size(px(13.))
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(theme.tab_active)
                            .text_color(theme.tab_active_foreground)
                    })
                    .when(!is_active, |d| {
                        d.bg(theme.tab)
                            .text_color(theme.tab_foreground)
                            .hover(move |s| s.bg(hover_bg))
                    })
                    .child(hugeicon(icon).size_3p5().text_color(icon_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(title),
                    )
                    .child(
                        Button::new(SharedString::from(format!("close-tab-{}", t.id)))
                            .icon(hugeicon(Hugeicon::Close))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_tab(&close_id, cx);
                            })),
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
                .h(px(32.))
                .w_full()
                .flex_shrink_0()
                .overflow_hidden()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.tab_bar)
                .children(tabs)
                .into_any_element(),
        )
    }

    /// Whether `path` is a view (tab icon).
    fn is_view(&self, path: &ObjectPath) -> bool {
        self.objects
            .values()
            .flatten()
            .any(|o| &o.path == path && o.kind == ObjectKind::View)
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
            .children(strip)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    fn render_details_panel(&self, cx: &App) -> AnyElement {
        let theme = cx.theme().clone();
        v_flex()
            .id("details-panel")
            .test_support()
            .w(px(240.))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .p_1()
                    .m_1p5()
                    .rounded_md()
                    .bg(theme.muted.opacity(0.6))
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .py_0p5()
                            .rounded_sm()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .bg(theme.background)
                            .text_color(theme.foreground)
                            .child("Details"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_center()
                            .py_0p5()
                            .rounded_sm()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .child("Assistant"),
                    ),
            )
            .child(
                div().px_2().pb_2().child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("🔍 Search for field..."),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No row selected"),
            )
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
