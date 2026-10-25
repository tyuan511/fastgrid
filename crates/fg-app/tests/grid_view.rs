//! The data grid tab driven through a real headless window.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use fg_app::views::{TableTab, TableTabEvent};
// No `use gpui_kit::*`: it would import `gpui_kit::test`, which then shadows the built-in
// `#[test]` that `#[gpui_kit::test]` expands to.
use fg_core::{ColumnMeta, DataType, ObjectPath, SortKey, TypeCategory, Value};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

struct Fixture {
    window: AnyWindowHandle,
    view: Entity<TableTab>,
    events: Rc<RefCell<Vec<TableTabEvent>>>,
}

fn open(cx: &mut TestAppContext) -> Fixture {
    cx.update(gpui_kit::init);
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(900.), px(600.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| TableTab::new(ObjectPath::new(Some("main"), "items"), window, cx))
            },
        )
        .unwrap()
    });
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &TableTabEvent, _| {
            log.borrow_mut().push(e.clone())
        })
        .detach();
    });
    Fixture {
        window,
        view,
        events,
    }
}

fn columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
        ColumnMeta::new("name", DataType::new("VARCHAR(100)", TypeCategory::Text)),
    ]
}

fn rows(n: i64) -> Vec<Vec<Value>> {
    (1..=n)
        .map(|i| {
            vec![
                Value::Int(i),
                if i % 2 == 0 {
                    Value::Null
                } else {
                    Value::Text(format!("item {i}"))
                },
            ]
        })
        .collect()
}

/// Run a full load cycle as the controller would.
fn load(cx: &mut TestAppContext, f: &Fixture, n: i64, total: Option<u64>) {
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            let g = v.begin_load(cx);
            v.set_columns(g, columns(), cx);
            v.append_rows(g, rows(n), cx);
            v.finish(g, 7, cx);
            if let Some(t) = total {
                v.set_total(g, t, cx);
            }
        });
        window.render_frame(cx);
    })
    .unwrap();
}

/// Rendered width of the `sort-{name}` header, which fills its column.
fn header_width(cx: &mut TestAppContext, f: &Fixture, id: &'static str) -> f32 {
    cx.update_window(f.window, |_, window, cx| {
        window.render_frame(cx);
        f32::from(window.find(id).bounds().size.width)
    })
    .unwrap()
}

/// Load exactly `rows` instead of the generated ones, as the controller would.
fn load_rows(cx: &mut TestAppContext, f: &Fixture, rows: Vec<Vec<Value>>) {
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            let g = v.begin_load(cx);
            v.set_columns(g, columns(), cx);
            v.append_rows(g, rows, cx);
            v.finish(g, 7, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
}

/// Double-click the resize handle of screen column `col_ix`: the handle band itself carries no
/// test id, so click 2 px inside the column header's right edge, where the trailing band sits.
fn double_click_handle(cx: &mut TestAppContext, f: &Fixture, col_ix: usize) {
    use gpui_kit::test::ClickOptions;
    use gpui_kit::{Point, px};
    cx.update_window(f.window, |_, window, cx| {
        window.render_frame(cx);
        let bounds = window.find(("col-header", col_ix)).bounds();
        window.click_with_options(
            ("col-header", col_ix),
            ClickOptions::new().with_count(2).with_offset(Point::new(
                bounds.size.width - px(2.),
                bounds.size.height / 2.,
            )),
            cx,
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn double_clicking_a_resize_handle_fits_the_column_to_its_content(cx: &mut TestAppContext) {
    let f = open(cx);
    load_rows(
        cx,
        &f,
        vec![vec![
            Value::Int(1),
            Value::Text("a very long item name that should widen the column".into()),
        ]],
    );
    let before = header_width(cx, &f, "sort-name");
    double_click_handle(cx, &f, 2);
    let after = header_width(cx, &f, "sort-name");
    assert!(
        after > before,
        "fitted {after} should exceed the default {before}"
    );
}

#[gpui_kit::test]
fn double_clicking_a_resize_handle_shrinks_a_column_to_short_content(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 3, None);
    let before = header_width(cx, &f, "sort-name");
    double_click_handle(cx, &f, 2);
    let after = header_width(cx, &f, "sort-name");
    assert!(
        after < before,
        "fitted {after} should shrink below the default {before}"
    );
}

#[gpui_kit::test]
fn double_clicking_a_resize_handle_with_no_rows_fits_the_header(cx: &mut TestAppContext) {
    let f = open(cx);
    load_rows(cx, &f, vec![]);
    let before = header_width(cx, &f, "sort-name");
    double_click_handle(cx, &f, 2);
    let after = header_width(cx, &f, "sort-name");
    assert!(
        after < before,
        "fitted {after} should shrink to the header below {before}"
    );
}

fn status(cx: &mut TestAppContext, f: &Fixture) -> String {
    cx.update_window(f.window, |_, window, _| {
        window.find("grid-status").label().unwrap().to_owned()
    })
    .unwrap()
}

fn disabled(cx: &mut TestAppContext, f: &Fixture, id: &'static str) -> bool {
    cx.update_window(f.window, |_, window, _| {
        window.find(id).disabled() == Some(true)
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_status_bar_reports_rows_total_and_time(cx: &mut TestAppContext) {
    let f = open(cx);
    assert_eq!(status(cx, &f), "No rows");
    load(cx, &f, 3, None);
    assert_eq!(status(cx, &f), "Showing 1–3 · 7 ms");
    load(cx, &f, 3, Some(1_234));
    assert_eq!(status(cx, &f), "Showing 1–3 of 1,234 · 7 ms");
}

/// First / last page and the page sizes sit in the panel behind the ⚙ between the arrows.
fn open_paging_panel(cx: &mut TestAppContext, f: &Fixture) {
    cx.update_window(f.window, |_, window, cx| {
        window.click("page-settings", cx);
        window.render_frame(cx);
        assert!(window.try_find("paging-panel").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_paging_panel_opens_and_closes_from_the_status_bar(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 300, Some(1000));
    cx.update_window(f.window, |_, window, _| {
        assert!(window.try_find("page-settings").is_some());
        assert!(window.try_find("paging-panel").is_none());
        assert!(window.try_find("page-size-1000").is_none());
    })
    .unwrap();
    open_paging_panel(cx, &f);
    cx.update_window(f.window, |_, window, cx| {
        assert!(window.try_find("page-size-1000").is_some());
        assert!(window.try_find("grid-page-first").is_some());
        window.click("page-settings", cx);
        window.render_frame(cx);
        assert!(window.try_find("paging-panel").is_none());
    })
    .unwrap();
    assert!(
        f.events.borrow().is_empty(),
        "opening the panel is not a reload"
    );
}

#[gpui_kit::test]
fn paging_buttons_follow_what_is_known(cx: &mut TestAppContext) {
    let f = open(cx);
    // 300 of 1000: more pages ahead, none behind.
    load(cx, &f, 300, Some(1000));
    open_paging_panel(cx, &f);
    assert!(disabled(cx, &f, "grid-page-prev"));
    assert!(!disabled(cx, &f, "grid-page-next"));
    assert!(disabled(cx, &f, "grid-page-first"));
    assert!(!disabled(cx, &f, "grid-page-last"));

    // A short result is the whole table.
    load(cx, &f, 12, Some(12));
    assert!(disabled(cx, &f, "grid-page-next"));
    assert!(disabled(cx, &f, "grid-page-last"));
}

#[gpui_kit::test]
fn next_page_moves_the_offset_and_asks_for_a_reload(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 300, Some(1000));
    cx.update_window(f.window, |_, window, cx| window.click("grid-page-next", cx))
        .unwrap();
    assert_eq!(*f.events.borrow(), vec![TableTabEvent::Reload]);
    cx.update_window(f.window, |_, _, cx| {
        assert_eq!(f.view.read(cx).query().to_table_query().offset, 300);
    })
    .unwrap();

    // Back and forth.
    cx.update_window(f.window, |_, window, cx| window.click("grid-page-prev", cx))
        .unwrap();
    assert_eq!(f.events.borrow().len(), 2);
    cx.update_window(f.window, |_, _, cx| {
        assert_eq!(f.view.read(cx).query().to_table_query().offset, 0);
    })
    .unwrap();
}

#[gpui_kit::test]
fn disabled_paging_buttons_do_nothing(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 12, Some(12));
    cx.update_window(f.window, |_, window, cx| {
        window.click("grid-page-next", cx);
        window.click("grid-page-prev", cx);
    })
    .unwrap();
    assert!(f.events.borrow().is_empty());
}

#[gpui_kit::test]
fn changing_the_page_size_reloads_and_keeps_the_first_visible_row(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 300, Some(5000));
    open_paging_panel(cx, &f);
    cx.update_window(f.window, |_, window, cx| window.click("page-size-1000", cx))
        .unwrap();
    assert_eq!(*f.events.borrow(), vec![TableTabEvent::Reload]);
    cx.update_window(f.window, |_, _, cx| {
        let q = f.view.read(cx).query().to_table_query();
        assert_eq!((q.limit, q.offset), (1000, 0));
    })
    .unwrap();

    // Choosing the size already in use is not a change.
    cx.update_window(f.window, |_, window, cx| window.click("page-size-1000", cx))
        .unwrap();
    assert_eq!(f.events.borrow().len(), 1);
}

#[gpui_kit::test]
fn sorting_resets_to_the_first_page_and_reloads(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 300, Some(1000));
    cx.update_window(f.window, |_, window, cx| window.click("grid-page-next", cx))
        .unwrap();
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.toggle_sort("name", false, cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(f.events.borrow().len(), 2);
    cx.update_window(f.window, |_, _, cx| {
        let q = f.view.read(cx).query().to_table_query();
        assert_eq!(q.offset, 0);
        assert_eq!(
            q.sort,
            vec![SortKey {
                column: "name".into(),
                descending: false,
                nulls: None
            }]
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn clicking_a_header_sorts_by_that_column(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 3, Some(3));
    cx.update_window(f.window, |_, window, cx| {
        window.click("sort-name", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(*f.events.borrow(), vec![TableTabEvent::Reload]);
    cx.update_window(f.window, |_, _, cx| {
        let tab = f.view.read(cx);
        assert_eq!(
            tab.query().to_table_query().sort,
            vec![SortKey {
                column: "name".into(),
                descending: false,
                nulls: None
            }]
        );
        // The click sorts instead of selecting the column.
        assert_eq!(tab.selection_text(cx), None);
    })
    .unwrap();
}

#[gpui_kit::test]
fn clicking_a_row_number_selects_the_whole_row(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 3, Some(3));
    cx.update_window(f.window, |_, window, cx| {
        window.click("row-number-2", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(f.window, |_, _, cx| {
        let tab = f.view.read(cx);
        assert_eq!(tab.selected_row_index(cx), Some(2));
        assert_eq!(tab.selection_text(cx).as_deref(), Some("3	item 3"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn results_from_an_outdated_load_are_ignored(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            let old = v.begin_load(cx);
            let new = v.begin_load(cx); // the user changed the query before the first answered
            v.set_columns(new, columns(), cx);
            v.append_rows(new, rows(2), cx);
            // The slow, outdated query finally answers: it must not touch anything.
            v.set_columns(old, vec![], cx);
            v.append_rows(old, rows(50), cx);
            v.set_total(old, 9999, cx);
            v.finish(old, 1, cx);
            assert_eq!(v.loaded_rows(), 2);
            assert!(
                v.is_loading(),
                "only the current generation can finish the load"
            );
            v.finish(new, 3, cx);
            assert!(!v.is_loading());
        });
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(status(cx, &f), "Showing 1–2 · 3 ms");
}

#[gpui_kit::test]
fn a_failed_load_shows_the_error_and_clears_the_spinner(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            let g = v.begin_load(cx);
            v.fail(g, "no such table: items".into(), cx);
            assert!(!v.is_loading());
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("grid-error").label(),
            Some("no such table: items")
        );
        // A new load clears the error.
        f.view.update(cx, |v, cx| {
            v.begin_load(cx);
        });
        window.render_frame(cx);
        assert!(window.try_find("grid-error").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn while_loading_the_status_does_not_describe_stale_rows(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 300, Some(1000));
    cx.update_window(f.window, |_, window, cx| window.click("grid-page-next", cx))
        .unwrap();
    // The query now points at page 2 but its rows have not arrived: the old rows are still shown.
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            v.begin_load(cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(status(cx, &f), "Loading…");
}

// ---- filter drawer -----------------------------------------------------------------------------

use fg_app::model::{DraftRow, FilterDraft};
use fg_core::{Filter, FilterOp};

fn open_drawer(cx: &mut TestAppContext, f: &Fixture) {
    load(cx, f, 5, Some(5));
    cx.update_window(f.window, |_, window, cx| {
        assert!(
            window.try_find("filter-drawer").is_none(),
            "closed until asked for"
        );
        window.click("filters-toggle", cx);
        window.render_frame(cx);
        assert!(window.try_find("filter-drawer").is_some());
    })
    .unwrap();
}

fn applied_filters(cx: &mut TestAppContext, f: &Fixture) -> Vec<Filter> {
    cx.update_window(f.window, |_, _, cx| {
        f.view.read(cx).query().to_table_query().filters
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_drawer_opens_from_the_status_bar_and_toggles_closed(cx: &mut TestAppContext) {
    let f = open(cx);
    open_drawer(cx, &f);
    cx.update_window(f.window, |_, window, cx| {
        window.click("filters-toggle", cx);
        window.render_frame(cx);
        assert!(window.try_find("filter-drawer").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn adding_a_rule_typing_a_value_and_applying_filters_the_grid(cx: &mut TestAppContext) {
    let f = open(cx);
    open_drawer(cx, &f);
    cx.update_window(f.window, |_, window, cx| {
        window.click("filter-add", cx);
        window.render_frame(cx);
        // A new rule starts on the first column with "=".
        window.click("filter-0-value", cx);
        window.input("42", cx);
        window.click("filter-apply", cx);
    })
    .unwrap();

    assert_eq!(*f.events.borrow(), vec![TableTabEvent::Reload]);
    let filters = applied_filters(cx, &f);
    assert_eq!(filters.len(), 1);
    assert_eq!(
        (filters[0].column.as_str(), filters[0].op),
        ("id", FilterOp::Eq)
    );
    assert_eq!(
        filters[0].values,
        vec![Value::Int(42)],
        "typed with the column's type"
    );

    // The status bar shows how many rules are active.
    cx.update_window(f.window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("filters-toggle").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn applying_returns_to_the_first_page(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 300, Some(1000));
    cx.update_window(f.window, |_, window, cx| window.click("grid-page-next", cx))
        .unwrap();
    cx.update_window(f.window, |_, window, cx| {
        window.click("filters-toggle", cx);
        window.render_frame(cx);
        let drawer = f.view.read(cx).filter_drawer().clone();
        drawer.update(cx, |d, cx| {
            d.load(
                FilterDraft {
                    rows: vec![DraftRow {
                        value: "7".into(),
                        ..DraftRow::new("id")
                    }],
                    ..Default::default()
                },
                cx,
            )
        });
        window.render_frame(cx);
        window.click("filter-apply", cx);
    })
    .unwrap();
    cx.update_window(f.window, |_, _, cx| {
        assert_eq!(f.view.read(cx).query().to_table_query().offset, 0);
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_value_that_does_not_fit_the_column_is_reported_not_applied(cx: &mut TestAppContext) {
    let f = open(cx);
    open_drawer(cx, &f);
    cx.update_window(f.window, |_, window, cx| {
        window.click("filter-add", cx);
        window.render_frame(cx);
        window.click("filter-0-value", cx);
        window.input("abc", cx);
        window.click("filter-apply", cx);
        window.render_frame(cx);
        assert!(
            window
                .find("filter-0-error")
                .label()
                .unwrap()
                .contains("abc")
        );
    })
    .unwrap();
    assert!(f.events.borrow().is_empty(), "nothing is reloaded");
    assert!(applied_filters(cx, &f).is_empty());
}

#[gpui_kit::test]
fn raw_sql_mode_applies_a_where_expression(cx: &mut TestAppContext) {
    let f = open(cx);
    open_drawer(cx, &f);
    cx.update_window(f.window, |_, window, cx| {
        window.click("filter-raw", cx);
        window.render_frame(cx);
        assert!(window.try_find("filter-raw-input").is_some());
        window.click("filter-raw-input", cx);
        window.input("id > 3 AND name LIKE 'item%'", cx);
        window.click("filter-apply", cx);
    })
    .unwrap();
    assert_eq!(*f.events.borrow(), vec![TableTabEvent::Reload]);
    cx.update_window(f.window, |_, _, cx| {
        let q = f.view.read(cx).query().to_table_query();
        assert_eq!(q.raw_where.as_deref(), Some("id > 3 AND name LIKE 'item%'"));
        assert!(q.filters.is_empty());
    })
    .unwrap();
}

#[gpui_kit::test]
fn clear_removes_every_rule_and_reloads(cx: &mut TestAppContext) {
    let f = open(cx);
    open_drawer(cx, &f);
    cx.update_window(f.window, |_, window, cx| {
        let drawer = f.view.read(cx).filter_drawer().clone();
        drawer.update(cx, |d, cx| {
            d.load(
                FilterDraft {
                    rows: vec![DraftRow {
                        value: "1".into(),
                        ..DraftRow::new("id")
                    }],
                    ..Default::default()
                },
                cx,
            )
        });
        window.render_frame(cx);
        window.click("filter-apply", cx);
    })
    .unwrap();
    assert_eq!(applied_filters(cx, &f).len(), 1);

    cx.update_window(f.window, |_, window, cx| window.click("filter-clear", cx))
        .unwrap();
    assert!(applied_filters(cx, &f).is_empty());
    assert_eq!(f.events.borrow().len(), 2);
    cx.update_window(f.window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("filter-0-value").is_none(),
            "the rule row is gone"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_rule_can_be_removed_and_the_drawer_remembers_saved_filters(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 5, Some(5));
    // Restore a saved view: the drawer is rebuilt from it, rows included.
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let mut q = fg_app::model::GridQuery::new(ObjectPath::new(Some("main"), "items"), 300);
            q.set_filters(
                vec![Filter {
                    column: "id".into(),
                    op: FilterOp::Gt,
                    values: vec![Value::Int(2)],
                    enabled: true,
                }],
                None,
            );
            t.restore_query(q, cx);
            t.toggle_filters(cx);
        });
        window.render_frame(cx);
        assert!(window.try_find("filter-0-value").is_some());
        window.click("filter-0-remove", cx);
        window.render_frame(cx);
        assert!(window.try_find("filter-0-value").is_none());
    })
    .unwrap();
}

// ---- copying -------------------------------------------------------------------------------------

use gpui_kit::ClipboardItem;

fn clipboard(cx: &mut TestAppContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|c| c.text())
}

#[gpui_kit::test]
fn copying_a_cell_row_or_column_puts_text_on_the_clipboard(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 4, Some(4)); // ids 1..4; names: item 1, NULL, item 3, NULL
    let copy = |cx: &mut TestAppContext, f: &Fixture| {
        cx.update_window(f.window, |_, _, cx| {
            f.view.update(cx, |t, cx| t.copy_selection(cx))
        })
        .unwrap();
        clipboard(cx)
    };

    // Nothing selected: nothing happens (and the clipboard is left alone).
    cx.write_to_clipboard(ClipboardItem::new_string("before".into()));
    assert_eq!(copy(cx, &f).as_deref(), Some("before"));

    // A cell.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_cell(0, 1, cx))
    })
    .unwrap();
    assert_eq!(copy(cx, &f).as_deref(), Some("item 1"));

    // A NULL is copied as the word NULL, not as an empty cell.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_cell(1, 1, cx))
    })
    .unwrap();
    assert_eq!(copy(cx, &f).as_deref(), Some("NULL"));

    // A number.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_cell(2, 0, cx))
    })
    .unwrap();
    assert_eq!(copy(cx, &f).as_deref(), Some("3"));
}

#[gpui_kit::test]
fn selection_text_for_rows_and_columns_is_spreadsheet_friendly(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            let g = v.begin_load(cx);
            v.set_columns(g, columns(), cx);
            v.append_rows(
                g,
                vec![
                    vec![Value::Int(1), Value::Text("has\ttab".into())],
                    vec![Value::Int(2), Value::Text("plain".into())],
                ],
                cx,
            );
            v.finish(g, 1, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    let text = |cx: &mut TestAppContext, f: &Fixture| {
        cx.update_window(f.window, |_, _, cx| f.view.read(cx).selection_text(cx))
            .unwrap()
    };

    // A cell containing a tab is returned as is; a row quotes it so a paste keeps it in one cell.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_cell(0, 1, cx))
    })
    .unwrap();
    assert_eq!(text(cx, &f).as_deref(), Some("has\ttab"));

    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_row(0, cx))
    })
    .unwrap();
    assert_eq!(text(cx, &f).as_deref(), Some("1\t\"has\ttab\""));
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_row(1, cx))
    })
    .unwrap();
    assert_eq!(text(cx, &f).as_deref(), Some("2\tplain"));

    // A column: one value per line, for this page.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.select_column(0, cx))
    })
    .unwrap();
    assert_eq!(text(cx, &f).as_deref(), Some("1\n2"));
}

// ---- foreign keys ------------------------------------------------------------------------------

use fg_app::model::JumpTarget;

fn authors_target() -> JumpTarget {
    JumpTarget {
        column: "name".into(),
        table: ObjectPath::new(Some("main"), "authors"),
        referenced_column: "author_name".into(),
    }
}

#[gpui_kit::test]
fn followable_cells_have_an_arrow_that_asks_to_open_the_referenced_row(cx: &mut TestAppContext) {
    let f = open(cx);
    cx.update_window(f.window, |_, _, cx| {
        f.view
            .update(cx, |t, cx| t.set_foreign_keys(vec![authors_target()], cx))
    })
    .unwrap();
    load(cx, &f, 3, Some(3)); // names: "item 1", NULL, "item 3"

    let events = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    cx.update(|cx| {
        cx.subscribe(&f.view, move |_, e: &TableTabEvent, _| {
            log.borrow_mut().push(e.clone())
        })
        .detach();
    });

    cx.update_window(f.window, |_, window, cx| {
        assert!(
            window.try_find("fk-0-1").is_some(),
            "a value can be followed"
        );
        assert!(
            window.try_find("fk-1-1").is_none(),
            "a NULL has nothing to follow"
        );
        assert!(
            window.try_find("fk-0-0").is_none(),
            "only foreign-key columns get arrows"
        );
        window.click("fk-0-1", cx);
    })
    .unwrap();

    let got = events.borrow().clone();
    assert_eq!(
        got,
        vec![TableTabEvent::FollowForeignKey {
            table: ObjectPath::new(Some("main"), "authors"),
            filter: Filter {
                column: "author_name".into(),
                op: FilterOp::Eq,
                values: vec![Value::Text("item 1".into())],
                enabled: true,
            },
        }]
    );
}

#[gpui_kit::test]
fn the_header_marks_foreign_key_columns(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 1, None);
    cx.update_window(f.window, |_, _, cx| {
        assert!(!f.view.read(cx).has_foreign_key("name", cx));
        f.view
            .update(cx, |t, cx| t.set_foreign_keys(vec![authors_target()], cx));
        assert!(f.view.read(cx).has_foreign_key("name", cx));
        assert!(!f.view.read(cx).has_foreign_key("id", cx));
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_jump_wins_over_saved_filters_whichever_arrives_first(cx: &mut TestAppContext) {
    let jump = Filter {
        column: "id".into(),
        op: FilterOp::Eq,
        values: vec![Value::Int(7)],
        enabled: true,
    };
    let saved = |path: ObjectPath| {
        let mut q = fg_app::model::GridQuery::new(path, 300);
        q.set_filters(
            vec![Filter {
                column: "name".into(),
                op: FilterOp::IsNull,
                values: vec![],
                enabled: true,
            }],
            None,
        );
        q
    };

    // A new tab: the jump arrives before the controller has restored the saved view state.
    let f = open(cx);
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| {
            t.apply_filter(jump.clone(), cx);
            t.restore_query(saved(ObjectPath::new(Some("main"), "items")), cx);
        });
        assert_eq!(f.view.read(cx).query().filters, vec![jump.clone()]);
    })
    .unwrap();

    // An already open tab: the filter replaces its rules and the grid reloads.
    let g = open(cx);
    cx.update_window(g.window, |_, _, cx| {
        g.view.update(cx, |t, cx| {
            t.restore_query(saved(ObjectPath::new(Some("main"), "items")), cx)
        });
        g.view.update(cx, |t, cx| t.apply_filter(jump.clone(), cx));
        assert_eq!(g.view.read(cx).query().filters, vec![jump.clone()]);
    })
    .unwrap();
    assert_eq!(*g.events.borrow(), vec![TableTabEvent::Reload]);
}

// ---- column visibility -------------------------------------------------------------------------

fn counts(cx: &mut TestAppContext, f: &Fixture) -> (usize, usize) {
    cx.update_window(f.window, |_, _, cx| f.view.read(cx).column_counts(cx))
        .unwrap()
}

#[gpui_kit::test]
fn columns_can_be_hidden_and_shown_from_the_panel(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 3, Some(3));
    assert_eq!(counts(cx, &f), (2, 2));

    cx.update_window(f.window, |_, window, cx| {
        assert!(window.try_find("columns-panel").is_none());
        window.click("columns-toggle", cx);
        window.render_frame(cx);
        assert!(window.try_find("columns-panel").is_some());
        window.click("col-visible-name", cx);
    })
    .unwrap();
    assert_eq!(counts(cx, &f), (1, 2));
    assert_eq!(*f.events.borrow(), vec![TableTabEvent::ViewChanged]);
    cx.update_window(f.window, |_, _, cx| {
        assert_eq!(f.view.read(cx).hidden_columns(cx), vec!["name".to_string()]);
    })
    .unwrap();

    // The last visible column cannot be hidden: a grid with no columns is useless.
    cx.update_window(f.window, |_, window, cx| window.click("col-visible-id", cx))
        .unwrap();
    assert_eq!(counts(cx, &f), (1, 2));
    assert_eq!(
        f.events.borrow().len(),
        1,
        "nothing changed, nothing announced"
    );

    cx.update_window(f.window, |_, window, cx| {
        window.click("columns-show-all", cx)
    })
    .unwrap();
    assert_eq!(counts(cx, &f), (2, 2));
    assert_eq!(f.events.borrow().len(), 2);
}

#[gpui_kit::test]
fn hidden_columns_survive_a_reload_and_are_saved_with_the_view(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 3, Some(3));
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| t.toggle_column("name", cx))
    })
    .unwrap();

    // A new result with the same columns keeps them hidden.
    load(cx, &f, 5, Some(5));
    assert_eq!(counts(cx, &f), (1, 2));

    let state = cx
        .update_window(f.window, |_, _, cx| f.view.read(cx).view_state(cx))
        .unwrap();
    assert_eq!(state.hidden_columns, vec!["name".to_string()]);

    // Restoring is silent: it is not a user change.
    let g = open(cx);
    load(cx, &g, 2, Some(2));
    cx.update_window(g.window, |_, _, cx| {
        g.view.update(cx, |t, cx| {
            t.restore_hidden_columns(state.hidden_columns.clone(), cx)
        })
    })
    .unwrap();
    assert_eq!(counts(cx, &g), (1, 2));
    assert!(g.events.borrow().is_empty());
}

#[gpui_kit::test]
fn copying_a_row_leaves_hidden_columns_out(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 2, Some(2)); // row 0: 1, "item 1"
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |t, cx| {
            t.toggle_column("id", cx);
            t.select_row(0, cx);
        });
        assert_eq!(
            f.view.read(cx).selection_text(cx).as_deref(),
            Some("item 1")
        );
        // Selecting a cell counts columns as shown: column 0 on screen is now "name".
        f.view.update(cx, |t, cx| t.select_cell(0, 0, cx));
        assert_eq!(
            f.view.read(cx).selection_text(cx).as_deref(),
            Some("item 1")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn shift_clicking_a_second_header_adds_it_to_the_sort(cx: &mut TestAppContext) {
    let f = open(cx);
    load(cx, &f, 3, Some(3));
    cx.update_window(f.window, |_, window, cx| {
        window.click("sort-name", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(f.window, |_, window, cx| {
        window.click_with_modifiers(
            "sort-id",
            gpui_kit::Modifiers {
                shift: true,
                ..Default::default()
            },
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(f.window, |_, _, cx| {
        let tab = f.view.read(cx);
        assert_eq!(
            tab.query().to_table_query().sort,
            vec![
                SortKey {
                    column: "name".into(),
                    descending: false,
                    nulls: None
                },
                SortKey {
                    column: "id".into(),
                    descending: false,
                    nulls: None
                },
            ],
            "Shift-click keeps the first key and appends the second"
        );
    })
    .unwrap();
}
