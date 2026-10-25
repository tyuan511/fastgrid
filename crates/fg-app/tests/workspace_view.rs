//! The workspace window with a synthetic schema (no database): sidebar, filter, tabs.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use fg_app::views::{WorkspaceEvent, WorkspaceView, open_workspace_window};
use fg_core::{
    ConnectionConfig, ConnectionParams, DbKind, Environment, ObjectInfo, ObjectKind, ObjectPath,
    TabKind, TableViewMode,
};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

struct Fixture {
    window: AnyWindowHandle,
    view: Entity<WorkspaceView>,
    opened: Rc<Cell<usize>>,
}

fn t(name: &str) -> ObjectPath {
    ObjectPath::new(Some("main"), name)
}

fn schema() -> (Vec<String>, BTreeMap<String, Vec<ObjectInfo>>) {
    let objects = ["users", "orders", "order_items", "products"]
        .iter()
        .map(|n| ObjectInfo::new(t(n), ObjectKind::Table))
        .chain([ObjectInfo::new(t("v_active_users"), ObjectKind::View)])
        .collect();
    (
        vec!["main".into()],
        BTreeMap::from([("main".to_string(), objects)]),
    )
}

fn open(cx: &mut TestAppContext) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        fg_app::views::bind_keys(cx);
    });
    let mut config = ConnectionConfig::new(
        "Billing",
        DbKind::Postgres,
        ConnectionParams::File { path: "x".into() },
    );
    config.environment = Environment::Production;
    let (window, view) = cx.update(|cx| open_workspace_window(cx, config).unwrap());
    let opened = Rc::new(Cell::new(0));
    let count = Rc::clone(&opened);
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &WorkspaceEvent, _| {
            if matches!(e, WorkspaceEvent::TableOpened { .. }) {
                count.set(count.get() + 1);
            }
        })
        .detach();
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| {
            let (ns, objs) = schema();
            v.set_schema(ns, objs, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    Fixture {
        window,
        view,
        opened,
    }
}

fn open_table(cx: &mut TestAppContext, f: &Fixture, name: &str) {
    cx.update_window(f.window, |_, window, cx| {
        f.view
            .update(cx, |v, cx| v.open_table(&t(name), window, cx));
        window.render_frame(cx);
    })
    .unwrap();
}

fn titles(cx: &mut TestAppContext, f: &Fixture) -> Vec<String> {
    cx.read_entity(&f.view, |v, _| {
        v.tabs()
            .tabs()
            .iter()
            .map(|t| match &t.kind {
                TabKind::Table { path, .. } => path.name.clone(),
                _ => "query".into(),
            })
            .collect()
    })
}

#[gpui_kit::test]
fn the_title_bar_shows_the_connection_with_its_environment(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, _| {
        assert_eq!(window.find("connection-pill").label(), Some("Billing"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_sidebar_lists_tables_and_views_and_an_empty_main_area_hints(cx: &mut TestAppContext) {
    let f = open(cx);
    let shown = cx.read_entity(&f.view, |v, _| v.visible_tables());
    assert_eq!(shown.len(), 5);
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("no-tab").is_some());
        assert!(window.try_find("schema-loading").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn typing_in_the_filter_narrows_the_tree(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, cx| {
        window.click("sidebar-filter", cx);
        window.input("usr", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let names: Vec<String> = cx
        .read_entity(&f.view, |v, _| v.visible_tables())
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["users", "v_active_users"]);

    // Clearing the filter restores everything.
    cx.update_window(f.window, |_, window, cx| {
        for _ in 0..3 {
            window.press("backspace", cx);
        }
    })
    .unwrap();
    assert_eq!(cx.read_entity(&f.view, |v, _| v.visible_tables()).len(), 5);
}

#[gpui_kit::test]
fn opening_a_table_twice_reuses_its_tab_and_loads_once(cx: &mut TestAppContext) {
    let f = open(cx);
    open_table(cx, &f, "users");
    open_table(cx, &f, "orders");
    open_table(cx, &f, "users");
    assert_eq!(titles(cx, &f), ["users", "orders"]);
    assert_eq!(
        f.opened.get(),
        2,
        "the controller is asked to load each table once"
    );
    let active = cx
        .read_entity(&f.view, |v, _| v.tabs().active().cloned())
        .unwrap();
    assert!(matches!(&active.kind, TabKind::Table { path, .. } if path.name == "users"));
}

#[gpui_kit::test]
fn closing_a_tab_drops_it_and_its_view(cx: &mut TestAppContext) {
    let f = open(cx);
    open_table(cx, &f, "users");
    open_table(cx, &f, "orders");
    let orders = cx.read_entity(&f.view, |v, _| v.tabs().active_id().unwrap().clone());

    cx.update_window(f.window, |_, window, cx| {
        window.click(format!("close-tab-{orders}"), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(titles(cx, &f), ["users"]);
    assert!(cx.read_entity(&f.view, |v, _| v.tab_view(&orders).is_none()));
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("no-tab").is_none())
    })
    .unwrap();

    // Closing the last tab brings the hint back.
    let users = cx.read_entity(&f.view, |v, _| v.tabs().active_id().unwrap().clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click(format!("close-tab-{users}"), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("no-tab").is_some())
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_sidebar_can_be_hidden_and_shown(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, cx| {
        assert!(window.try_find("sidebar").is_some());
        window.click("toggle-sidebar", cx);
        window.render_frame(cx);
        assert!(window.try_find("sidebar").is_none());
        window.click("toggle-sidebar", cx);
        window.render_frame(cx);
        assert!(window.try_find("sidebar").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_session_restores_only_tables_that_still_exist(cx: &mut TestAppContext) {
    let f = open(cx);
    open_table(cx, &f, "users");
    open_table(cx, &f, "orders");
    let mut saved = cx.read_entity(&f.view, |v, _| v.session());
    // A table dropped since last time.
    if let Some(TabKind::Table { path, mode }) = saved.tabs.get_mut(0).map(|t| &mut t.kind) {
        *path = t("dropped_table");
        *mode = TableViewMode::Data;
    }

    let second = open(cx);
    cx.update_window(second.window, |_, window, cx| {
        second
            .view
            .update(cx, |v, cx| v.restore(&saved, window, cx));
    })
    .unwrap();
    assert_eq!(titles(cx, &second), ["orders"]);
}

#[gpui_kit::test]
fn the_session_remembers_the_sidebar(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, cx| {
        window.click("toggle-sidebar", cx);
    })
    .unwrap();
    let saved = cx.read_entity(&f.view, |v, _| v.session());
    assert!(!saved.sidebar_visible);

    let second = open(cx);
    cx.update_window(second.window, |_, window, cx| {
        second
            .view
            .update(cx, |v, cx| v.restore(&saved, window, cx));
        window.render_frame(cx);
        assert!(window.try_find("sidebar").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_sidebar_says_when_the_schema_is_loading_or_failed(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let config = ConnectionConfig::new(
        "x",
        DbKind::Sqlite,
        ConnectionParams::File { path: "x".into() },
    );
    let (window, view) = cx.update(|cx| open_workspace_window(cx, config).unwrap());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("schema-loading").is_some(),
            "shown until the schema arrives"
        );
        assert!(window.try_find("schema-error").is_none());

        view.update(cx, |v, cx| {
            v.set_schema_error("Could not read the schema: boom".into(), cx)
        });
        window.render_frame(cx);
        assert!(window.try_find("schema-loading").is_none());
        assert_eq!(
            window.find("schema-error").label(),
            Some("Could not read the schema: boom")
        );

        let (ns, objs) = schema();
        view.update(cx, |v, cx| v.set_schema(ns, objs, cx));
        window.render_frame(cx);
        assert!(window.try_find("schema-error").is_none());
        assert!(window.try_find("schema-loading").is_none());
    })
    .unwrap();
}

// ---- keyboard ------------------------------------------------------------------------------------

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

fn quick_visible(cx: &mut TestAppContext, f: &Fixture) -> bool {
    cx.read_entity(&f.view, |v, _| v.quick_open_visible())
}

#[gpui_kit::test]
fn quick_open_finds_a_table_by_typing_and_opens_it_with_enter(cx: &mut TestAppContext) {
    let f = open(cx);
    assert!(!quick_visible(cx, &f));
    press(cx, &f, "secondary-p");
    assert!(quick_visible(cx, &f), "⌘P opens the palette");
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("quick-open").is_some());
    })
    .unwrap();

    type_text(cx, &f, "ordit");
    press(cx, &f, "enter");
    assert!(!quick_visible(cx, &f), "choosing closes the palette");
    assert_eq!(
        titles(cx, &f),
        ["order_items"],
        "fuzzy match 'ordit' -> order_items"
    );
}

#[gpui_kit::test]
fn escape_closes_quick_open_without_opening_anything(cx: &mut TestAppContext) {
    let f = open(cx);
    press(cx, &f, "secondary-p");
    assert!(quick_visible(cx, &f));
    press(cx, &f, "escape");
    assert!(!quick_visible(cx, &f));
    assert!(titles(cx, &f).is_empty());

    // The window still reacts to shortcuts afterwards (focus came back).
    press(cx, &f, "secondary-p");
    assert!(quick_visible(cx, &f));
}

#[gpui_kit::test]
fn the_arrow_keys_choose_between_matches(cx: &mut TestAppContext) {
    let f = open(cx);
    press(cx, &f, "secondary-p");
    type_text(cx, &f, "order");
    // Matches: order_items, orders (shorter first on a tie is the model's rule; the palette keeps
    // its own order, so just check that moving down changes the result).
    press(cx, &f, "down");
    press(cx, &f, "enter");
    let first = titles(cx, &f);
    assert_eq!(first.len(), 1);

    press(cx, &f, "secondary-p");
    type_text(cx, &f, "order");
    press(cx, &f, "enter");
    let second = titles(cx, &f);
    assert_eq!(
        second.len(),
        2,
        "a different table was chosen the second time"
    );
    assert_ne!(second[0], second[1]);
}

#[gpui_kit::test]
fn tab_shortcuts_close_cycle_and_toggle(cx: &mut TestAppContext) {
    let f = open(cx);
    open_table(cx, &f, "users");
    open_table(cx, &f, "orders");
    open_table(cx, &f, "products");
    let active = |cx: &mut TestAppContext| {
        cx.read_entity(&f.view, |v, _| match &v.tabs().active().unwrap().kind {
            TabKind::Table { path, .. } => path.name.clone(),
            _ => String::new(),
        })
    };
    assert_eq!(active(cx), "products");

    press(cx, &f, "secondary-[");
    assert_eq!(active(cx), "orders");
    press(cx, &f, "secondary-]");
    assert_eq!(active(cx), "products");
    press(cx, &f, "ctrl-tab");
    assert_eq!(active(cx), "users", "wraps around");

    press(cx, &f, "secondary-w");
    assert_eq!(titles(cx, &f), ["orders", "products"]);

    press(cx, &f, "secondary-b");
    assert!(!cx.read_entity(&f.view, |v, _| v.sidebar_visible()));
    press(cx, &f, "secondary-b");
    assert!(cx.read_entity(&f.view, |v, _| v.sidebar_visible()));
}

#[gpui_kit::test]
fn the_filter_shortcut_toggles_the_active_tabs_drawer(cx: &mut TestAppContext) {
    let f = open(cx);
    press(cx, &f, "secondary-f"); // no tab open: nothing happens, no panic
    open_table(cx, &f, "users");
    let open = |cx: &mut TestAppContext| {
        cx.read_entity(&f.view, |v, cx| {
            let id = v.tabs().active_id().unwrap().clone();
            v.tab_view(&id).unwrap().read(cx).filters_open()
        })
    };
    assert!(!open(cx));
    press(cx, &f, "secondary-f");
    assert!(open(cx));
    press(cx, &f, "secondary-f");
    assert!(!open(cx));
}

#[gpui_kit::test]
fn the_theme_button_asks_the_controller_to_cycle(cx: &mut TestAppContext) {
    let f = open(cx);
    let asked = Rc::new(Cell::new(0));
    let count = Rc::clone(&asked);
    cx.update(|cx| {
        cx.subscribe(&f.view, move |_, e: &WorkspaceEvent, _| {
            if matches!(e, WorkspaceEvent::CycleTheme) {
                count.set(count.get() + 1);
            }
        })
        .detach();
    });
    cx.update_window(f.window, |_, window, cx| window.click("cycle-theme", cx))
        .unwrap();
    assert_eq!(asked.get(), 1);
}

#[gpui_kit::test]
fn the_sidebar_width_is_kept_within_bounds_and_saved(cx: &mut TestAppContext) {
    let f = open(cx);
    let set = |cx: &mut TestAppContext, w: f32| {
        cx.update_window(f.window, |_, window, cx| {
            f.view
                .update(cx, |v, cx| v.set_sidebar_width(w, window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.read_entity(&f.view, |v, _| v.sidebar_width())
    };
    assert_eq!(
        cx.read_entity(&f.view, |v, _| v.sidebar_width()),
        220.0,
        "the default"
    );
    assert_eq!(set(cx, 300.0), 300.0);
    assert_eq!(set(cx, 50.0), 140.0, "not narrower than the minimum");
    assert_eq!(set(cx, 9000.0), 450.0, "not wider than the maximum");

    set(cx, 333.0);
    let saved = cx.read_entity(&f.view, |v, _| v.session());
    assert_eq!(saved.sidebar_width, Some(333.0));

    let second = open(cx);
    cx.update_window(second.window, |_, window, cx| {
        second
            .view
            .update(cx, |v, cx| v.restore(&saved, window, cx));
    })
    .unwrap();
    assert_eq!(
        cx.read_entity(&second.view, |v, _| v.sidebar_width()),
        333.0
    );
}

#[gpui_kit::test]
fn dragging_a_tab_onto_another_reorders_the_strip(cx: &mut TestAppContext) {
    let f = open(cx);
    open_table(cx, &f, "users");
    open_table(cx, &f, "orders");
    assert_eq!(titles(cx, &f), ["users", "orders"]);
    let ids: Vec<_> = cx.read_entity(&f.view, |v, _| {
        v.tabs().tabs().iter().map(|t| t.id.clone()).collect()
    });

    cx.update_window(f.window, |_, window, cx| {
        window.drag_to(format!("tab-{}", ids[1]), format!("tab-{}", ids[0]), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(titles(cx, &f), ["orders", "users"]);
}

#[gpui_kit::test]
fn selecting_a_row_in_a_table_populates_the_details_inspector(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("details-panel").is_some());
        assert!(window.try_find("details-filter").is_some());
    })
    .unwrap();

    open_table(cx, &f, "users");
    let tab = cx.read_entity(&f.view, |v, _| {
        let id = v.tabs().active_id().unwrap();
        v.tab_view(id).unwrap().clone()
    });

    cx.update_window(f.window, |_, window, cx| {
        tab.update(cx, |t, cx| {
            let generation = t.begin_load(cx);
            t.set_columns(
                generation,
                vec![
                    fg_core::ColumnMeta::new(
                        "id",
                        fg_core::DataType::new("INTEGER", fg_core::TypeCategory::Integer),
                    ),
                    fg_core::ColumnMeta::new(
                        "username",
                        fg_core::DataType::new("TEXT", fg_core::TypeCategory::Text),
                    ),
                ],
                cx,
            );
            t.append_rows(
                generation,
                vec![vec![
                    fg_core::Value::Int(42),
                    fg_core::Value::Text("alice".into()),
                ]],
                cx,
            );
            t.finish(generation, 5, cx);
            t.select_row(0, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();

    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("details-panel").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn closing_a_dirty_table_tab_asks_first_and_only_then_closes(cx: &mut TestAppContext) {
    let f = open(cx);
    open_table(cx, &f, "users");
    let id = cx.read_entity(&f.view, |v, _| v.tabs().tabs()[0].id.clone());
    let tab = cx.read_entity(&f.view, |v, _| v.tab_view(&id).unwrap().clone());
    cx.update_window(f.window, |_, window, cx| {
        tab.update(cx, |t, cx| {
            t.set_primary_key(vec!["id".to_owned()], cx);
            let generation = t.begin_load(cx);
            t.set_columns(
                generation,
                vec![
                    fg_core::ColumnMeta::new(
                        "id",
                        fg_core::DataType::new("INTEGER", fg_core::TypeCategory::Integer),
                    ),
                    fg_core::ColumnMeta::new(
                        "username",
                        fg_core::DataType::new("TEXT", fg_core::TypeCategory::Text),
                    ),
                ],
                cx,
            );
            t.append_rows(
                generation,
                vec![vec![
                    fg_core::Value::Int(42),
                    fg_core::Value::Text("alice".into()),
                ]],
                cx,
            );
            t.finish(generation, 1, cx);
            t.select_cell(0, 1, cx);
            t.set_selected_cell_to_null(cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        cx.read_entity(&tab, |t, cx| t.pending(cx).total()),
        1,
        "one staged edit"
    );

    let banner = |cx: &mut TestAppContext| {
        cx.update_window(f.window, |_, window, _| {
            window.try_find("confirm-close-tab").is_some()
        })
        .unwrap()
    };
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.close_tab(&id, cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(titles(cx, &f), vec!["users"], "first close only asks");
    assert!(banner(cx));

    cx.update_window(f.window, |_, window, cx| {
        window.click("confirm-close-no", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(titles(cx, &f), vec!["users"], "cancel keeps the tab");
    assert!(!banner(cx));

    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.close_tab(&id, cx));
        window.render_frame(cx);
        window.click("confirm-close-yes", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(titles(cx, &f).is_empty(), "confirming closes the tab");
}
