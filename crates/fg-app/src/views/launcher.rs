//! The connection list shown at startup: search, open, create, edit, duplicate, delete.
//!
//! The view holds the data and the form state and reports what the user asked for
//! ([`LauncherEvent`]); saving, deleting and opening happen in [`crate::controller`]. That split
//! keeps the view testable with plain data, without a service or a database.

use fg_core::{ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Environment, TagColor};
use fg_store::StoredConnection;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
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

/// One line of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: ConnectionId,
    pub name: String,
    pub detail: String,
    pub kind: DbKind,
    pub environment: Environment,
    pub tag: Option<TagColor>,
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
                config: Some(c),
            },
            StoredConnection::Unreadable { id, reason } => Row {
                name: "Unsupported connection".into(),
                detail: format!("Saved by a newer version ({reason})"),
                id,
                kind: DbKind::Unknown,
                environment: Environment::Unknown,
                tag: None,
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
    create_if_missing: bool,
    error: Option<SharedString>,
    selected_engine: DbKind,
}

pub struct Launcher {
    rows: Vec<Row>,
    selected: Option<ConnectionId>,
    message: Option<SharedString>,
    opening: Option<ConnectionId>,
    search: Entity<InputState>,
    editor: Option<Editor>,
    confirm_delete: Option<ConnectionId>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
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
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search connections…"));
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
            selected: None,
            message: None,
            opening: None,
            search,
            editor: None,
            confirm_delete: None,
            focus,
            _subscriptions: vec![on_search],
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
        let create_if_missing = draft.create_if_missing;
        let selected_engine = base.as_ref().map_or(DbKind::Sqlite, |c| c.kind);
        self.editor = Some(Editor {
            draft,
            base,
            name,
            path,
            environment,
            color,
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
        Some(ConnectionDraft {
            id: e.draft.id.clone(),
            name: e.name.read(cx).value().to_string(),
            environment: env_label
                .as_deref()
                .map_or(e.draft.environment, environment_from_label),
            color: color_name.as_deref().and_then(color_from_label),
            path: e.path.read(cx).value().to_string(),
            create_if_missing: e.create_if_missing,
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

const ENGINES: &[(DbKind, &str, &str, f32, f32, f32)] = &[
    (
        DbKind::Postgres,
        "PostgreSQL",
        "Pg",
        205.0 / 360.0,
        0.90,
        0.48,
    ),
    (DbKind::MySql, "MySQL", "Ms", 28.0 / 360.0, 0.95, 0.52),
    (DbKind::Sqlite, "SQLite", "Sl", 262.0 / 360.0, 0.83, 0.58),
    (DbKind::Unknown, "MariaDB", "Ma", 173.0 / 360.0, 0.85, 0.36),
    (
        DbKind::Unknown,
        "SQL Server",
        "Ss",
        215.0 / 360.0,
        0.16,
        0.47,
    ),
    (DbKind::Unknown, "DuckDB", "Du", 222.0 / 360.0, 0.47, 0.11),
    (DbKind::Unknown, "Redis", "Re", 0.0 / 360.0, 0.72, 0.51),
    (
        DbKind::Unknown,
        "ClickHouse",
        "Ch",
        45.0 / 360.0,
        0.93,
        0.47,
    ),
];

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let visible: Vec<Row> = self.visible_rows(cx).into_iter().cloned().collect();
        let rows: Vec<AnyElement> = visible
            .iter()
            .map(|row| {
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
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(row.name.clone()),
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
                            .child("Connecting…")
                            .into_any_element()
                    } else {
                        div().size_2p5().rounded_full().bg(dot).into_any_element()
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.select(select_id.clone(), cx);
                        if event.click_count() >= 2 && readable {
                            this.open_selected(cx);
                        }
                        let _ = &open_id;
                    }))
                    .into_any_element()
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
            // Left Sidebar: Brand & Actions
            .child(
                v_flex()
                    .w(px(200.))
                    .h_full()
                    .bg(theme.sidebar)
                    .border_r_1()
                    .border_color(theme.border)
                    .p_3()
                    .justify_between()
                    .child(
                        v_flex()
                            .gap_3()
                            .items_center()
                            .pt_2()
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
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Version 0.0.1"),
                                    ),
                            )
                            .child(
                                div()
                                    .px_2p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(theme.warning.opacity(0.15))
                                    .text_color(theme.warning)
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("SQLite · PostgreSQL · MySQL"),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .w_full()
                            .child(
                                Button::new("backup-db")
                                    .icon(hugeicon(Hugeicon::Backup))
                                    .label("Backup Database")
                                    .ghost()
                                    .small()
                                    .w_full(),
                            )
                            .child(
                                Button::new("restore-db")
                                    .icon(hugeicon(Hugeicon::Restore))
                                    .label("Restore Database")
                                    .ghost()
                                    .small()
                                    .w_full(),
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
                                    .ghost()
                                    .small()
                                    .w_full()
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(LauncherEvent::CycleTheme);
                                    })),
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
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Button::new("new-connection")
                                            .icon(hugeicon(Hugeicon::Plus))
                                            .label("New…")
                                            .primary()
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.new_connection(window, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("new-group")
                                            .icon(hugeicon(Hugeicon::FolderPlus))
                                            .ghost()
                                            .tooltip("New Group"),
                                    ),
                            )
                            .child(
                                div().w(px(260.)).child(
                                    Input::new(&self.search)
                                        .id("launcher-search")
                                        .small()
                                        .cleanable(true),
                                ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("open-selected")
                                    .label("Open")
                                    .primary()
                                    .small()
                                    .disabled(!can_open)
                                    .on_click(cx.listener(|this, _, _, cx| this.open_selected(cx))),
                            )
                            .child(
                                Button::new("edit-selected")
                                    .icon(hugeicon(Hugeicon::Edit))
                                    .label("Edit…")
                                    .small()
                                    .disabled(!editable)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.edit_selected(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("duplicate-selected")
                                    .icon(hugeicon(Hugeicon::Copy))
                                    .label("Duplicate")
                                    .small()
                                    .disabled(!editable)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.duplicate_selected(cx);
                                    })),
                            )
                            .child(
                                Button::new("delete-selected")
                                    .icon(hugeicon(Hugeicon::Trash))
                                    .label("Delete")
                                    .small()
                                    .disabled(!has_selection)
                                    .on_click(cx.listener(|this, _, _, cx| this.ask_delete(cx))),
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
                            .child(if self.rows.is_empty() {
                                "No connections yet. Click New… to add a SQLite file."
                            } else {
                                "No connection matches the search."
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
    fn render_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.editor.as_ref()?;
        let theme = cx.theme().clone();
        let title = if e.draft.id.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        let field = |label: &'static str, body: AnyElement| {
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
                let is_chosen = match active_engine {
                    DbKind::Sqlite => *kind == DbKind::Sqlite && *name == "SQLite",
                    DbKind::Postgres => *kind == DbKind::Postgres && *name == "PostgreSQL",
                    DbKind::MySql => *kind == DbKind::MySql && *name == "MySQL",
                    _ => *kind == DbKind::Sqlite && *name == "SQLite",
                };
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
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Choose database engine"),
                                )
                                .child(h_flex().flex_wrap().gap_2().children(engine_cards)),
                        )
                        .child(field(
                            "Name",
                            Input::new(&e.name).id("editor-name").into_any_element(),
                        ))
                        .child(field(
                            "Database file",
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&e.path).id("editor-path")))
                                .child(Button::new("editor-browse").label("Browse…").on_click(
                                    cx.listener(|this, _, window, cx| this.browse_file(window, cx)),
                                ))
                                .into_any_element(),
                        ))
                        .child(
                            h_flex()
                                .gap_3()
                                .child(
                                    div().flex_1().child(field(
                                        "Environment",
                                        Select::new(&e.environment)
                                            .id("editor-environment")
                                            .into_any_element(),
                                    )),
                                )
                                .child(div().flex_1().child(field(
                                    "Colour tag",
                                    Select::new(&e.color).id("editor-color").into_any_element(),
                                ))),
                        )
                        .child(
                            Checkbox::new("editor-create")
                                .label("Create the file if it does not exist")
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
                                .child(Button::new("editor-cancel").label("Cancel").on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.close_editor(window, cx);
                                    }),
                                ))
                                .child(
                                    Button::new("editor-save").label("Save").primary().on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.save_editor(window, cx);
                                        }),
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
