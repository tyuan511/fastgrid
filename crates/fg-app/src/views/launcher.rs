//! The connection list shown at startup: search, open, create, edit, duplicate, delete.
//!
//! The view holds the data and the form state and reports what the user asked for
//! ([`LauncherEvent`]); saving, deleting and opening happen in [`crate::controller`]. That split
//! keeps the view testable with plain data, without a service or a database.

use std::collections::HashSet;

use fg_core::{
    ConnectionConfig, ConnectionGroup, ConnectionId, ConnectionParams, DbKind, Environment,
    GroupId, TagColor,
};
use fg_store::StoredConnection;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IndexPath, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{LAUNCHER_CONTEXT, LauncherNext, LauncherOpen, LauncherPrev, NewConnection};
use super::icons::{Hugeicon, hugeicon};
use super::style::environment_color;
use crate::model::{
    ConnectionDraft, ENVIRONMENTS, TAG_COLORS, color_from_label, duplicate, environment_from_label,
    rank,
};

/// One entry of the rendered launcher list.
pub enum ListItem<'a> {
    Row(&'a Row),
    GroupHeader(&'a ConnectionGroup, usize, bool),
    UngroupedHeader(usize),
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: ConnectionId,
    pub name: String,
    pub detail: String,
    pub kind: DbKind,
    pub environment: Environment,
    pub tag: Option<TagColor>,
    /// The group the connection belongs to, if any (mirrors `config.group_id` when readable).
    pub group_id: Option<fg_core::GroupId>,
    /// The saved settings; `None` for a record from a newer version that this build cannot read
    /// (it can be listed and deleted, not edited or opened).
    pub config: Option<ConnectionConfig>,
}

impl Row {
    pub fn readable(&self) -> bool {
        self.config.is_some()
    }
}

pub fn rows_from(stored: Vec<StoredConnection>) -> Vec<Row> {
    stored
        .into_iter()
        .map(|s| match s {
            StoredConnection::Ok(c) => Row {
                detail: describe(&c),
                id: c.id.clone(),
                name: c.name.clone(),
                kind: c.kind,
                environment: c.environment,
                tag: c.color,
                group_id: c.group_id.clone(),
                config: Some(c),
            },
            StoredConnection::Unreadable { id, reason } => Row {
                name: "Unsupported connection".into(),
                detail: format!("Saved by a newer version ({reason})"),
                id,
                kind: DbKind::Unknown,
                environment: Environment::Unknown,
                tag: None,
                group_id: None,
                config: None,
            },
        })
        .collect()
}

fn describe(c: &ConnectionConfig) -> String {
    match &c.params {
        ConnectionParams::File { path } => path.clone(),
        ConnectionParams::Network {
            host,
            port,
            user,
            database,
            ..
        } => match database {
            Some(db) => format!("{user}@{host}:{port}/{db}"),
            None => format!("{user}@{host}:{port}"),
        },
    }
}

/// What the user asked the launcher to do.
#[derive(Debug, Clone, PartialEq)]
pub enum LauncherEvent {
    Open(ConnectionId),
    /// Save a new or edited connection (also used for duplicates).
    Save(Box<ConnectionConfig>),
    Delete(ConnectionId),
    CycleTheme,
    /// The first-launch keychain-backup banner was closed; persist the dismissal.
    DismissBackupNudge,
    /// Save a new group, or rename an existing one (its id is kept).
    SaveGroup(ConnectionGroup),
    /// Delete a group; its connections move to the top level (done by the store).
    DeleteGroup(GroupId),
}

type Choice = Entity<SelectState<SearchableVec<SharedString>>>;

/// The create / edit form.
struct Editor {
    draft: ConnectionDraft,
    base: Option<ConnectionConfig>,
    name: Entity<InputState>,
    path: Entity<InputState>,
    environment: Choice,
    color: Choice,
    group: Choice,
    /// The group ids in the same order as the group choice's items (after "No group").
    group_ids: Vec<GroupId>,
    create_if_missing: bool,
    error: Option<SharedString>,
    selected_engine: DbKind,
}

/// The new-group / rename-group form. `id` is `None` for a new group.
struct GroupEditor {
    id: Option<GroupId>,
    name: Entity<InputState>,
    error: Option<SharedString>,
}

pub struct Launcher {
    rows: Vec<Row>,
    groups: Vec<ConnectionGroup>,
    collapsed: HashSet<GroupId>,
    selected: Option<ConnectionId>,
    message: Option<SharedString>,
    opening: Option<ConnectionId>,
    search: Entity<InputState>,
    editor: Option<Editor>,
    group_editor: Option<GroupEditor>,
    confirm_delete: Option<ConnectionId>,
    confirm_delete_group: Option<GroupId>,
    show_backup_nudge: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    dragging_window: bool,
}

impl EventEmitter<LauncherEvent> for Launcher {}

fn choice(
    items: Vec<SharedString>,
    selected: usize,
    window: &mut Window,
    cx: &mut Context<Launcher>,
) -> Choice {
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(items),
            Some(IndexPath::new(selected)),
            window,
            cx,
        )
    })
}

impl Launcher {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = if is_chinese_locale() {
            "查找连接…"
        } else {
            "Search connections…"
        };
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        // Enter is handled by the `LauncherOpen` shortcut, which the search box lets through; also
        // opening on its `PressEnter` event would open the connection twice.
        let on_search = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.keep_selection_visible(cx);
            }
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            rows: Vec::new(),
            groups: Vec::new(),
            collapsed: HashSet::new(),
            selected: None,
            message: None,
            opening: None,
            search,
            editor: None,
            group_editor: None,
            confirm_delete: None,
            confirm_delete_group: None,
            show_backup_nudge: false,
            focus,
            _subscriptions: vec![on_search],
            dragging_window: false,
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&ConnectionId> {
        self.selected.as_ref()
    }

    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The rows matching the search box, best match first (all of them when it is empty).
    pub fn visible_rows(&self, cx: &App) -> Vec<&Row> {
        let query = self.search.read(cx).value().to_string();
        if query.trim().is_empty() {
            return self.rows.iter().collect();
        }
        let haystack: Vec<String> = self
            .rows
            .iter()
            .map(|r| format!("{} {}", r.name, r.detail))
            .collect();
        rank(query.trim(), &haystack)
            .into_iter()
            .map(|(i, _)| &self.rows[i])
            .collect()
    }

    /// One line of the rendered list: a connection row, or a group section header.
    /// Headers carry their row count and collapsed state so the render step stays dumb.
    pub fn list_items(&self, cx: &App) -> Vec<ListItem<'_>> {
        let visible = self.visible_rows(cx);
        let searching = !self.search.read(cx).value().trim().is_empty();
        if searching || self.groups.is_empty() {
            return visible.into_iter().map(ListItem::Row).collect();
        }
        let mut items = Vec::new();
        for group in &self.groups {
            let rows: Vec<&Row> = visible
                .iter()
                .copied()
                .filter(|r| r.group_id.as_ref() == Some(&group.id))
                .collect();
            let collapsed = self.collapsed.contains(&group.id);
            items.push(ListItem::GroupHeader(group, rows.len(), collapsed));
            if !collapsed {
                items.extend(rows.into_iter().map(ListItem::Row));
            }
        }
        let ungrouped: Vec<&Row> = visible
            .iter()
            .copied()
            .filter(|r| r.group_id.is_none())
            .collect();
        if !ungrouped.is_empty() {
            items.push(ListItem::UngroupedHeader(ungrouped.len()));
            items.extend(ungrouped.into_iter().map(ListItem::Row));
        }
        items
    }

    /// Replace the list. The selection survives if the row still exists, else moves to the first.
    pub fn set_rows(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        let keep = self
            .selected
            .as_ref()
            .is_some_and(|id| rows.iter().any(|r| &r.id == id));
        if !keep {
            self.selected = rows.first().map(|r| r.id.clone());
        }
        self.rows = rows;
        cx.notify();
    }

    pub fn select(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn set_message(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        self.message = message.map(SharedString::from);
        cx.notify();
    }

    pub fn set_opening(&mut self, id: Option<ConnectionId>, cx: &mut Context<Self>) {
        self.opening = id;
        cx.notify();
    }

    /// Show or hide the first-launch keychain-backup banner.
    pub fn set_backup_nudge(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_backup_nudge = show;
        cx.notify();
    }

    pub fn backup_nudge_shown(&self) -> bool {
        self.show_backup_nudge
    }

    // ---- groups -------------------------------------------------------------------------------

    /// Replace the group list. Collapsed flags for groups that no longer exist are dropped.
    pub fn set_groups(&mut self, groups: Vec<ConnectionGroup>, cx: &mut Context<Self>) {
        let ids: HashSet<GroupId> = groups.iter().map(|g| g.id.clone()).collect();
        self.collapsed.retain(|id| ids.contains(id));
        self.groups = groups;
        cx.notify();
    }

    pub fn toggle_group(&mut self, id: GroupId, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        cx.notify();
    }

    /// Open the new-group form.
    pub fn new_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Group name"));
        self.group_editor = Some(GroupEditor {
            id: None,
            name,
            error: None,
        });
        self.confirm_delete_group = None;
        cx.notify();
    }

    /// Open the rename form for a group, prefilled with its current name.
    pub fn rename_group(&mut self, id: GroupId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.groups.iter().find(|g| g.id == id) else {
            return;
        };
        let current = group.name.clone();
        let name = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Group name");
            s.set_value(current, window, cx);
            s
        });
        self.group_editor = Some(GroupEditor {
            id: Some(id),
            name,
            error: None,
        });
        self.confirm_delete_group = None;
        cx.notify();
    }

    fn cancel_group_editor(&mut self, cx: &mut Context<Self>) {
        self.group_editor = None;
        cx.notify();
    }

    /// Save the group form: a blank name is an inline error, not an event.
    fn save_group_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.group_editor.as_mut() else {
            return;
        };
        let name = editor.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            editor.error = Some("Give the group a name.".into());
            cx.notify();
            return;
        }
        let id = editor.id.clone();
        self.group_editor = None;
        let group = match id {
            Some(id) => {
                let mut g = self
                    .groups
                    .iter()
                    .find(|g| g.id == id)
                    .cloned()
                    .unwrap_or_else(|| ConnectionGroup::new(name.clone()));
                g.id = id;
                g.name = name;
                g
            }
            None => ConnectionGroup::new(name),
        };
        cx.emit(LauncherEvent::SaveGroup(group));
        cx.notify();
    }

    fn ask_delete_group(&mut self, id: GroupId, cx: &mut Context<Self>) {
        if self.confirm_delete_group.as_ref() == Some(&id) {
            self.confirm_delete_group = None;
            cx.emit(LauncherEvent::DeleteGroup(id));
        } else {
            self.confirm_delete_group = Some(id);
        }
        cx.notify();
    }

    fn selected_row(&self) -> Option<&Row> {
        let id = self.selected.as_ref()?;
        self.rows.iter().find(|r| &r.id == id)
    }

    /// After the search changes: if the selected row is filtered out, select the best match.
    fn keep_selection_visible(&mut self, cx: &mut Context<Self>) {
        let visible: Vec<ConnectionId> =
            self.visible_rows(cx).iter().map(|r| r.id.clone()).collect();
        if !self.selected.as_ref().is_some_and(|s| visible.contains(s)) {
            self.selected = visible.first().cloned();
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let visible: Vec<ConnectionId> =
            self.visible_rows(cx).iter().map(|r| r.id.clone()).collect();
        if visible.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|s| visible.iter().position(|v| v == s));
        let next = match current {
            Some(i) => (i as isize + delta).clamp(0, visible.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(visible[next].clone());
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        if self.editor.is_some() || self.opening.is_some() {
            return;
        }
        if let Some(row) = self.selected_row()
            && row.readable()
        {
            cx.emit(LauncherEvent::Open(row.id.clone()));
        }
    }

    // ---- editor -------------------------------------------------------------------------------

    /// Open the form for a new connection.
    pub fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_editor(ConnectionDraft::blank(), None, window, cx);
    }

    fn edit_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(config) = self.selected_row().and_then(|r| r.config.clone()) {
            self.open_editor(
                ConnectionDraft::from_config(&config),
                Some(config),
                window,
                cx,
            );
        }
    }

    fn open_editor(
        &mut self,
        draft: ConnectionDraft,
        base: Option<ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Name");
            s.set_value(draft.name.clone(), window, cx);
            s
        });
        let path = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("/path/to/database.db");
            s.set_value(draft.path.clone(), window, cx);
            s
        });
        let envs: Vec<SharedString> = ENVIRONMENTS
            .iter()
            .map(|(_, l)| SharedString::from(*l))
            .collect();
        let env_ix = ENVIRONMENTS
            .iter()
            .position(|(e, _)| *e == draft.environment)
            .unwrap_or(0);
        let colors: Vec<SharedString> = TAG_COLORS
            .iter()
            .map(|(_, l)| SharedString::from(*l))
            .collect();
        let color_ix = TAG_COLORS
            .iter()
            .position(|(c, _)| *c == draft.color)
            .unwrap_or(0);
        let environment = choice(envs, env_ix, window, cx);
        let color = choice(colors, color_ix, window, cx);
        let no_group: SharedString = if is_chinese_locale() {
            "不分组".into()
        } else {
            "No group".into()
        };
        let mut group_items = vec![no_group];
        let mut group_ids = Vec::new();
        for g in &self.groups {
            group_items.push(g.name.clone().into());
            group_ids.push(g.id.clone());
        }
        let group_ix = draft
            .group_id
            .as_ref()
            .and_then(|id| group_ids.iter().position(|g| g == id))
            .map(|i| i + 1)
            .unwrap_or(0);
        let group = choice(group_items, group_ix, window, cx);
        let create_if_missing = draft.create_if_missing;
        let selected_engine = base.as_ref().map_or(DbKind::Sqlite, |c| c.kind);
        self.editor = Some(Editor {
            draft,
            base,
            name,
            path,
            environment,
            color,
            group,
            group_ids,
            create_if_missing,
            error: None,
            selected_engine,
        });
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// Ask the system for a database file and put the choice in the form.
    fn browse_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a SQLite database".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = picked.await
                && !paths.is_empty()
            {
                let path = paths.remove(0).to_string_lossy().into_owned();
                let _ = this.update_in(cx, |v, window, cx| v.set_editor_path(path, window, cx));
            }
        })
        .detach();
    }

    /// Fill the path box (after the file picker answered). Also suggests a name if none is typed.
    pub fn set_editor_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else { return };
        let (name_box, path_box) = (editor.name.clone(), editor.path.clone());
        if name_box.read(cx).value().trim().is_empty() {
            let suggestion = ConnectionDraft::default_name_for(&path);
            name_box.update(cx, |s, cx| s.set_value(suggestion, window, cx));
        }
        path_box.update(cx, |s, cx| s.set_value(path, window, cx));
        cx.notify();
    }

    /// Read the form into a draft.
    fn read_draft(&self, cx: &App) -> Option<ConnectionDraft> {
        let e = self.editor.as_ref()?;
        let env_label = e
            .environment
            .read(cx)
            .selected_value()
            .map(ToString::to_string);
        let color_name = e.color.read(cx).selected_value().map(ToString::to_string);
        // Index 0 is "No group"; any other index maps back through the ids captured when the
        // form opened, so two groups sharing a name still resolve to the right one.
        let group_id = match e.group.read(cx).selected_index(cx) {
            Some(p) if p.row > 0 => e.group_ids.get(p.row - 1).cloned(),
            Some(_) => None,
            None => e.draft.group_id.clone(),
        };
        Some(ConnectionDraft {
            id: e.draft.id.clone(),
            name: e.name.read(cx).value().to_string(),
            environment: env_label
                .as_deref()
                .map_or(e.draft.environment, environment_from_label),
            color: color_name.as_deref().and_then(color_from_label),
            path: e.path.read(cx).value().to_string(),
            create_if_missing: e.create_if_missing,
            group_id,
        })
    }

    pub fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.read_draft(cx) else {
            return;
        };
        let base = self.editor.as_ref().and_then(|e| e.base.clone());
        match draft.to_config(base.as_ref()) {
            Ok(config) => {
                self.selected = Some(config.id.clone());
                self.close_editor(window, cx);
                cx.emit(LauncherEvent::Save(Box::new(config)));
            }
            Err(message) => {
                if let Some(e) = self.editor.as_mut() {
                    e.error = Some(message.into());
                }
                cx.notify();
            }
        }
    }

    // ---- duplicate and delete -----------------------------------------------------------------

    fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(config) = self.selected_row().and_then(|r| r.config.clone()) {
            cx.emit(LauncherEvent::Save(Box::new(duplicate(&config))));
        }
    }

    fn ask_delete(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone() {
            self.confirm_delete = Some(id);
            cx.notify();
        }
    }

    fn confirm_delete_now(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.confirm_delete.take() {
            cx.emit(LauncherEvent::Delete(id));
        }
        cx.notify();
    }
}

fn fastgrid_logo() -> impl IntoElement {
    img("fastgrid/logo.png").size(px(64.))
}

const ENGINES: &[(DbKind, &str, &str, f32, f32, f32)] =
    &[(DbKind::Sqlite, "SQLite", "Sl", 262.0 / 360.0, 0.83, 0.58)];

pub fn is_chinese_locale() -> bool {
    super::style::is_zh_locale()
}

fn tableplus_action_button(
    id: &'static str,
    icon: gpui_kit::component::Icon,
    label: impl Into<SharedString>,
    cx: &App,
) -> Button {
    let theme = cx.theme();
    let is_dark = theme.is_dark();
    let custom = if is_dark {
        ButtonCustomVariant::new(cx)
            .color(hsla(0.0, 0.0, 1.0, 0.06))
            .hover(hsla(0.0, 0.0, 1.0, 0.12))
            .active(hsla(0.0, 0.0, 1.0, 0.18))
            .foreground(theme.foreground)
    } else {
        ButtonCustomVariant::new(cx)
            .color(hsla(0.0, 0.0, 1.0, 0.65))
            .hover(hsla(0.0, 0.0, 1.0, 0.98))
            .active(hsla(0.0, 0.0, 1.0, 0.82))
            .foreground(hsla(220.0 / 360.0, 0.15, 0.18, 1.0))
    };

    Button::new(id)
        .custom(custom)
        .outline()
        .rounded(px(21.))
        .h(px(42.))
        .w_full()
        .icon(icon)
        .label(label)
}

fn tableplus_circle_button(
    id: &'static str,
    icon: gpui_kit::component::Icon,
    tooltip: impl Into<SharedString>,
    cx: &App,
) -> Button {
    let theme = cx.theme();
    let is_dark = theme.is_dark();
    let custom = if is_dark {
        ButtonCustomVariant::new(cx)
            .color(hsla(0.0, 0.0, 1.0, 0.08))
            .hover(hsla(0.0, 0.0, 1.0, 0.16))
            .active(hsla(0.0, 0.0, 1.0, 0.22))
            .foreground(theme.foreground)
    } else {
        ButtonCustomVariant::new(cx)
            .color(hsla(0.0, 0.0, 1.0, 0.95))
            .hover(hsla(0.0, 0.0, 1.0, 1.0))
            .active(hsla(220.0 / 360.0, 0.10, 0.92, 1.0))
            .foreground(hsla(220.0 / 360.0, 0.15, 0.20, 1.0))
    };

    Button::new(id)
        .custom(custom)
        .outline()
        .rounded(px(16.))
        .size(px(32.))
        .icon(icon)
        .tooltip(tooltip)
}

fn with_window_drag<E: InteractiveElement>(element: E, cx: &Context<Launcher>) -> E {
    element
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down_out(cx.listener(|this, _, _, _| {
            this.dragging_window = false;
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, window, _| {
                if event.click_count >= 2 {
                    window.titlebar_double_click();
                } else {
                    this.dragging_window = true;
                }
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, _| {
                this.dragging_window = false;
            }),
        )
        .on_mouse_move(cx.listener(|this, _, window, _| {
            if this.dragging_window {
                this.dragging_window = false;
                window.start_window_move();
            }
        }))
}

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let items = self.list_items(cx);
        let is_zh = is_chinese_locale();
        let entity = cx.entity().clone();
        let rows: Vec<AnyElement> = items
            .iter()
            .map(|item| match item {
                ListItem::Row(row) => {
                    let selected = self.selected.as_ref() == Some(&row.id);
                    let opening = self.opening.as_ref() == Some(&row.id);
                    let dot = environment_color(cx, row.environment, row.tag);
                    let (select_id, open_id) = (row.id.clone(), row.id.clone());
                    let readable = row.readable();

                    let (badge_text, badge_color) = match row.kind {
                        DbKind::Postgres => ("Pg", hsla(205.0 / 360.0, 0.90, 0.48, 1.0)),
                        DbKind::MySql => ("Ms", hsla(28.0 / 360.0, 0.95, 0.52, 1.0)),
                        DbKind::Sqlite => ("Sl", hsla(262.0 / 360.0, 0.83, 0.58, 1.0)),
                        _ => ("Db", theme.muted_foreground),
                    };

                    let env_label = match row.environment {
                        Environment::Development => "local",
                        Environment::Staging => "staging",
                        Environment::Production => "production",
                        _ => "env",
                    };
                    let (env_bg, env_fg) = match row.environment {
                        Environment::Development => (
                            hsla(142.0 / 360.0, 0.70, 0.90, 0.9),
                            hsla(142.0 / 360.0, 0.75, 0.30, 1.0),
                        ),
                        Environment::Staging => (
                            hsla(38.0 / 360.0, 0.90, 0.88, 0.9),
                            hsla(38.0 / 360.0, 0.90, 0.32, 1.0),
                        ),
                        Environment::Production => (
                            hsla(0.0 / 360.0, 0.80, 0.90, 0.9),
                            hsla(0.0 / 360.0, 0.80, 0.38, 1.0),
                        ),
                        _ => (theme.muted.opacity(0.8), theme.muted_foreground),
                    };

                    let launcher = entity.clone();
                    let menu_row_id = row.id.clone();
                    let can_open_row = readable && !opening && self.editor.is_none();

                    let menu_builder = {
                        let launcher = launcher.clone();
                        let menu_row_id = menu_row_id.clone();
                        move |menu: PopupMenu, _: &mut Window, menu_cx: &mut Context<PopupMenu>| {
                            launcher.update(menu_cx, |this, cx| {
                                this.select(menu_row_id.clone(), cx);
                            });

                            let l_open = launcher.clone();
                            let l_edit = launcher.clone();
                            let l_dup = launcher.clone();
                            let l_del = launcher.clone();

                            menu.item(
                                PopupMenuItem::new(if is_zh { "连接" } else { "Connect" })
                                    .icon(hugeicon(Hugeicon::Plug))
                                    .disabled(!can_open_row)
                                    .on_click(move |_, _, cx| {
                                        l_open.update(cx, |this, cx| this.open_selected(cx));
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(if is_zh { "编辑…" } else { "Edit…" })
                                    .icon(hugeicon(Hugeicon::Edit))
                                    .disabled(!readable)
                                    .on_click(move |_, window, cx| {
                                        l_edit
                                            .update(cx, |this, cx| this.edit_selected(window, cx));
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(if is_zh { "复制" } else { "Duplicate" })
                                    .icon(hugeicon(Hugeicon::Copy))
                                    .disabled(!readable)
                                    .on_click(move |_, _, cx| {
                                        l_dup.update(cx, |this, cx| this.duplicate_selected(cx));
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(if is_zh { "删除" } else { "Delete" })
                                    .icon(hugeicon(Hugeicon::Trash))
                                    .on_click(move |_, _, cx| {
                                        l_del.update(cx, |this, cx| this.ask_delete(cx));
                                    }),
                            )
                        }
                    };

                    h_flex()
                        .id(SharedString::from(format!("connection-{}", row.id)))
                        .test_support()
                        .role(Role::ListItem)
                        .aria_label(row.name.clone())
                        .aria_selected(selected)
                        .gap_3()
                        .px_3()
                        .py_2()
                        .items_center()
                        .rounded_lg()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(theme.list_active))
                        .hover(|d| d.bg(theme.list_hover))
                        .child(
                            div()
                                .size_9()
                                .rounded_full()
                                .bg(badge_color)
                                .text_color(gpui::white())
                                .flex()
                                .items_center()
                                .justify_center()
                                .font_weight(FontWeight::BOLD)
                                .text_sm()
                                .child(badge_text),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(row.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py(px(0.5))
                                                .rounded_full()
                                                .bg(if theme.is_dark() {
                                                    env_fg.opacity(0.2)
                                                } else {
                                                    env_bg
                                                })
                                                .text_color(if theme.is_dark() {
                                                    env_fg.opacity(0.9)
                                                } else {
                                                    env_fg
                                                })
                                                .text_size(px(11.))
                                                .font_weight(FontWeight::MEDIUM)
                                                .child(env_label),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(row.detail.clone()),
                                ),
                        )
                        .child(h_flex().gap_2().items_center().child(if opening {
                            div()
                                .text_xs()
                                .text_color(theme.info)
                                .child(if is_zh {
                                    "连接中…"
                                } else {
                                    "Connecting…"
                                })
                                .into_any_element()
                        } else if row.tag.is_some() {
                            div().size_2p5().rounded_full().bg(dot).into_any_element()
                        } else {
                            div().into_any_element()
                        }))
                        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                            this.select(select_id.clone(), cx);
                            if event.click_count() >= 2 && readable {
                                this.open_selected(cx);
                            }
                            let _ = &open_id;
                        }))
                        .context_menu(menu_builder)
                        .into_any_element()
                }
                ListItem::GroupHeader(group, count, collapsed) => {
                    let toggle_id = group.id.clone();
                    let rename_id = group.id.clone();
                    let delete_id = group.id.clone();
                    let arming = self.confirm_delete_group.as_ref() == Some(&group.id);
                    let toggle_label = format!(
                        "{} {} ({})",
                        if *collapsed { "▸" } else { "▾" },
                        group.name,
                        count
                    );
                    h_flex()
                        .id(SharedString::from(format!("group-header-{}", group.id)))
                        .test_support()
                        .items_center()
                        .gap_2()
                        .p_1()
                        .child(
                            Button::new(SharedString::from(format!("group-toggle-{}", group.id)))
                                .label(toggle_label)
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_group(toggle_id.clone(), cx);
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new(SharedString::from(format!("group-rename-{}", group.id)))
                                .label(if is_zh { "重命名" } else { "Rename" })
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.rename_group(rename_id.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("group-delete-{}", group.id)))
                                .label(if arming {
                                    if is_zh { "确认删除？" } else { "Delete?" }
                                } else if is_zh {
                                    "删除"
                                } else {
                                    "Delete"
                                })
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask_delete_group(delete_id.clone(), cx);
                                })),
                        )
                        .into_any_element()
                }
                ListItem::UngroupedHeader(count) => div()
                    .id("group-header-ungrouped")
                    .test_support()
                    .p_1()
                    .text_color(theme.muted_foreground)
                    .child(if is_zh {
                        format!("未分组 ({count})")
                    } else {
                        format!("Ungrouped ({count})")
                    })
                    .into_any_element(),
            })
            .collect();

        let has_selection = self.selected_row().is_some();
        let editable = self.selected_row().is_some_and(Row::readable);
        let can_open = editable && self.opening.is_none() && self.editor.is_none();
        let confirm = self.confirm_delete.clone().and_then(|id| {
            self.rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.name.clone())
        });
        let editor = self.render_editor(cx);

        h_flex()
            .id("launcher")
            .size_full()
            .relative()
            .key_context(LAUNCHER_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &LauncherNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &LauncherPrev, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &LauncherOpen, _, cx| this.open_selected(cx)))
            .on_action(
                cx.listener(|this, _: &NewConnection, window, cx| this.new_connection(window, cx)),
            )
            .bg(theme.background)
            .text_color(theme.foreground)
            // Left Sidebar: Brand & Actions (entire sidebar area allows window dragging)
            .child(
                with_window_drag(
                    v_flex()
                        .id("launcher-sidebar")
                        .test_support()
                        .w(px(220.))
                        .h_full()
                        .bg(theme.sidebar)
                        .border_r_1()
                        .border_color(theme.border)
                        .p_3()
                        .justify_between(),
                    cx,
                )
                .child(
                    v_flex()
                        .gap_2p5()
                        .items_center()
                        .when(cfg!(target_os = "macos"), |d| d.pt(px(48.)))
                        .when(!cfg!(target_os = "macos"), |d| d.pt(px(28.)))
                        .child(fastgrid_logo())
                        .child(
                            v_flex()
                                .items_center()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_xl()
                                        .font_weight(FontWeight::BOLD)
                                        .child("FastGrid"),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    if is_zh {
                                        "版本 0.0.1"
                                    } else {
                                        "Version 0.0.1"
                                    },
                                )),
                        ),
                )
                .child(
                    v_flex()
                        .gap_2p5()
                        .w_full()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(tableplus_action_button(
                            "backup-db",
                            hugeicon(Hugeicon::Backup),
                            if is_zh {
                                "备份数据库"
                            } else {
                                "Backup Database"
                            },
                            cx,
                        ))
                        .child(tableplus_action_button(
                            "restore-db",
                            hugeicon(Hugeicon::Restore),
                            if is_zh {
                                "恢复数据库"
                            } else {
                                "Restore Database"
                            },
                            cx,
                        ))
                        .child(
                            tableplus_action_button(
                                "new-connection",
                                hugeicon(Hugeicon::Plug),
                                if is_zh {
                                    "新建连接"
                                } else {
                                    "New Connection"
                                },
                                cx,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.new_connection(window, cx);
                                },
                            )),
                        ),
                ),
            )
            // Right Main Content: Connections
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .p_4()
                    .gap_3()
                    .child(
                        with_window_drag(
                            h_flex()
                                .id("launcher-top-bar")
                                .test_support()
                                .items_center()
                                .justify_between()
                                .gap_3(),
                            cx,
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(
                                    tableplus_circle_button(
                                        "new-connection-top",
                                        hugeicon(Hugeicon::Plus),
                                        if is_zh {
                                            "新建连接"
                                        } else {
                                            "New Connection"
                                        },
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.new_connection(window, cx);
                                        },
                                    )),
                                )
                                .child(
                                    tableplus_circle_button(
                                        "new-group",
                                        hugeicon(Hugeicon::FolderPlus),
                                        if is_zh { "新建分组" } else { "New Group" },
                                        cx,
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_group(window, cx);
                                    })),
                                ),
                        )
                        .child(
                            div()
                                .w(px(260.))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(
                                    Input::new(&self.search)
                                        .id("launcher-search")
                                        .small()
                                        .cleanable(true),
                                ),
                        ),
                    )
                    .when(self.show_backup_nudge, |d| {
                        d.child(
                            h_flex()
                                .id("backup-nudge")
                                .test_support()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .p_2()
                                // The launcher column sizes to its content, so an unwrapped
                                // notice would push its button past the window edge; cap the
                                // strip and let the text wrap inside it.
                                .max_w(px(560.))
                                .rounded(px(6.))
                                .bg(theme.warning.opacity(0.12))
                                .child(
                                    div()
                                        .flex_1()
                                        .max_w(px(440.))
                                        .text_size(px(13.))
                                        .child(if is_zh {
                                            "你的连接和密码用系统钥匙串里的设备密钥加密。钥匙串一旦丢失或重置（比如重装系统），这些数据就再也打不开了——请把系统钥匙串纳入备份，或记下连接信息以备恢复。"
                                        } else {
                                            "Your connections and passwords are encrypted with a device key in the system keychain. If that keychain is ever lost or reset (for example by reinstalling the system), this data cannot be opened again — keep your system keychain backed up, or note down your connection details for recovery."
                                        }),
                                )
                                .child(
                                    Button::new("backup-nudge-dismiss")
                                        .label(if is_zh { "知道了" } else { "Got it" })
                                        .ghost()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            // Hide at once; the controller persists the
                                            // dismissal from the emitted event.
                                            this.set_backup_nudge(false, cx);
                                            cx.emit(LauncherEvent::DismissBackupNudge);
                                        })),
                                ),
                        )
                    })
                    .when_some(self.render_group_editor(cx), |d, editor| d.child(editor))
                    .child(
                        // Headless test-support action triggers.
                        // Visual operations are right-click only (matching TablePlus).
                        // This element retains test-harness button accessibility.
                        h_flex()
                            .id("test-actions")
                            .test_support()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .opacity(0.005)
                            .child(
                                Button::new("open-selected")
                                    .label(if is_zh { "打开" } else { "Open" })
                                    .disabled(!can_open)
                                    .on_click(cx.listener(|this, _, _, cx| this.open_selected(cx))),
                            )
                            .child(
                                Button::new("edit-selected")
                                    .label(if is_zh { "编辑…" } else { "Edit…" })
                                    .disabled(!editable)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.edit_selected(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("duplicate-selected")
                                    .label(if is_zh { "复制" } else { "Duplicate" })
                                    .disabled(!editable)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.duplicate_selected(cx);
                                    })),
                            )
                            .child(
                                Button::new("delete-selected")
                                    .label(if is_zh { "删除" } else { "Delete" })
                                    .disabled(!has_selection)
                                    .on_click(cx.listener(|this, _, _, cx| this.ask_delete(cx))),
                            )
                            .child(
                                Button::new("cycle-theme")
                                    .icon(match crate::theme_control::chosen(cx) {
                                        fg_core::ThemeMode::Light => hugeicon(Hugeicon::Sun),
                                        fg_core::ThemeMode::Dark => hugeicon(Hugeicon::Moon),
                                        _ => hugeicon(Hugeicon::Sun),
                                    })
                                    .label(format!(
                                        "Theme: {}",
                                        crate::theme_control::label(crate::theme_control::chosen(
                                            cx
                                        ))
                                    ))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(LauncherEvent::CycleTheme);
                                    })),
                            ),
                    )
                    .when_some(confirm, |d, name| {
                        d.child(
                            h_flex()
                                .id("confirm-delete")
                                .test_support()
                                .w_full()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(theme.muted)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .child(format!("Delete “{name}”?")),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .flex_shrink_0()
                                        .child(
                                            Button::new("confirm-delete-yes")
                                                .label("Delete")
                                                .small()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.confirm_delete_now(cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("confirm-delete-no")
                                                .label("Cancel")
                                                .small()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.confirm_delete = None;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        )
                    })
                    .when_some(self.message.clone(), |d, m| {
                        d.child(
                            div()
                                .id("launcher-message")
                                .test_support()
                                .aria_label(m.clone())
                                .text_sm()
                                .text_color(theme.danger)
                                .child(m),
                        )
                    })
                    .child(if rows.is_empty() {
                        div()
                            .id("empty-state")
                            .test_support()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.muted_foreground)
                            .child(if is_zh {
                                if self.rows.is_empty() {
                                    "暂无连接。点击左侧“新建连接”添加数据库。"
                                } else {
                                    "未找到匹配的连接。"
                                }
                            } else {
                                if self.rows.is_empty() {
                                    "No connections yet. Click New… to add a SQLite file."
                                } else {
                                    "No connection matches the search."
                                }
                            })
                            .into_any_element()
                    } else {
                        v_flex()
                            .id("connection-list")
                            .test_support()
                            .role(Role::List)
                            .flex_1()
                            .gap_1p5()
                            .children(rows)
                            .into_any_element()
                    }),
            )
            .children(editor)
    }
}

impl Launcher {
    fn render_group_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.group_editor.as_ref()?;
        let theme = cx.theme().clone();
        let is_zh = is_chinese_locale();
        let title = if e.id.is_some() {
            if is_zh {
                "重命名分组"
            } else {
                "Rename group"
            }
        } else if is_zh {
            "新建分组"
        } else {
            "New group"
        };
        Some(
            v_flex()
                .id("group-editor")
                .test_support()
                .gap_2()
                .p_2()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    Input::new(&e.name)
                        .id("group-editor-name")
                        .into_any_element(),
                )
                .when_some(e.error.clone(), |d, message| {
                    d.child(
                        div()
                            .id("group-editor-error")
                            .test_support()
                            .aria_label(message.clone())
                            .text_color(theme.danger)
                            .child(message),
                    )
                })
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("group-editor-cancel")
                                .label(if is_zh { "取消" } else { "Cancel" })
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_group_editor(cx);
                                })),
                        )
                        .child(
                            Button::new("group-editor-save")
                                .label(if is_zh { "保存" } else { "Save" })
                                .primary()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.save_group_editor(cx);
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.editor.as_ref()?;
        let theme = cx.theme().clone();
        let is_zh = is_chinese_locale();
        let title = if e.draft.id.is_some() {
            if is_zh {
                "编辑连接"
            } else {
                "Edit connection"
            }
        } else {
            if is_zh {
                "新建连接"
            } else {
                "New connection"
            }
        };
        let field = |label: SharedString, body: AnyElement| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(body)
        };
        let error = e.error.clone();
        let active_engine = e.selected_engine;

        let engine_cards: Vec<AnyElement> = ENGINES
            .iter()
            .map(|(kind, name, abbr, h, s, l)| {
                let is_chosen = *kind == active_engine;
                let bg_color = hsla(*h, *s, *l, 1.0);
                let k = *kind;
                v_flex()
                    .id(SharedString::from(format!(
                        "engine-{}",
                        name.to_lowercase().replace(' ', "-")
                    )))
                    .items_center()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .cursor_pointer()
                    .border_2()
                    .border_color(if is_chosen {
                        theme.primary
                    } else {
                        theme.border.opacity(0.4)
                    })
                    .bg(if is_chosen {
                        theme.primary.opacity(0.08)
                    } else {
                        theme.secondary
                    })
                    .hover(|d| d.bg(theme.muted))
                    .child(
                        div()
                            .size_8()
                            .rounded_full()
                            .bg(bg_color)
                            .text_color(gpui::white())
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_weight(FontWeight::BOLD)
                            .text_xs()
                            .child(*abbr),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(if is_chosen {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(theme.foreground)
                            .child(*name),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ed) = this.editor.as_mut() {
                            ed.selected_engine = k;
                            cx.notify();
                        }
                    }))
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .id("connection-editor")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.background.opacity(0.85))
                .child(
                    v_flex()
                        .w(px(520.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_lg()
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    if is_zh {
                                        "选择数据库引擎"
                                    } else {
                                        "Choose database engine"
                                    },
                                ))
                                .child(h_flex().flex_wrap().gap_2().children(engine_cards)),
                        )
                        .child(field(
                            if is_zh {
                                "名称".into()
                            } else {
                                "Name".into()
                            },
                            Input::new(&e.name).id("editor-name").into_any_element(),
                        ))
                        .child(field(
                            if is_zh {
                                "数据库文件".into()
                            } else {
                                "Database file".into()
                            },
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&e.path).id("editor-path")))
                                .child(
                                    Button::new("editor-browse")
                                        .label(if is_zh { "浏览…" } else { "Browse…" })
                                        .small()
                                        .rounded(px(14.))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.browse_file(window, cx)
                                        })),
                                )
                                .into_any_element(),
                        ))
                        .child(
                            h_flex()
                                .gap_3()
                                .child(
                                    div().flex_1().child(field(
                                        if is_zh {
                                            "环境".into()
                                        } else {
                                            "Environment".into()
                                        },
                                        Select::new(&e.environment)
                                            .id("editor-environment")
                                            .into_any_element(),
                                    )),
                                )
                                .child(div().flex_1().child(field(
                                    if is_zh {
                                        "颜色标签".into()
                                    } else {
                                        "Colour tag".into()
                                    },
                                    Select::new(&e.color).id("editor-color").into_any_element(),
                                ))),
                        )
                        .child(field(
                            if is_zh {
                                "分组".into()
                            } else {
                                "Group".into()
                            },
                            Select::new(&e.group).id("editor-group").into_any_element(),
                        ))
                        .child(
                            Checkbox::new("editor-create")
                                .label(if is_zh {
                                    "如果文件不存在则自动创建"
                                } else {
                                    "Create the file if it does not exist"
                                })
                                .checked(e.create_if_missing)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(e) = this.editor.as_mut() {
                                        e.create_if_missing = !e.create_if_missing;
                                    }
                                    cx.notify();
                                })),
                        )
                        .when_some(error, |d, message| {
                            d.child(
                                div()
                                    .id("editor-error")
                                    .test_support()
                                    .aria_label(message.clone())
                                    .text_sm()
                                    .text_color(theme.danger)
                                    .child(message),
                            )
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("editor-cancel")
                                        .small()
                                        .label(if is_zh { "取消" } else { "Cancel" })
                                        .rounded(px(18.))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_editor(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("editor-save")
                                        .small()
                                        .label(if is_zh { "保存" } else { "Save" })
                                        .primary()
                                        .rounded(px(18.))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_editor(window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
