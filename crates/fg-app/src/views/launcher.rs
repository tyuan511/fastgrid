//! The connection list shown at startup.
//!
//! This view only shows data and reports what the user asked for ([`LauncherEvent`]); loading,
//! saving and opening happen in [`crate::controller`]. That split keeps the view testable with
//! plain data, without a service or a database.

use fg_core::{ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Environment, TagColor};
use fg_store::StoredConnection;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::style::environment_color;

/// One line of the list. Plain data.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: ConnectionId,
    pub name: String,
    pub detail: String,
    pub kind: DbKind,
    pub environment: Environment,
    pub tag: Option<TagColor>,
    /// False for a record from a newer version that this build cannot read.
    pub readable: bool,
}

pub fn rows_from(stored: Vec<StoredConnection>) -> Vec<Row> {
    stored
        .into_iter()
        .map(|s| match s {
            StoredConnection::Ok(c) => Row {
                detail: describe(&c),
                id: c.id,
                name: c.name,
                kind: c.kind,
                environment: c.environment,
                tag: c.color,
                readable: true,
            },
            StoredConnection::Unreadable { id, reason } => Row {
                name: "Unsupported connection".into(),
                detail: format!("Saved by a newer version ({reason})"),
                id,
                kind: DbKind::Unknown,
                environment: Environment::Unknown,
                tag: None,
                readable: false,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherEvent {
    Open(ConnectionId),
    AddSqlite,
}

pub struct Launcher {
    rows: Vec<Row>,
    selected: Option<ConnectionId>,
    message: Option<SharedString>,
    opening: Option<ConnectionId>,
}

impl EventEmitter<LauncherEvent> for Launcher {}

impl Launcher {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            selected: None,
            message: None,
            opening: None,
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&ConnectionId> {
        self.selected.as_ref()
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

    fn request_open(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if self.opening.is_none() {
            cx.emit(LauncherEvent::Open(id));
        }
    }
}

impl Default for Launcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows: Vec<AnyElement> =
            self.rows
                .iter()
                .map(|row| {
                    let selected = self.selected.as_ref() == Some(&row.id);
                    let opening = self.opening.as_ref() == Some(&row.id);
                    let dot = environment_color(cx, row.environment, row.tag);
                    let (select_id, open_id) = (row.id.clone(), row.id.clone());
                    let readable = row.readable;
                    h_flex()
                        .id(SharedString::from(format!("connection-{}", row.id)))
                        .test_support()
                        .aria_label(row.name.clone())
                        .gap_3()
                        .px_3()
                        .py_2()
                        .items_center()
                        .rounded_md()
                        .when(selected, |d| d.bg(theme.list_active))
                        .hover(|d| d.bg(theme.list_hover))
                        .child(div().size_2p5().rounded_full().bg(dot))
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_0p5()
                                .child(div().text_sm().child(row.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(row.detail.clone()),
                                ),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(
                            if opening {
                                "Connecting…"
                            } else {
                                row.kind.display_name()
                            },
                        ))
                        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                            this.select(select_id.clone(), cx);
                            if event.click_count() >= 2 && readable {
                                this.request_open(open_id.clone(), cx);
                            }
                        }))
                        .into_any_element()
                })
                .collect();

        let can_open = self.selected.is_some() && self.opening.is_none();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_lg().child("Connections"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("open-selected")
                                    .label("Open")
                                    .primary()
                                    .disabled(!can_open)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(id) = this.selected.clone() {
                                            this.request_open(id, cx);
                                        }
                                    })),
                            )
                            .child(Button::new("add-sqlite").label("Add SQLite…").on_click(
                                cx.listener(|_, _, _, cx| {
                                    cx.emit(LauncherEvent::AddSqlite);
                                }),
                            )),
                    ),
            )
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
                    .child("No connections yet. Add a SQLite file to get started.")
                    .into_any_element()
            } else {
                v_flex()
                    .id("connection-list")
                    .test_support()
                    .role(Role::List)
                    .flex_1()
                    .gap_1()
                    .children(rows)
                    .into_any_element()
            })
    }
}
