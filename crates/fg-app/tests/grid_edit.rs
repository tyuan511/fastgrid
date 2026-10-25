//! Staged edits in the data grid: painting, in-place editing, and `⌘S`, through a real headless
//! window. The commit itself (SQL, transaction) is covered in `fg-service`; here the grid hands
//! its changes to the owner and reacts to the answer.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use fg_app::model::Counts;
use fg_app::views::{CellMark, TableTab, TableTabEvent};
use fg_core::{ColumnMeta, DataType, DbKind, ObjectPath, TypeCategory, Value};
use fg_sql::{CellChange, RowChange};
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

fn open(cx: &mut TestAppContext, primary_key: &[&str]) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        fg_app::views::bind_keys(cx);
    });
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
    let f = Fixture {
        window,
        view,
        events,
    };
    let keys: Vec<String> = primary_key.iter().map(|k| (*k).to_owned()).collect();
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| {
            v.set_primary_key(keys, cx);
            v.set_engine(DbKind::Sqlite, cx);
        });
    })
    .unwrap();
    load(cx, &f);
    f
}

fn columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
        ColumnMeta::new("name", DataType::new("VARCHAR(100)", TypeCategory::Text)),
    ]
}

fn load(cx: &mut TestAppContext, f: &Fixture) {
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            let g = v.begin_load(cx);
            v.set_columns(g, columns(), cx);
            v.append_rows(
                g,
                (1..=3)
                    .map(|i| vec![Value::Int(i), Value::Text(format!("item {i}"))])
                    .collect(),
                cx,
            );
            v.finish(g, 1, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
}

fn press(cx: &mut TestAppContext, f: &Fixture, keys: &str) {
    cx.update_window(f.window, |_, window, cx| {
        window.press(keys, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn select(cx: &mut TestAppContext, f: &Fixture, row: usize, col: usize) {
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.select_cell(row, col, cx));
        window.render_frame(cx);
    })
    .unwrap();
}

fn begin_edit(cx: &mut TestAppContext, f: &Fixture, row: usize, col: usize) {
    select(cx, f, row, col);
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.begin_edit(window, cx));
        window.render_frame(cx);
    })
    .unwrap();
}

fn type_in_editor(cx: &mut TestAppContext, f: &Fixture, text: &str) {
    let input = cx.read_entity(&f.view, |v, cx| v.editor_input(cx)).unwrap();
    let text = text.to_owned();
    cx.update_window(f.window, |_, window, cx| {
        input.update(cx, |s, cx| s.set_value(text, window, cx));
        window.render_frame(cx);
    })
    .unwrap();
}

fn pending(cx: &mut TestAppContext, f: &Fixture) -> Counts {
    cx.read_entity(&f.view, |v, cx| v.pending(cx))
}

fn mark(cx: &mut TestAppContext, f: &Fixture, row: usize, col: usize) -> CellMark {
    cx.read_entity(&f.view, |v, cx| v.mark_at(row, col, cx))
}

fn shown(cx: &mut TestAppContext, f: &Fixture, row: usize, col: usize) -> Option<Value> {
    cx.read_entity(&f.view, |v, cx| v.shown_value_at(row, col, cx))
}

fn editing(cx: &mut TestAppContext, f: &Fixture) -> bool {
    cx.read_entity(&f.view, |v, cx| v.is_editing(cx))
}

fn drain(cx: &mut TestAppContext, f: &Fixture) -> Vec<TableTabEvent> {
    cx.update(|_| {});
    std::mem::take(&mut *f.events.borrow_mut())
}

fn error_text(cx: &mut TestAppContext, f: &Fixture) -> Option<String> {
    cx.update_window(f.window, |_, window, _| {
        window
            .try_find("grid-error")
            .and_then(|e| e.label().map(str::to_owned))
    })
    .unwrap()
}

#[gpui_kit::test]
fn enter_stages_an_edit_and_the_cell_is_painted(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    assert_eq!(mark(cx, &f, 0, 1), CellMark::None);

    begin_edit(cx, &f, 0, 1);
    assert!(editing(cx, &f));
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.editor_text(cx))
            .as_deref(),
        Some("item 1"),
        "the editor starts with the cell's text"
    );
    type_in_editor(cx, &f, "renamed");
    press(cx, &f, "enter");

    assert!(!editing(cx, &f), "Enter closes the editor");
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Text("renamed".into())));
    assert_eq!(mark(cx, &f, 0, 1), CellMark::Modified);
    assert_eq!(mark(cx, &f, 0, 0), CellMark::None, "only the edited cell");
    assert_eq!(mark(cx, &f, 1, 1), CellMark::None);
    assert_eq!(pending(cx, &f).modified_rows, 1);
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.changes(cx)),
        [RowChange::Update {
            key: vec![("id".into(), Value::Int(1))],
            cells: vec![(
                "name".into(),
                CellChange::Set(Value::Text("renamed".into()))
            )],
        }]
    );
}

#[gpui_kit::test]
fn escape_closes_the_editor_without_staging(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "nope");
    press(cx, &f, "escape");
    assert!(!editing(cx, &f));
    assert_eq!(pending(cx, &f).total(), 0);
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Text("item 1".into())));
}

#[gpui_kit::test]
fn a_value_that_does_not_fit_the_column_stays_in_the_editor(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 0);
    type_in_editor(cx, &f, "abc");
    press(cx, &f, "enter");
    assert!(editing(cx, &f), "the editor stays open");
    assert_eq!(pending(cx, &f).total(), 0);
    assert!(
        error_text(cx, &f).unwrap().contains("not a whole number"),
        "the reason is shown"
    );

    type_in_editor(cx, &f, "10");
    press(cx, &f, "enter");
    assert!(!editing(cx, &f));
    assert_eq!(shown(cx, &f, 0, 0), Some(Value::Int(10)));
    assert!(error_text(cx, &f).is_none(), "the message goes away");
}

#[gpui_kit::test]
fn the_word_null_in_caps_sets_null_and_other_text_is_literal(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "NULL");
    press(cx, &f, "enter");
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Null));

    begin_edit(cx, &f, 1, 1);
    type_in_editor(cx, &f, "null");
    press(cx, &f, "enter");
    assert_eq!(shown(cx, &f, 1, 1), Some(Value::Text("null".into())));
}

#[gpui_kit::test]
fn typing_the_original_value_back_unstages_the_cell(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "changed");
    press(cx, &f, "enter");
    assert_eq!(pending(cx, &f).total(), 1);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "item 1");
    press(cx, &f, "enter");
    assert_eq!(pending(cx, &f).total(), 0);
    assert_eq!(mark(cx, &f, 0, 1), CellMark::None);
}

#[gpui_kit::test]
fn a_table_without_a_primary_key_cannot_be_edited(cx: &mut TestAppContext) {
    let f = open(cx, &[]);
    assert!(
        cx.read_entity(&f.view, |v, cx| v.read_only_reason(cx))
            .is_some()
    );
    begin_edit(cx, &f, 0, 1);
    assert!(!editing(cx, &f));
    assert!(error_text(cx, &f).unwrap().contains("no primary key"));
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| v.insert_row(cx))
    })
    .unwrap();
    assert_eq!(pending(cx, &f).total(), 0);
}

#[gpui_kit::test]
fn deleted_and_added_rows_are_marked(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    select(cx, &f, 1, 0);
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| v.toggle_delete_row(cx))
    })
    .unwrap();
    assert_eq!(mark(cx, &f, 1, 0), CellMark::Deleted);
    assert_eq!(mark(cx, &f, 1, 1), CellMark::Deleted, "the whole row");
    assert_eq!(mark(cx, &f, 0, 0), CellMark::None);

    // A second toggle brings it back.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| v.toggle_delete_row(cx))
    })
    .unwrap();
    assert_eq!(mark(cx, &f, 1, 0), CellMark::None);

    // A new row shows after the loaded ones, with defaults until edited.
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| v.insert_row(cx))
    })
    .unwrap();
    assert_eq!(cx.read_entity(&f.view, |v, cx| v.total_rows(cx)), 4);
    assert_eq!(mark(cx, &f, 3, 0), CellMark::Inserted);
    assert!(
        matches!(shown(cx, &f, 3, 1), Some(Value::Other { ref text, .. }) if text == "DEFAULT")
    );
    begin_edit(cx, &f, 3, 1);
    type_in_editor(cx, &f, "fresh");
    press(cx, &f, "enter");
    assert_eq!(shown(cx, &f, 3, 1), Some(Value::Text("fresh".into())));
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.changes(cx)),
        [RowChange::Insert {
            cells: vec![("name".into(), CellChange::Set(Value::Text("fresh".into())))]
        }]
    );
}

#[gpui_kit::test]
fn undo_and_redo_step_through_the_edits(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "x");
    press(cx, &f, "enter");
    assert_eq!(pending(cx, &f).total(), 1);
    press(cx, &f, "secondary-z");
    assert_eq!(pending(cx, &f).total(), 0);
    press(cx, &f, "secondary-shift-z");
    assert_eq!(pending(cx, &f).total(), 1);
}

#[gpui_kit::test]
fn staged_edits_survive_a_reload(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "kept");
    press(cx, &f, "enter");
    // Paging or sorting reloads the same table: the edit is keyed by primary key.
    load(cx, &f);
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Text("kept".into())));
    assert_eq!(pending(cx, &f).modified_rows, 1);
}

#[gpui_kit::test]
fn cmd_s_asks_the_owner_to_commit_once_and_reacts_to_the_answer(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    // Nothing staged: nothing to commit.
    press(cx, &f, "secondary-s");
    assert!(drain(cx, &f).is_empty());

    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "saved");
    // `⌘S` with the editor still open stages it first, so what is on screen is what is committed.
    press(cx, &f, "secondary-s");
    assert_eq!(drain(cx, &f), [TableTabEvent::CommitRequested]);
    assert!(!editing(cx, &f));
    assert!(cx.read_entity(&f.view, |v, _| v.is_committing()));
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.changes(cx)).len(),
        1,
        "the open edit was included"
    );

    // A second press while the commit runs is ignored.
    press(cx, &f, "secondary-s");
    assert!(drain(cx, &f).is_empty());

    // Failure: the changes stay, with the reason.
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| {
            v.commit_finished(Err("change 1: boom".into()), cx)
        });
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(pending(cx, &f).total(), 1, "nothing typed is lost");
    assert!(error_text(cx, &f).unwrap().contains("boom"));
    assert!(!cx.read_entity(&f.view, |v, _| v.is_committing()));

    // Success: staged changes are gone and the page is asked to reload.
    press(cx, &f, "secondary-s");
    assert_eq!(drain(cx, &f), [TableTabEvent::CommitRequested]);
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.commit_finished(Ok(()), cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(pending(cx, &f).total(), 0);
    assert_eq!(drain(cx, &f), [TableTabEvent::Reload]);
}

#[gpui_kit::test]
fn the_commit_and_discard_buttons_follow_the_pending_count(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    let exists = |cx: &mut TestAppContext, id: &'static str| {
        cx.update_window(f.window, |_, window, _| window.try_find(id).is_some())
            .unwrap()
    };
    assert!(!exists(cx, "commit-changes"));
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "y");
    press(cx, &f, "enter");
    assert!(exists(cx, "commit-changes"), "Commit (1) appears");
    assert!(exists(cx, "discard-changes"));
    cx.update_window(f.window, |_, window, cx| {
        window.click("discard-changes", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(pending(cx, &f).total(), 0);
    assert!(!exists(cx, "commit-changes"));
}

fn label(cx: &mut TestAppContext, f: &Fixture, id: &'static str) -> Option<String> {
    cx.update_window(f.window, |_, window, _| {
        window
            .try_find(id)
            .and_then(|e| e.label().map(str::to_owned))
    })
    .unwrap()
}

/// Give the grid the keyboard, as a click on a cell does.
fn focus_grid(cx: &mut TestAppContext, f: &Fixture) {
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |v, cx| v.focus_grid(window, cx));
        window.render_frame(cx);
    })
    .unwrap();
}

fn selected(cx: &mut TestAppContext, f: &Fixture) -> Option<(usize, usize)> {
    cx.read_entity(&f.view, |v, cx| v.selected_cell(cx))
}

#[gpui_kit::test]
fn typing_on_a_selected_cell_starts_editing_with_what_was_typed(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    select(cx, &f, 0, 1);
    focus_grid(cx, &f);
    press(cx, &f, "z");
    assert!(editing(cx, &f), "a printable key opens the editor");
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.editor_text(cx))
            .as_deref(),
        Some("z"),
        "it replaces the old value, and the key is not typed twice"
    );
    press(cx, &f, "enter");
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Text("z".into())));
}

#[gpui_kit::test]
fn shortcuts_and_space_do_not_start_an_edit(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    select(cx, &f, 0, 1);
    focus_grid(cx, &f);
    press(cx, &f, "space");
    press(cx, &f, "secondary-c");
    assert!(!editing(cx, &f));
}

#[gpui_kit::test]
fn tab_enter_and_shift_tab_confirm_and_move(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    // Tab: confirm, move right.
    begin_edit(cx, &f, 0, 0);
    type_in_editor(cx, &f, "11");
    press(cx, &f, "tab");
    assert_eq!(shown(cx, &f, 0, 0), Some(Value::Int(11)));
    assert_eq!(selected(cx, &f), Some((0, 1)));
    assert!(!editing(cx, &f));

    // Tab at the end of a row wraps to the start of the next.
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "n1");
    press(cx, &f, "tab");
    assert_eq!(selected(cx, &f), Some((1, 0)));

    // Shift+Tab goes back, wrapping to the end of the previous row.
    begin_edit(cx, &f, 1, 0);
    type_in_editor(cx, &f, "22");
    press(cx, &f, "shift-tab");
    assert_eq!(selected(cx, &f), Some((0, 1)));

    // Enter: confirm, move down.
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "n2");
    press(cx, &f, "enter");
    assert_eq!(selected(cx, &f), Some((1, 1)));

    // A rejected value neither stages nor moves.
    begin_edit(cx, &f, 1, 0);
    type_in_editor(cx, &f, "x");
    press(cx, &f, "tab");
    assert!(editing(cx, &f));
    assert_eq!(selected(cx, &f), Some((1, 0)));
}

#[gpui_kit::test]
fn the_last_cell_stays_put_instead_of_leaving_the_grid(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 2, 1);
    type_in_editor(cx, &f, "last");
    press(cx, &f, "tab");
    assert_eq!(shown(cx, &f, 2, 1), Some(Value::Text("last".into())));
    assert_eq!(selected(cx, &f), Some((2, 1)));
}

#[gpui_kit::test]
fn clicking_away_keeps_what_was_typed(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "kept on blur");
    // A click on another cell moves the selection.
    select(cx, &f, 1, 1);
    assert!(!editing(cx, &f));
    assert_eq!(
        shown(cx, &f, 0, 1),
        Some(Value::Text("kept on blur".into()))
    );
}

#[gpui_kit::test]
fn clicking_away_from_a_bad_value_says_it_was_not_saved(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 0);
    type_in_editor(cx, &f, "nope");
    select(cx, &f, 1, 1);
    assert!(!editing(cx, &f), "no stale box is left on screen");
    assert_eq!(shown(cx, &f, 0, 0), Some(Value::Int(1)));
    assert!(error_text(cx, &f).unwrap().starts_with("Not saved"));
}

#[gpui_kit::test]
fn unsaved_changes_are_announced_in_a_bar_with_save_and_discard(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    assert!(
        label(cx, &f, "pending-bar").is_none(),
        "nothing to announce yet"
    );

    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "a");
    press(cx, &f, "enter");
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| {
            v.select_row(1, cx);
            v.toggle_delete_row(cx);
            v.insert_row(cx);
        })
    })
    .unwrap();
    cx.update_window(f.window, |_, window, _| window.refresh())
        .unwrap();
    cx.update_window(f.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let bar = label(cx, &f, "pending-bar").expect("the bar appears with the first change");
    // The wording follows the system language; the numbers are what matters here.
    let english = bar.contains("3 unsaved changes")
        && bar.contains("1 edited")
        && bar.contains("1 new")
        && bar.contains("1 deleted");
    let chinese = bar.contains("3 项未保存的更改")
        && bar.contains("1 行已修改")
        && bar.contains("1 行新增")
        && bar.contains("1 行待删除");
    assert!(english || chinese, "{bar}");
}

#[gpui_kit::test]
fn the_bar_offers_a_working_save_button(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "a");
    press(cx, &f, "enter");
    cx.update_window(f.window, |_, window, cx| {
        window.click("commit-changes", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(drain(cx, &f), [TableTabEvent::CommitRequested]);
    // While it runs the button says so and cannot be pressed again.
    assert!(cx.read_entity(&f.view, |v, _| v.is_committing()));
}

#[gpui_kit::test]
fn the_sql_preview_shows_the_statements_with_values_written_in(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    begin_edit(cx, &f, 0, 1);
    type_in_editor(cx, &f, "O'Neil");
    press(cx, &f, "enter");
    select(cx, &f, 1, 0);
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| v.toggle_delete_row(cx))
    })
    .unwrap();
    assert!(
        label(cx, &f, "preview-panel").is_none(),
        "closed until asked for"
    );

    press(cx, &f, "secondary-shift-p");
    cx.update_window(f.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let panel = label(cx, &f, "preview-panel").expect("the panel opens");
    assert_eq!(
        panel,
        "DELETE FROM \"main\".\"items\" WHERE \"id\" = 2;\n\
         UPDATE \"main\".\"items\" SET \"name\" = 'O''Neil' WHERE \"id\" = 1;"
    );

    // The panel goes away with the changes it described.
    cx.update_window(f.window, |_, window, cx| {
        window.click("discard-changes", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(label(cx, &f, "preview-panel").is_none());
    assert!(label(cx, &f, "pending-bar").is_none());
}

#[gpui_kit::test]
fn cloning_a_row_copies_it_without_its_key(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    select(cx, &f, 1, 0);
    focus_grid(cx, &f);
    press(cx, &f, "secondary-d");

    assert_eq!(cx.read_entity(&f.view, |v, cx| v.total_rows(cx)), 4);
    assert_eq!(mark(cx, &f, 3, 1), CellMark::Inserted);
    assert_eq!(shown(cx, &f, 3, 1), Some(Value::Text("item 2".into())));
    // The key column is cleared, so the copy takes its default on commit.
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.changes(cx)),
        [RowChange::Insert {
            cells: vec![("name".into(), CellChange::Set(Value::Text("item 2".into())))]
        }]
    );
    assert_eq!(pending(cx, &f).inserted_rows, 1);
}

#[gpui_kit::test]
fn set_null_empty_default_and_revert_act_on_the_selected_cell(cx: &mut TestAppContext) {
    let f = open(cx, &["id"]);
    macro_rules! act {
        ($method:ident) => {
            cx.update_window(f.window, |_, _, cx| {
                f.view.update(cx, |v, cx| v.$method(cx));
            })
            .unwrap()
        };
    }

    select(cx, &f, 0, 1);
    act!(set_selected_cell_to_null);
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Null));
    assert_eq!(mark(cx, &f, 0, 1), CellMark::Modified);

    act!(revert_selected_cell);
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Text("item 1".into())));
    assert_eq!(mark(cx, &f, 0, 1), CellMark::None);
    assert_eq!(pending(cx, &f).total(), 0);

    act!(set_selected_cell_to_empty);
    assert_eq!(shown(cx, &f, 0, 1), Some(Value::Text(String::new())));

    act!(set_selected_cell_to_default);
    assert!(
        matches!(shown(cx, &f, 0, 1), Some(Value::Other { ref text, .. }) if text == "DEFAULT")
    );
    assert_eq!(
        cx.read_entity(&f.view, |v, cx| v.changes(cx)),
        [RowChange::Update {
            key: vec![("id".into(), Value::Int(1))],
            cells: vec![("name".into(), CellChange::Default)],
        }]
    );

    // Empty is refused on a non-text column instead of staging a bad value.
    select(cx, &f, 0, 0);
    act!(set_selected_cell_to_empty);
    assert_eq!(shown(cx, &f, 0, 0), Some(Value::Int(1)));
    assert!(error_text(cx, &f).unwrap().contains("text columns"));
}
