//! The launcher driven through a real (headless) window: real hit-testing, clicks and keys.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use fg_app::views::{Launcher, LauncherEvent, Row, bind_keys};
use fg_core::{ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Environment, TagColor};
use gpui_kit::test::TestWindowExt as _;
// No `use gpui_kit::*`: it would import `gpui_kit::test`, which then shadows the built-in
// `#[test]` that `#[gpui_kit::test]` expands to.
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

fn config(name: &str) -> ConnectionConfig {
    ConnectionConfig::new(
        name,
        DbKind::Sqlite,
        ConnectionParams::File {
            path: format!("/data/{name}.db"),
        },
    )
}

fn row(name: &str, readable: bool) -> Row {
    let c = config(name);
    Row {
        id: c.id.clone(),
        name: name.into(),
        detail: format!("/data/{name}.db"),
        kind: if readable {
            DbKind::Sqlite
        } else {
            DbKind::Unknown
        },
        environment: Environment::Development,
        tag: None,
        config: readable.then_some(c),
    }
}

fn row_id(r: &Row) -> String {
    format!("connection-{}", r.id)
}

struct Fixture {
    window: AnyWindowHandle,
    view: Entity<Launcher>,
    events: Rc<RefCell<Vec<LauncherEvent>>>,
}

fn open(cx: &mut TestAppContext, rows: Vec<Row>) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        bind_keys(cx);
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(720.), px(520.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Launcher::new(window, cx)),
        )
        .unwrap()
    });
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &LauncherEvent, _| {
            log.borrow_mut().push(e.clone())
        })
        .detach();
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.set_rows(rows, cx));
        window.render_frame(cx);
    })
    .unwrap();
    Fixture {
        window,
        view,
        events,
    }
}

fn selected(cx: &mut TestAppContext, f: &Fixture) -> Option<ConnectionId> {
    cx.read_entity(&f.view, |v, _| v.selected().cloned())
}

fn press(cx: &mut TestAppContext, f: &Fixture, key: &str) {
    cx.update_window(f.window, |_, window, cx| {
        window.press(key, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn type_text(cx: &mut TestAppContext, f: &Fixture, text: &str) {
    cx.update_window(f.window, |_, window, cx| {
        window.input(text, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn click(cx: &mut TestAppContext, f: &Fixture, id: &str) {
    let id = id.to_owned();
    cx.update_window(f.window, |_, window, cx| {
        window.click(id, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn exists(cx: &mut TestAppContext, f: &Fixture, id: &str) -> bool {
    let id = id.to_owned();
    cx.update_window(f.window, |_, window, _| window.try_find(id).is_some())
        .unwrap()
}

// ---- the list ------------------------------------------------------------------------------------

#[gpui_kit::test]
fn empty_list_shows_a_hint_and_nothing_to_open(cx: &mut TestAppContext) {
    let f = open(cx, vec![]);
    assert!(exists(cx, &f, "empty-state"));
    assert!(!exists(cx, &f, "connection-list"));
    cx.update_window(f.window, |_, window, _| {
        assert_eq!(window.find("open-selected").disabled(), Some(true));
        assert_eq!(window.find("edit-selected").disabled(), Some(true));
        assert_eq!(window.find("delete-selected").disabled(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_first_row_is_selected_and_clicking_selects_another(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    assert_eq!(selected(cx, &f), Some(a.id.clone()));
    cx.update_window(f.window, |_, window, _| {
        assert_eq!(window.find(row_id(&a)).label(), Some("alpha"));
    })
    .unwrap();
    click(cx, &f, &row_id(&b));
    assert_eq!(selected(cx, &f), Some(b.id.clone()));
    assert!(f.events.borrow().is_empty(), "a single click only selects");
}

#[gpui_kit::test]
fn double_click_and_the_open_button_ask_to_open_the_selection(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);

    // Events are delivered after each update finishes, so check between updates.
    cx.update_window(f.window, |_, window, cx| {
        window.double_click(row_id(&b), cx)
    })
    .unwrap();
    assert_eq!(*f.events.borrow(), vec![LauncherEvent::Open(b.id.clone())]);

    click(cx, &f, &row_id(&a));
    click(cx, &f, "open-selected");
    assert_eq!(
        f.events.borrow().last(),
        Some(&LauncherEvent::Open(a.id.clone()))
    );
    assert_eq!(f.events.borrow().len(), 2);
}

#[gpui_kit::test]
fn the_arrow_keys_move_the_selection_and_enter_opens_it(cx: &mut TestAppContext) {
    let rows: Vec<Row> = ["a", "b", "c"].iter().map(|n| row(n, true)).collect();
    let f = open(cx, rows.clone());
    assert_eq!(selected(cx, &f), Some(rows[0].id.clone()));

    press(cx, &f, "down");
    assert_eq!(selected(cx, &f), Some(rows[1].id.clone()));
    press(cx, &f, "down");
    press(cx, &f, "down");
    assert_eq!(
        selected(cx, &f),
        Some(rows[2].id.clone()),
        "stops at the last row"
    );
    press(cx, &f, "up");
    assert_eq!(selected(cx, &f), Some(rows[1].id.clone()));

    press(cx, &f, "enter");
    assert_eq!(
        *f.events.borrow(),
        vec![LauncherEvent::Open(rows[1].id.clone())]
    );
}

#[gpui_kit::test]
fn unreadable_connections_are_listed_and_deletable_but_not_openable_or_editable(
    cx: &mut TestAppContext,
) {
    let bad = row("from the future", false);
    let f = open(cx, vec![bad.clone()]);
    assert!(
        exists(cx, &f, "connection-list"),
        "a non-empty list is registered"
    );
    cx.update_window(f.window, |_, window, cx| {
        window.double_click(row_id(&bad), cx)
    })
    .unwrap();
    press(cx, &f, "enter");
    assert!(f.events.borrow().is_empty());
    cx.update_window(f.window, |_, window, _| {
        assert_eq!(window.find("edit-selected").disabled(), Some(true));
        assert_eq!(window.find("duplicate-selected").disabled(), Some(true));
        // An enabled button reports no disabled state at all.
        assert_ne!(window.find("delete-selected").disabled(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn while_a_connection_is_opening_further_opens_are_ignored(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    cx.update_window(f.window, |_, window, cx| {
        f.view
            .update(cx, |v, cx| v.set_opening(Some(a.id.clone()), cx));
        window.render_frame(cx);
        assert_eq!(window.find("open-selected").disabled(), Some(true));
        window.double_click(row_id(&b), cx);
    })
    .unwrap();
    assert!(f.events.borrow().is_empty());
}

#[gpui_kit::test]
fn messages_appear_and_clear(cx: &mut TestAppContext) {
    let f = open(cx, vec![row("alpha", true)]);
    assert!(!exists(cx, &f, "launcher-message"));
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            v.set_message(Some("Could not connect: boom".into()), cx)
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("launcher-message").label(),
            Some("Could not connect: boom")
        );
        f.view.update(cx, |v, cx| v.set_message(None, cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert!(!exists(cx, &f, "launcher-message"));
}

#[gpui_kit::test]
fn reloading_keeps_the_selection_when_the_row_survives(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    click(cx, &f, &row_id(&b));
    cx.update_window(f.window, |_, _, cx| {
        f.view
            .update(cx, |v, cx| v.set_rows(vec![a.clone(), b.clone()], cx))
    })
    .unwrap();
    assert_eq!(selected(cx, &f), Some(b.id.clone()));
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| v.set_rows(vec![a.clone()], cx))
    })
    .unwrap();
    assert_eq!(
        selected(cx, &f),
        Some(a.id.clone()),
        "the selected row disappeared"
    );
}

#[gpui_kit::test]
fn the_theme_button_asks_to_cycle_the_theme(cx: &mut TestAppContext) {
    let f = open(cx, vec![]);
    click(cx, &f, "cycle-theme");
    assert_eq!(*f.events.borrow(), vec![LauncherEvent::CycleTheme]);
}

// ---- search --------------------------------------------------------------------------------------

#[gpui_kit::test]
fn the_search_box_filters_the_list_by_name_or_path(cx: &mut TestAppContext) {
    let rows = vec![
        row("billing", true),
        row("analytics", true),
        row("blog", true),
    ];
    let f = open(cx, rows.clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click("launcher-search", cx)
    })
    .unwrap();
    type_text(cx, &f, "bl");
    let names: Vec<String> = cx.read_entity(&f.view, |v, cx| {
        v.visible_rows(cx).iter().map(|r| r.name.clone()).collect()
    });
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"billing".to_string()) && names.contains(&"blog".to_string()));

    // The selection moves off a row that was filtered out.
    let sel = selected(cx, &f).unwrap();
    assert_ne!(sel, rows[1].id);

    type_text(cx, &f, "zzzz");
    assert!(exists(cx, &f, "empty-state"));
    cx.update_window(f.window, |_, window, cx| {
        for _ in 0..6 {
            window.press("backspace", cx);
        }
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(cx.read_entity(&f.view, |v, cx| v.visible_rows(cx).len()), 3);
}

#[gpui_kit::test]
fn enter_in_the_search_box_opens_the_best_match(cx: &mut TestAppContext) {
    let rows = vec![row("billing", true), row("analytics", true)];
    let f = open(cx, rows.clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click("launcher-search", cx)
    })
    .unwrap();
    type_text(cx, &f, "analy");
    press(cx, &f, "enter");
    assert_eq!(
        *f.events.borrow(),
        vec![LauncherEvent::Open(rows[1].id.clone())]
    );
}

// ---- new / edit / duplicate / delete -------------------------------------------------------------

fn saved(f: &Fixture) -> Vec<ConnectionConfig> {
    f.events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            LauncherEvent::Save(c) => Some((**c).clone()),
            _ => None,
        })
        .collect()
}

#[gpui_kit::test]
fn a_new_connection_is_created_from_the_form(cx: &mut TestAppContext) {
    let f = open(cx, vec![]);
    assert!(!exists(cx, &f, "connection-editor"));
    click(cx, &f, "new-connection");
    assert!(exists(cx, &f, "connection-editor"));

    cx.update_window(f.window, |_, window, cx| window.click("editor-name", cx))
        .unwrap();
    type_text(cx, &f, "Shop");
    cx.update_window(f.window, |_, window, cx| window.click("editor-path", cx))
        .unwrap();
    type_text(cx, &f, "/data/shop.db");
    click(cx, &f, "editor-create");
    click(cx, &f, "editor-save");

    let configs = saved(&f);
    assert_eq!(configs.len(), 1);
    let c = &configs[0];
    assert_eq!((c.name.as_str(), c.kind), ("Shop", DbKind::Sqlite));
    assert!(matches!(&c.params, ConnectionParams::File { path } if path == "/data/shop.db"));
    assert!(c.option_enabled("create"));
    assert_eq!(c.environment, Environment::Development);
    assert!(
        !exists(cx, &f, "connection-editor"),
        "saving closes the form"
    );
    assert_eq!(
        selected(cx, &f),
        Some(c.id.clone()),
        "and selects the new connection"
    );
}

#[gpui_kit::test]
fn the_form_needs_a_file_and_keeps_open_until_it_has_one(cx: &mut TestAppContext) {
    let f = open(cx, vec![]);
    click(cx, &f, "new-connection");
    click(cx, &f, "editor-save");
    assert!(exists(cx, &f, "connection-editor"));
    assert!(exists(cx, &f, "editor-error"));
    assert!(saved(&f).is_empty());

    // The suggested name comes from the chosen file when none was typed.
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            v.set_editor_path("/x/orders.sqlite".into(), window, cx)
        });
        window.render_frame(cx);
    })
    .unwrap();
    click(cx, &f, "editor-save");
    assert_eq!(saved(&f)[0].name, "orders");
}

#[gpui_kit::test]
fn cancel_and_the_new_connection_shortcut(cx: &mut TestAppContext) {
    let f = open(cx, vec![row("a", true)]);
    press(cx, &f, "secondary-n");
    assert!(exists(cx, &f, "connection-editor"), "⌘N opens the form");
    click(cx, &f, "editor-cancel");
    assert!(!exists(cx, &f, "connection-editor"));
    assert!(f.events.borrow().is_empty());

    // Enter does not open the selected connection while a form is showing.
    press(cx, &f, "secondary-n");
    press(cx, &f, "enter");
    assert!(f.events.borrow().is_empty());
}

#[gpui_kit::test]
fn editing_starts_from_the_saved_connection_and_keeps_its_other_settings(cx: &mut TestAppContext) {
    let mut c = config("billing");
    c.startup_sql = vec!["PRAGMA foreign_keys = ON".into()];
    c.environment = Environment::Staging;
    c.color = Some(TagColor::Blue);
    let r = Row {
        config: Some(c.clone()),
        ..row("billing", true)
    };
    let r = Row {
        id: c.id.clone(),
        ..r
    };
    let f = open(cx, vec![r]);

    click(cx, &f, "edit-selected");
    assert!(exists(cx, &f, "connection-editor"));
    cx.update_window(f.window, |_, window, cx| window.click("editor-name", cx))
        .unwrap();
    // Select all, then replace the name.
    press(cx, &f, "secondary-a");
    type_text(cx, &f, "Billing DB");
    click(cx, &f, "editor-save");

    let configs = saved(&f);
    assert_eq!(configs.len(), 1);
    let e = &configs[0];
    assert_eq!(e.id, c.id, "the same connection, not a new one");
    assert_eq!(e.name, "Billing DB");
    assert_eq!(
        e.environment,
        Environment::Staging,
        "untouched fields keep their value"
    );
    assert_eq!(e.color, Some(TagColor::Blue));
    assert_eq!(e.startup_sql, c.startup_sql);
}

#[gpui_kit::test]
fn duplicate_saves_a_copy_with_a_new_id(cx: &mut TestAppContext) {
    let r = row("shop", true);
    let f = open(cx, vec![r.clone()]);
    click(cx, &f, "duplicate-selected");
    let configs = saved(&f);
    assert_eq!(configs.len(), 1);
    assert_ne!(configs[0].id, r.id);
    assert_eq!(configs[0].name, "shop copy");
}

#[gpui_kit::test]
fn delete_asks_first_and_only_then_deletes(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    click(cx, &f, &row_id(&b));
    click(cx, &f, "delete-selected");
    assert!(exists(cx, &f, "confirm-delete"));
    assert!(f.events.borrow().is_empty(), "nothing is deleted yet");

    click(cx, &f, "confirm-delete-no");
    assert!(!exists(cx, &f, "confirm-delete"));
    assert!(f.events.borrow().is_empty());

    click(cx, &f, "delete-selected");
    click(cx, &f, "confirm-delete-yes");
    assert_eq!(
        *f.events.borrow(),
        vec![LauncherEvent::Delete(b.id.clone())]
    );
    assert!(!exists(cx, &f, "confirm-delete"));
}

#[gpui_kit::test]
fn selecting_another_row_cancels_a_pending_delete(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    click(cx, &f, "delete-selected");
    assert!(exists(cx, &f, "confirm-delete"));
    click(cx, &f, &row_id(&b));
    assert!(
        !exists(cx, &f, "confirm-delete"),
        "the question was about a different row"
    );
}
