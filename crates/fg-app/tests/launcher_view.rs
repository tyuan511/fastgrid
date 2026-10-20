//! The launcher driven through a real (headless) window: real hit-testing, clicks and frames.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use fg_app::views::{Launcher, LauncherEvent, Row};
use fg_core::{ConnectionId, DbKind, Environment};
use gpui_kit::test::TestWindowExt as _;
// No `use gpui_kit::*`: it would import `gpui_kit::test`, which then shadows the built-in
// `#[test]` that `#[gpui_kit::test]` expands to.
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

fn row(name: &str, readable: bool) -> Row {
    Row {
        id: ConnectionId::new(),
        name: name.into(),
        detail: format!("/data/{name}.db"),
        kind: if readable {
            DbKind::Sqlite
        } else {
            DbKind::Unknown
        },
        environment: Environment::Development,
        tag: None,
        readable,
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
    cx.update(gpui_kit::init);
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(640.), px(480.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |_, cx| cx.new(|_| Launcher::new()),
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

#[gpui_kit::test]
fn empty_list_shows_a_hint_and_nothing_to_open(cx: &mut TestAppContext) {
    let f = open(cx, vec![]);
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("empty-state").is_some());
        assert!(window.try_find("connection-list").is_none());
        assert_eq!(window.find("open-selected").disabled(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_first_row_is_selected_and_clicking_selects_another(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    cx.update_window(f.window, |_, window, cx| {
        assert_eq!(f.view.read(cx).selected(), Some(&a.id));
        assert_eq!(window.find(row_id(&a)).label(), Some("alpha"));

        window.click(row_id(&b), cx);
        assert_eq!(f.view.read(cx).selected(), Some(&b.id));
        window.click(row_id(&a), cx);
        assert_eq!(f.view.read(cx).selected(), Some(&a.id));
        assert!(f.events.borrow().is_empty(), "a single click only selects");
    })
    .unwrap();
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

    cx.update_window(f.window, |_, window, cx| {
        window.click(row_id(&a), cx);
        window.click("open-selected", cx);
    })
    .unwrap();
    assert_eq!(
        f.events.borrow().last(),
        Some(&LauncherEvent::Open(a.id.clone()))
    );
    assert_eq!(f.events.borrow().len(), 2);
}

#[gpui_kit::test]
fn add_sqlite_is_requested_through_an_event(cx: &mut TestAppContext) {
    let f = open(cx, vec![]);
    cx.update_window(f.window, |_, window, cx| {
        window.click("add-sqlite", cx);
    })
    .unwrap();
    assert_eq!(*f.events.borrow(), vec![LauncherEvent::AddSqlite]);
}

#[gpui_kit::test]
fn unreadable_connections_are_listed_but_cannot_be_opened(cx: &mut TestAppContext) {
    let bad = row("from the future", false);
    let f = open(cx, vec![bad.clone()]);
    cx.update_window(f.window, |_, window, cx| {
        assert!(window.try_find(row_id(&bad)).is_some());
        assert!(
            window.try_find("connection-list").is_some(),
            "a non-empty list is registered"
        );
        window.double_click(row_id(&bad), cx);
    })
    .unwrap();
    assert!(f.events.borrow().is_empty());
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
    cx.update_window(f.window, |_, window, cx| {
        assert!(window.try_find("launcher-message").is_none());
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
        assert!(window.try_find("launcher-message").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn reloading_keeps_the_selection_when_the_row_survives(cx: &mut TestAppContext) {
    let (a, b) = (row("alpha", true), row("beta", true));
    let f = open(cx, vec![a.clone(), b.clone()]);
    cx.update_window(f.window, |_, window, cx| {
        window.click(row_id(&b), cx);
        f.view
            .update(cx, |v, cx| v.set_rows(vec![a.clone(), b.clone()], cx));
        window.render_frame(cx);
        assert_eq!(f.view.read(cx).selected(), Some(&b.id));

        // The selected row disappears: selection moves to the first row.
        f.view.update(cx, |v, cx| v.set_rows(vec![a.clone()], cx));
        window.render_frame(cx);
        assert_eq!(f.view.read(cx).selected(), Some(&a.id));
    })
    .unwrap();
}
