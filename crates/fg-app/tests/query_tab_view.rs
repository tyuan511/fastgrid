//! The query tab driven through a real headless window: the editor, run intents, refusals and the
//! result panel. No database: the run results are pushed in exactly as the controller pushes them,
//! so this tests the view's behaviour, not the service's (that is `query_tab_e2e.rs`).
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use fg_app::model::{QueryDocumentState, RunMode};
use fg_app::views::{QueryTab, QueryTabEvent};
// No `use gpui_kit::*`: it would import `gpui_kit::test`, which then shadows the built-in
// `#[test]` that `#[gpui_kit::test]` expands to.
use fg_core::{
    ColumnMeta, ConnectionId, DangerReason, DataType, DbKind, QueryError, SafeMode, TypeCategory,
    UnixMillis, Value,
};
use fg_driver::Event;
use fg_service::{PolicyRefusal, plan};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, SharedString, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};

struct Fixture {
    window: AnyWindowHandle,
    view: Entity<QueryTab>,
    events: Rc<RefCell<Vec<QueryTabEvent>>>,
}

fn document() -> QueryDocumentState {
    QueryDocumentState::new(ConnectionId::new(), "Query", UnixMillis(1_700_000_000_000))
}

/// Open a tab holding `script`, with the run intents recorded.
fn open(cx: &mut TestAppContext, script: &str) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        fg_app::views::bind_keys(cx);
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    let script = script.to_owned();
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
            |window, cx| cx.new(|cx| QueryTab::new(document(), DbKind::Sqlite, script, window, cx)),
        )
        .unwrap()
    });
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &QueryTabEvent, _| {
            log.borrow_mut().push(e.clone())
        })
        .detach();
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let _ = view;
    })
    .unwrap();
    Fixture {
        window,
        view,
        events,
    }
}

/// Focus the editor and type. Keystrokes need an observed focus, hence the click first.
fn type_into(cx: &mut TestAppContext, f: &Fixture, text: &str) {
    let editor = cx.read_entity(&f.view, |t, _| t.editor_state().clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        window.input(text, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

/// Put the caret at `offset` (and focus the editor), without changing the text.
fn place_caret(cx: &mut TestAppContext, f: &Fixture, offset: usize) {
    let editor = cx.read_entity(&f.view, |t, _| t.editor_state().clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        editor.update(cx, |editor, cx| {
            editor.set_selected_range(offset..offset, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
}

fn select(cx: &mut TestAppContext, f: &Fixture, range: Range<usize>) {
    let editor = cx.read_entity(&f.view, |t, _| t.editor_state().clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        editor.update(cx, |editor, cx| editor.set_selected_range(range, cx));
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

fn drain(cx: &mut TestAppContext, f: &Fixture) -> Vec<QueryTabEvent> {
    // The subscription runs after the update that emitted the event, so read it in its own turn.
    cx.update(|_| {});
    std::mem::take(&mut *f.events.borrow_mut())
}

fn target_sql(cx: &mut TestAppContext, f: &Fixture, mode: RunMode) -> Option<String> {
    cx.read_entity(&f.view, |t, cx| {
        t.target(mode, cx).map(|target| target.sql().to_owned())
    })
}

fn columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
        ColumnMeta::new("name", DataType::new("TEXT", TypeCategory::Text)),
    ]
}

fn rows(n: i64) -> Vec<Vec<Value>> {
    (1..=n)
        .map(|i| vec![Value::Int(i), Value::Text(format!("row {i}"))])
        .collect()
}

fn end(truncated: bool) -> Event {
    Event::ResultEnd {
        affected_rows: Some(0),
        last_insert_id: None,
        truncated,
        elapsed_ms: 3,
        warnings: vec![],
    }
}

/// Feed a run the way the controller does.
fn feed(cx: &mut TestAppContext, f: &Fixture, events: Vec<Event>) {
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t
                .target(RunMode::All, cx)
                .expect("a target for the whole buffer");
            let script = t.script(cx);
            let generation = t.begin_run(target, script, cx);
            for event in events {
                t.apply_event(generation, event, cx);
            }
            t.finish_run(generation, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
}

fn status(cx: &mut TestAppContext, f: &Fixture) -> String {
    cx.update_window(f.window, |_, window, _| {
        window.find("run-status").label().unwrap().to_owned()
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_editor_shows_the_script_and_typing_marks_the_draft_dirty(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;");
    let editor = cx.read_entity(&f.view, |t, _| t.editor_state().clone());
    cx.read_entity(&editor, |editor, _| {
        assert_eq!(editor.value().as_ref(), "SELECT 1;");
    });
    assert!(!cx.read_entity(&f.view, |t, _| t.is_dirty()));

    type_into(cx, &f, " ");
    assert!(cx.read_entity(&f.view, |t, _| t.is_dirty()));
    assert!(
        cx.update_window(f.window, |_, window, _| window
            .try_find("draft-dirty")
            .is_some())
            .unwrap(),
        "an unsaved draft says so"
    );
}

#[gpui_kit::test]
fn typing_asks_the_controller_to_persist_the_draft(cx: &mut TestAppContext) {
    let f = open(cx, "");
    type_into(cx, &f, "SELECT 2;");
    let events = drain(cx, &f);
    assert!(
        events.contains(&QueryTabEvent::DraftChanged),
        "every edit asks for a save: {events:?}"
    );
}

#[gpui_kit::test]
fn cmd_enter_runs_the_statement_under_the_caret(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;\nSELECT 2;");
    place_caret(cx, &f, 18); // inside `SELECT 2`
    press(cx, &f, "secondary-enter");

    let events = drain(cx, &f);
    assert_eq!(
        events,
        vec![QueryTabEvent::Run {
            mode: RunMode::CurrentStatement
        }]
    );
    assert_eq!(
        target_sql(cx, &f, RunMode::CurrentStatement).as_deref(),
        Some("SELECT 2"),
        "only the statement the caret is in"
    );
}

#[gpui_kit::test]
fn cmd_shift_enter_runs_the_whole_script(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;\nSELECT 2;");
    place_caret(cx, &f, 0);
    press(cx, &f, "secondary-shift-enter");
    assert_eq!(
        drain(cx, &f),
        vec![QueryTabEvent::Run { mode: RunMode::All }]
    );
    assert_eq!(
        target_sql(cx, &f, RunMode::All).as_deref(),
        Some("SELECT 1;\nSELECT 2;")
    );
}

#[gpui_kit::test]
fn a_selection_runs_what_is_selected(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;\nSELECT 2;");
    select(cx, &f, 0..8);
    assert_eq!(
        target_sql(cx, &f, RunMode::Selection).as_deref(),
        Some("SELECT 1"),
        "the selection wins over the caret"
    );
}

#[gpui_kit::test]
fn cmd_period_cancels_the_running_statement(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;");
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("a target");
            t.begin_run(target, t.script(cx), cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    // The cancel key is scoped to the editor, so it needs the editor focused.
    place_caret(cx, &f, 0);
    press(cx, &f, "secondary-.");
    assert_eq!(drain(cx, &f), vec![QueryTabEvent::Cancel]);
    assert!(
        status(cx, &f).starts_with("Cancelled"),
        "the status bar says it was cancelled: {}",
        status(cx, &f)
    );
}

#[gpui_kit::test]
fn a_refusal_is_shown_and_reports_nothing_ran(cx: &mut TestAppContext) {
    let f = open(cx, "DELETE FROM items;");
    let script = cx.read_entity(&f.view, |t, cx| t.script(cx));
    let plan = plan(&script, DbKind::Sqlite, SafeMode::ReadOnly);
    let refusal = plan.refusal.expect("a read-only refusal");

    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("a target");
            let generation = t.begin_run(target, script.clone(), cx);
            t.apply_plan(generation, &plan, cx);
            t.refuse(generation, &plan, refusal, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();

    let shown = cx
        .update_window(f.window, |_, window, _| {
            window.find("run-refused").label().unwrap().to_owned()
        })
        .unwrap();
    assert!(
        shown.contains("read-only"),
        "the refusal explains why: {shown}"
    );
    assert_eq!(
        status(cx, &f),
        "Nothing ran.",
        "a refusal says nothing ran rather than counting rows"
    );
    assert_eq!(
        cx.read_entity(&f.view, |t, _| t.results().sets().len()),
        0,
        "and no result set was created"
    );
}

#[gpui_kit::test]
fn a_dangerous_statement_asks_before_it_runs(cx: &mut TestAppContext) {
    let f = open(cx, "DELETE FROM items;");
    let script = cx.read_entity(&f.view, |t, cx| t.script(cx));
    let plan = plan(&script, DbKind::Sqlite, SafeMode::Alert);
    let (statement, danger) = match plan.refusal {
        Some(PolicyRefusal::NeedsConfirmation {
            statement, danger, ..
        }) => (
            statement,
            danger.expect("a delete without WHERE is dangerous"),
        ),
        other => panic!("expected a confirmation demand, got {other:?}"),
    };
    let shown_statement = plan
        .statements
        .get(statement)
        .map(|s| s.sql.clone())
        .unwrap_or_default();
    assert_eq!(shown_statement, "DELETE FROM items");

    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("a target");
            t.begin_run(target.clone(), script.clone(), cx);
            t.ask_confirmation(
                fg_app::views::PendingConfirmation {
                    script: script.clone(),
                    statement: shown_statement.clone(),
                    target,
                    reason: Some(danger),
                    production: true,
                },
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();

    let prompt = cx
        .update_window(f.window, |_, window, _| {
            (
                window.try_find("confirm-run").is_some(),
                window.find("confirm-statement").label().unwrap().to_owned(),
                window
                    .find("confirm-title")
                    .label()
                    .unwrap_or_default()
                    .contains("no WHERE"),
            )
        })
        .unwrap();
    assert!(prompt.0, "the prompt is on screen");
    assert_eq!(
        prompt.1, "DELETE FROM items",
        "the user reads the statement"
    );
    assert!(prompt.2, "and why it is dangerous: {:?}", prompt.2);

    cx.update_window(f.window, |_, window, cx| {
        window.click("confirm-run-yes", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        drain(cx, &f),
        vec![QueryTabEvent::ConfirmedRun {
            script: script.clone()
        }],
        "confirming hands back the exact text the prompt showed"
    );
}

#[gpui_kit::test]
fn declining_the_prompt_runs_nothing(cx: &mut TestAppContext) {
    let f = open(cx, "DROP TABLE items;");
    let script = cx.read_entity(&f.view, |t, cx| t.script(cx));
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("a target");
            t.begin_run(target.clone(), script.clone(), cx);
            t.ask_confirmation(
                fg_app::views::PendingConfirmation {
                    script: script.clone(),
                    statement: script.clone(),
                    target,
                    reason: Some(DangerReason::DropObject),
                    production: false,
                },
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();

    cx.update_window(f.window, |_, window, cx| {
        window.click("confirm-run-no", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(drain(cx, &f), vec![QueryTabEvent::DismissedConfirmation]);
    // Read the view's own state, not the snapshot registry: a removed element can stay registered.
    assert!(
        cx.read_entity(&f.view, |t, _| t.pending_confirmation().is_none()),
        "the prompt is gone once declined"
    );
}

#[gpui_kit::test]
fn each_result_set_gets_its_own_tab_and_the_status_counts_rows(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1; SELECT 2;");
    feed(
        cx,
        &f,
        vec![
            Event::ResultStart {
                index: 0,
                columns: columns(),
            },
            Event::Rows(rows(3)),
            end(false),
            Event::ResultStart {
                index: 1,
                columns: columns(),
            },
            Event::Rows(rows(2)),
            end(false),
        ],
    );

    let tabs = cx
        .update_window(f.window, |_, window, _| {
            (
                window.find("result-tab-0").label().unwrap().to_owned(),
                window.find("result-tab-1").label().unwrap().to_owned(),
            )
        })
        .unwrap();
    assert_eq!(tabs.0, "Result 1 · 3 rows");
    assert_eq!(tabs.1, "Result 2 · 2 rows");
    assert_eq!(status(cx, &f), "5 rows · 2 of 2 statements · 6 ms");

    // Switching tabs swaps the grid to that set.
    cx.update_window(f.window, |_, window, cx| {
        window.click("result-tab-1", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        cx.read_entity(&f.view, |t, _| t.active_set()),
        1,
        "the second set is displayed"
    );
}

#[gpui_kit::test]
fn a_truncated_result_says_so(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT * FROM large_events;");
    feed(
        cx,
        &f,
        vec![
            Event::ResultStart {
                index: 0,
                columns: columns(),
            },
            Event::Rows(rows(2)),
            end(true),
        ],
    );
    assert!(
        status(cx, &f).contains("truncated"),
        "the cap is reported, not hidden: {}",
        status(cx, &f)
    );
    let label = cx
        .update_window(f.window, |_, window, _| {
            window
                .find("result-tab-0")
                .label()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap();
    assert!(
        label.contains("truncated"),
        "the tab itself says the result is short: {label}"
    );
}

#[gpui_kit::test]
fn a_statement_that_writes_reports_affected_rows(cx: &mut TestAppContext) {
    let f = open(cx, "UPDATE items SET qty = 1;");
    feed(
        cx,
        &f,
        vec![
            Event::ResultStart {
                index: 0,
                columns: vec![],
            },
            Event::ResultEnd {
                affected_rows: Some(7),
                last_insert_id: None,
                truncated: false,
                elapsed_ms: 2,
                warnings: vec![],
            },
        ],
    );
    let label = cx
        .update_window(f.window, |_, window, _| {
            window.find("result-tab-0").label().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(label, "Result 1 · 7 affected");
}

#[gpui_kit::test]
fn a_query_error_underlines_the_failing_bytes_of_that_statement(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;\nSELEC 2;");
    place_caret(cx, &f, 18); // inside `SELEC 2`
    // The engine reports a position inside the failing statement; the tab must map it to the buffer.
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t
                .target(RunMode::CurrentStatement, cx)
                .expect("the second statement");
            assert_eq!(target.start(), 10, "the caret is in statement 2");
            let generation = t.begin_run(target, t.script(cx), cx);
            t.fail_run(
                generation,
                &fg_core::Error::Query(QueryError {
                    message: "near \"SELEC\": syntax error".into(),
                    code: Some("1".into()),
                    position: Some(0),
                }),
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();

    let ranges: Vec<Range<usize>> = cx.read_entity(&f.view, |t, cx| t.underline_ranges(cx));
    assert_eq!(ranges.len(), 1, "one underline, at the word that broke");
    let shown = cx
        .update_window(f.window, |_, window, _| {
            window.find("run-error").label().unwrap().to_owned()
        })
        .unwrap();
    assert!(
        shown.contains("syntax error") && shown.contains("(1)"),
        "message and engine code: {shown}"
    );
}

#[gpui_kit::test]
fn results_from_an_outdated_run_are_ignored(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;");
    let (stale, current) = cx
        .update_window(f.window, |_, _window, cx| {
            f.view.update(cx, |t, cx| {
                let target = t.target(RunMode::All, cx).expect("a target");
                let script = t.script(cx);
                let first = t.begin_run(target.clone(), script.clone(), cx);
                let second = t.begin_run(target, script, cx);
                t.apply_event(
                    first,
                    Event::ResultStart {
                        index: 0,
                        columns: columns(),
                    },
                    cx,
                );
                t.apply_event(
                    second,
                    Event::ResultStart {
                        index: 0,
                        columns: columns(),
                    },
                    cx,
                );
                (first, second)
            })
        })
        .unwrap();
    let _ = (stale, current);
    let sets = cx.read_entity(&f.view, |t, _| t.results().sets().len());
    assert_eq!(sets, 1, "the abandoned run cannot add a result set");
}

#[gpui_kit::test]
fn an_unreadable_draft_is_shown_read_only_and_never_saved_over(cx: &mut TestAppContext) {
    let f = open(cx, "");
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            t.set_unavailable("unknown field `future`".into(), cx)
        });
        window.render_frame(cx);
    })
    .unwrap();
    let banner = cx
        .update_window(f.window, |_, window, _| {
            window
                .find("draft-unavailable")
                .label()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap();
    assert!(
        banner.contains("newer version"),
        "the reason is visible: {banner}"
    );

    // Read-only: a keystroke cannot change the buffer, so there is nothing to save, and the
    // controller must never write this draft back (it checks `is_unavailable` first).
    type_into(cx, &f, "x");
    assert!(
        !drain(cx, &f).contains(&QueryTabEvent::DraftChanged),
        "an unreadable draft does not accept edits"
    );
    let state = cx.read_entity(&f.view, |t, cx| {
        (t.is_unavailable(), t.is_dirty(), t.script(cx))
    });
    assert!(state.0, "it stays marked unreadable");
    assert!(!state.1, "and nothing is queued as an unsaved change");
    assert_eq!(state.2, "", "the buffer is untouched");
}

#[gpui_kit::test]
fn the_record_failure_is_shown_to_the_user(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;");
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("a target");
            let generation = t.begin_run(target, t.script(cx), cx);
            t.set_record_error(generation, Some("disk full".into()), cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    let shown = cx
        .update_window(f.window, |_, window, _| {
            window
                .find("record-error")
                .label()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap();
    assert!(
        shown.contains("could not record it") && shown.contains("disk full"),
        "a lost audit trail is visible: {shown}"
    );
}

#[gpui_kit::test]
fn cmd_y_opens_the_history_panel_and_asks_for_entries(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    cx.update_window(w.window, |_, window, cx| {
        window.click("sidebar-filter", cx);
        window.press("secondary-y", cx);
        window.render_frame(cx);
    })
    .unwrap();

    assert!(cx.read_entity(&w.view, |v, _| v.history_visible()));
    assert!(
        cx.update_window(w.window, |_, window, _| window
            .try_find("history-panel")
            .is_some())
            .unwrap(),
        "the panel is on screen"
    );
    assert!(
        workspace_events(&w).contains(&Seen::HistoryRequested(String::new())),
        "and it asks the controller for the newest entries"
    );
}

#[gpui_kit::test]
fn the_history_panel_lists_entries_with_their_outcome(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| {
            v.open_history(window, cx);
            v.set_history(
                vec![
                    entry(
                        "SELECT * FROM items",
                        fg_core::Outcome::Ok,
                        1_700_000_000_000,
                    ),
                    entry(
                        "DELETE FROM items",
                        fg_core::Outcome::Error,
                        1_700_000_060_000,
                    ),
                ],
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();

    let first = cx
        .update_window(w.window, |_, window, _| {
            window.find("history-0").label().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(first, "SELECT * FROM items");
    let second = cx
        .update_window(w.window, |_, window, _| {
            window.find("history-1").label().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(
        second, "DELETE FROM items",
        "a failed statement is listed too"
    );
}

#[gpui_kit::test]
fn picking_a_history_entry_puts_it_in_the_active_query_tab(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    let document = document();
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| {
            v.open_query(document, "SELECT 1;".into(), window, cx)
        });
        window.render_frame(cx);
    })
    .unwrap();

    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| {
            v.open_history(window, cx);
            v.set_history(
                vec![entry("SELECT 42", fg_core::Outcome::Ok, 1_700_000_000_000)],
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| v.pick_history(cx));
        window.render_frame(cx);
    })
    .unwrap();

    let seen = workspace_events(&w);
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, Seen::HistoryPicked(_)))
            .count(),
        1,
        "exactly one statement is handed to the controller: {seen:?}"
    );
    assert!(seen.contains(&Seen::HistoryPicked("SELECT 42".into())));
    assert!(
        !cx.read_entity(&w.view, |v, _| v.history_visible()),
        "and the panel closes"
    );
}

#[gpui_kit::test]
fn the_history_panel_says_when_it_is_empty_or_loading(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| v.open_history(window, cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        cx.update_window(w.window, |_, window, _| window
            .try_find("history-loading")
            .is_some())
            .unwrap(),
        "an in-flight load is not an empty list"
    );

    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| v.set_history(Vec::new(), cx));
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        cx.update_window(w.window, |_, window, _| window
            .try_find("history-empty")
            .is_some())
            .unwrap()
    );
}

#[gpui_kit::test]
fn the_lock_button_cycles_the_guard_level_and_asks_to_persist_it(cx: &mut TestAppContext) {
    use fg_core::SafeMode;
    let w = open_workspace(cx);
    assert_eq!(
        cx.read_entity(&w.view, |v, _| v.safe_mode()),
        SafeMode::Normal,
        "a development connection starts at Normal"
    );

    cx.update_window(w.window, |_, window, cx| {
        window.click("lock-btn", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        workspace_events(&w),
        vec![Seen::SafeMode(SafeMode::Alert)],
        "Normal -> Alert"
    );

    assert_eq!(
        fg_app::views::next_safe_mode(SafeMode::Alert),
        SafeMode::Strict
    );
    assert_eq!(
        fg_app::views::next_safe_mode(SafeMode::Strict),
        SafeMode::ReadOnly
    );
    assert_eq!(
        fg_app::views::next_safe_mode(SafeMode::ReadOnly),
        SafeMode::Normal
    );
    assert_eq!(
        fg_app::views::next_safe_mode(SafeMode::Unknown),
        SafeMode::Normal,
        "an unknown level is treated as read-only first, then wraps"
    );
}

#[gpui_kit::test]
fn the_stop_button_is_only_enabled_while_a_query_runs(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    let document = document();
    let tab = cx
        .update_window(w.window, |_, window, cx| {
            w.view.update(cx, |v, cx| {
                v.open_query(document, "SELECT 1;".into(), window, cx)
            })
        })
        .unwrap();

    let disabled = |cx: &mut TestAppContext| {
        cx.update_window(w.window, |_, window, _| window.find("stop-btn").disabled())
            .unwrap()
    };
    assert_ne!(disabled(cx), Some(false), "nothing is running yet");

    cx.update_window(w.window, |_, window, cx| {
        tab.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("a target");
            t.begin_run(target, t.script(cx), cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    assert_ne!(disabled(cx), Some(true), "a run makes it clickable");
}

#[gpui_kit::test]
fn a_query_tab_lives_in_the_strip_and_in_the_saved_session(cx: &mut TestAppContext) {
    use fg_app::views::{TabContent, WorkspaceEvent, open_workspace_window};
    use fg_core::{
        ConnectionConfig, ConnectionParams, Environment, ObjectInfo, ObjectKind, ObjectPath,
    };

    cx.update(|cx| {
        gpui_kit::init(cx);
        fg_app::views::bind_keys(cx);
    });
    let mut config = ConnectionConfig::new(
        "Billing",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: "x.db".into(),
        },
    );
    config.environment = Environment::Development;
    let (window, view) = cx.update(|cx| open_workspace_window(cx, config).unwrap());

    let asked = Rc::new(std::cell::Cell::new(0));
    let count = Rc::clone(&asked);
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &WorkspaceEvent, _| {
            if matches!(e, WorkspaceEvent::NewQueryRequested) {
                count.set(count.get() + 1);
            }
        })
        .detach();
    });

    // `⌘T` asks for a tab; the controller creates it (it owns ids and timestamps).
    cx.update_window(window, |_, window, cx| {
        // Focus something inside the window so the `Workspace` key context is on the path.
        window.click("sidebar-filter", cx);
        window.press("secondary-t", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update(|_| {});
    assert_eq!(asked.get(), 1, "the shortcut reaches the controller");

    let document = document();
    let id = document.id().clone();
    let _ = &id;
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.open_query(document, "SELECT 42;".into(), window, cx)
        });
        window.render_frame(cx);
    })
    .unwrap();

    // The tab id is the tab's own, not the document's, so read it back from the view.
    let tab_id = cx.read_entity(&view, |v, _| {
        v.tabs()
            .tabs()
            .iter()
            .find(|t| matches!(t.kind, fg_core::TabKind::Query { .. }))
            .map(|t| t.id.clone())
            .expect("a query tab")
    });
    let strip = cx
        .update_window(window, |_, window, _| {
            window
                .try_find(SharedString::from(format!("tab-{tab_id}")))
                .is_some()
        })
        .unwrap();
    assert!(strip, "the tab is in the strip");
    cx.read_entity(&view, |v, cx| {
        let tab = v
            .tabs()
            .tabs()
            .iter()
            .find(|t| matches!(t.kind, fg_core::TabKind::Query { .. }))
            .expect("a query tab in the session");
        assert_eq!(
            v.query_view(&tab.id).map(|t| t.read(cx).title()),
            Some("Query".to_owned())
        );
        assert!(matches!(v.active_tab_view(), Some(TabContent::Query(_))));
    });
    let saved = cx.read_entity(&view, |v, _| v.session());
    assert_eq!(saved.tabs.len(), 1, "the session keeps the query tab");
    assert!(
        matches!(saved.tabs[0].kind, fg_core::TabKind::Query { .. }),
        "and its kind"
    );

    // An object tree row still opens a table tab alongside it.
    let path = ObjectPath::new(Some("main"), "items");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.set_schema(
                vec!["main".into()],
                std::collections::BTreeMap::from([(
                    "main".to_string(),
                    vec![ObjectInfo::new(path.clone(), ObjectKind::Table)],
                )]),
                cx,
            );
            v.open_table(&path, window, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    let kinds: Vec<String> = cx.read_entity(&view, |v, _| {
        v.tabs()
            .tabs()
            .iter()
            .map(|t| match &t.kind {
                fg_core::TabKind::Table { .. } => "table".to_owned(),
                fg_core::TabKind::Query { .. } => "query".to_owned(),
            })
            .collect()
    });
    assert_eq!(kinds, vec!["query", "table"]);
    assert!(
        cx.update_window(window, |_, window, _| window
            .try_find("query-tab")
            .is_none())
            .unwrap(),
        "the grid is shown for the active tab, so the editor is not rendered"
    );
}

/// The caret position used by the tests above must be inside the second statement.
#[test]
fn the_fixture_offsets_match_their_scripts() {
    let script = "SELECT 1;\nSELECT 2;";
    assert_eq!(script.as_bytes()[17], b'2');
    assert_eq!(script.as_bytes()[10], b'S');
}

#[gpui_kit::test]
fn a_multi_statement_failure_underlines_the_statement_that_failed(cx: &mut TestAppContext) {
    // This is the case that used to have no underline: `fg-lsp` works out which statement failed
    // from how many produced a result set, so a script can be pointed at too.
    let script = "SELECT 1;\nSELECT 2;\nSELEC 3;";
    let f = open(cx, script);
    let third = script.rfind("SELEC 3").expect("the third statement");

    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let target = t.target(RunMode::All, cx).expect("the whole script");
            let plan = plan(target.sql(), DbKind::Sqlite, SafeMode::Normal);
            let generation = t.begin_run(target, t.script(cx), cx);
            t.apply_plan(generation, &plan, cx);
            // Two statements produced a result set before the third failed.
            t.apply_event(
                generation,
                Event::ResultStart {
                    index: 0,
                    columns: columns(),
                },
                cx,
            );
            t.apply_event(generation, end(false), cx);
            t.apply_event(
                generation,
                Event::ResultStart {
                    index: 1,
                    columns: columns(),
                },
                cx,
            );
            t.apply_event(generation, end(false), cx);
            t.fail_run(
                generation,
                &fg_core::Error::Query(QueryError {
                    message: "near \"SELEC\": syntax error".into(),
                    code: Some("1".into()),
                    position: Some(0),
                }),
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();

    let ranges = cx.read_entity(&f.view, |t, cx| t.underline_ranges(cx));
    assert_eq!(ranges.len(), 1, "exactly one underline");
    assert_eq!(
        ranges[0].start, third,
        "it starts at the third statement, not at the top of the script"
    );
    assert_eq!(
        &script[ranges[0].clone()],
        "SELEC",
        "on the word that broke"
    );

    let shown = cx
        .update_window(f.window, |_, window, _| {
            window.find("run-error").label().unwrap().to_owned()
        })
        .unwrap();
    assert!(
        shown.contains("syntax error"),
        "and the message is there: {shown}"
    );
}

// ---- history panel, snippets and safe mode -----------------------------------------------------

/// The events these tests care about, extracted as they arrive: `WorkspaceEvent` carries entity
/// handles and is not `Clone`, so the log keeps only plain data.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    HistoryRequested(String),
    HistoryPicked(String),
    SafeMode(fg_core::SafeMode),
    NewQuery,
    SnippetsRequested,
    SnippetSave { title: String, sql: String },
}

struct Workspace {
    window: AnyWindowHandle,
    view: Entity<fg_app::views::WorkspaceView>,
    events: Rc<RefCell<Vec<Seen>>>,
}

fn open_workspace(cx: &mut TestAppContext) -> Workspace {
    use fg_app::views::{WorkspaceEvent, open_workspace_window};
    use fg_core::{ConnectionConfig, ConnectionParams, Environment};

    cx.update(|cx| {
        gpui_kit::init(cx);
        fg_app::views::bind_keys(cx);
    });
    let mut config = ConnectionConfig::new(
        "shop",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: "x.db".into(),
        },
    );
    config.environment = Environment::Development;
    let (window, view) = cx.update(|cx| open_workspace_window(cx, config).unwrap());
    let events: Rc<RefCell<Vec<Seen>>> = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &WorkspaceEvent, _| {
            let seen = match e {
                WorkspaceEvent::HistoryRequested { search } => {
                    Seen::HistoryRequested(search.clone())
                }
                WorkspaceEvent::HistoryPicked { sql } => Seen::HistoryPicked(sql.clone()),
                WorkspaceEvent::SafeModeChanged { mode } => Seen::SafeMode(*mode),
                WorkspaceEvent::NewQueryRequested => Seen::NewQuery,
                WorkspaceEvent::SnippetsRequested => Seen::SnippetsRequested,
                WorkspaceEvent::SnippetSave { title, sql } => Seen::SnippetSave {
                    title: title.clone(),
                    sql: sql.clone(),
                },
                _ => return,
            };
            log.borrow_mut().push(seen);
        })
        .detach();
    });
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    Workspace {
        window,
        view,
        events,
    }
}

fn entry(sql: &str, outcome: fg_core::Outcome, at: i64) -> fg_core::QueryHistoryEntry {
    fg_core::QueryHistoryEntry {
        id: fg_core::HistoryId::new(),
        connection_id: ConnectionId::new(),
        namespace: Some("main".into()),
        sql: sql.into(),
        executed_at: UnixMillis(at),
        duration_ms: Some(4),
        outcome,
        row_count: Some(1),
        error: None,
        source: fg_core::QuerySource::Editor,
    }
}

fn workspace_events(w: &Workspace) -> Vec<Seen> {
    std::mem::take(&mut *w.events.borrow_mut())
}

fn snippet(title: &str, sql: &str, at: i64) -> fg_core::Snippet {
    fg_core::Snippet {
        id: fg_core::SnippetId::new(),
        connection_id: None,
        title: title.into(),
        sql: sql.into(),
        tags: Vec::new(),
        sort_order: 0,
        created_at: UnixMillis(at),
        updated_at: UnixMillis(at),
    }
}

// ---- formatting, comments, explain ------------------------------------------------------------

#[gpui_kit::test]
fn cmd_slash_toggles_line_comments(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT 1;\nSELECT 2;");
    select(cx, &f, 0..20);
    press(cx, &f, "secondary-/");
    let text = cx.read_entity(&f.view, |t, cx| t.script(cx));
    assert_eq!(text, "-- SELECT 1;\n-- SELECT 2;");

    // And back.
    select(cx, &f, 0..text.len());
    press(cx, &f, "secondary-/");
    assert_eq!(
        cx.read_entity(&f.view, |t, cx| t.script(cx)),
        "SELECT 1;\nSELECT 2;"
    );
}

#[gpui_kit::test]
fn alt_shift_f_formats_the_buffer(cx: &mut TestAppContext) {
    let f = open(cx, "select id,name from items where id>3");
    place_caret(cx, &f, 0);
    press(cx, &f, "alt-shift-f");
    assert_eq!(
        cx.read_entity(&f.view, |t, cx| t.script(cx)),
        "SELECT id, name\nFROM items\nWHERE id > 3\n"
    );
}

#[gpui_kit::test]
fn formatting_keeps_the_caret_in_place(cx: &mut TestAppContext) {
    let f = open(cx, "select 1");
    place_caret(cx, &f, 8); // end of the text
    press(cx, &f, "alt-shift-f");
    let (text, cursor) = cx.read_entity(&f.view, |t, cx| (t.script(cx), t.cursor(cx)));
    assert_eq!(text, "SELECT 1\n");
    assert!(cursor <= text.len(), "the caret stays inside the text");
}

#[gpui_kit::test]
fn cmd_shift_e_asks_to_explain_instead_of_running(cx: &mut TestAppContext) {
    let f = open(cx, "SELECT * FROM items;");
    place_caret(cx, &f, 0);
    press(cx, &f, "secondary-shift-e");
    let events = drain(cx, &f);
    assert_eq!(
        events,
        vec![QueryTabEvent::Explain],
        "explain is its own intent, not a run"
    );
    assert!(
        !cx.read_entity(&f.view, |t, _| t.results().is_running()),
        "nothing was executed by the shortcut itself"
    );
}

#[test]
fn the_explain_prefix_matches_the_engine() {
    // The controller builds `EXPLAIN QUERY PLAN …` for SQLite and a bare `EXPLAIN` elsewhere; the
    // plan query is what gets classified, which is why explaining a write works on a read-only
    // connection.
    let sql = "DELETE FROM items";
    let sqlite = format!("EXPLAIN QUERY PLAN {sql}");
    let planned = fg_service::plan(&sqlite, DbKind::Sqlite, SafeMode::ReadOnly);
    assert!(
        planned.refusal.is_none(),
        "an EXPLAIN is a read even when it wraps a write: {planned:?}"
    );
    let bare = format!("EXPLAIN {sql}");
    let planned = fg_service::plan(&bare, DbKind::Postgres, SafeMode::ReadOnly);
    assert!(planned.refusal.is_none());
}

// ---- snippets ---------------------------------------------------------------------------------

#[gpui_kit::test]
fn cmd_shift_s_opens_the_snippets_panel_and_asks_for_the_list(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    cx.update_window(w.window, |_, window, cx| {
        window.click("sidebar-filter", cx);
        window.press("secondary-shift-s", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(cx.read_entity(&w.view, |v, _| v.snippets_visible()));
    assert!(
        cx.update_window(w.window, |_, window, _| window
            .try_find("snippets-panel")
            .is_some())
            .unwrap()
    );
}

#[gpui_kit::test]
fn the_snippets_panel_lists_and_searches(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| {
            v.open_snippets(window, cx);
            v.set_snippets(
                vec![
                    snippet("All items", "SELECT * FROM items", 1),
                    snippet("Count orders", "SELECT count(*) FROM orders", 2),
                ],
                Vec::new(),
                cx,
            );
        });
        window.render_frame(cx);
    })
    .unwrap();
    let first = cx
        .update_window(w.window, |_, window, _| {
            window.find("snippet-0").label().unwrap().to_owned()
        })
        .unwrap();
    assert_eq!(first, "All items");

    // Searching filters by title, SQL and tags.
    cx.update_window(w.window, |_, window, cx| {
        w.view
            .update(cx, |v, cx| v.search_snippets("count", window, cx));
        window.render_frame(cx);
    })
    .unwrap();
    let visible = cx.read_entity(&w.view, |v, cx| v.snippet_titles(cx));
    assert_eq!(visible, vec!["Count orders".to_owned()]);
}

#[gpui_kit::test]
fn a_snippet_is_saved_from_the_current_statement(cx: &mut TestAppContext) {
    let w = open_workspace(cx);
    let document = document();
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| {
            v.open_query(document, "SELECT 42;".into(), window, cx)
        });
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(w.window, |_, window, cx| {
        w.view.update(cx, |v, cx| {
            v.open_snippets(window, cx);
            v.save_current_as_snippet(cx);
        });
        window.render_frame(cx);
    })
    .unwrap();

    let seen = workspace_events(&w);
    let saved = seen
        .into_iter()
        .find_map(|e| match e {
            Seen::SnippetSave { title, sql } => Some((title, sql)),
            _ => None,
        })
        .expect("the save is handed to the controller");
    assert_eq!(saved.1, "SELECT 42;");
    assert_eq!(
        saved.0, "SELECT 42",
        "the title is the first line, minus the semicolon"
    );
}

#[gpui_kit::test]
fn the_editor_offers_completions_from_the_schema(cx: &mut TestAppContext) {
    use fg_app::views::CompletionCatalog;

    let f = open(cx, "SELECT i. FROM items i");
    // What the controller does when the schema (and `describe`) answer.
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            let mut catalog = CompletionCatalog::with_objects(vec![fg_core::ObjectPath::new(
                Some("main"),
                "items",
            )]);
            catalog.set_columns(
                fg_core::ObjectPath::new(Some("main"), "items"),
                vec![ColumnMeta::new(
                    "id",
                    DataType::new("INTEGER", TypeCategory::Integer),
                )],
            );
            t.set_catalog(catalog, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();

    // The caret right after the dot: `i.` is a column position.
    let editor = cx.read_entity(&f.view, |t, _| t.editor_state().clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        editor.update(cx, |editor, cx| editor.set_selected_range(9..9, cx));
        window.render_frame(cx);
    })
    .unwrap();

    let provider = cx.read_entity(&f.view, |t, cx| {
        t.editor_state().read(cx).lsp().completion_provider.clone()
    });
    let provider = provider.expect("the tab installs a completion provider");

    let (rope, offset) = cx.read_entity(&f.view, |t, cx| {
        let editor = t.editor_state().read(cx);
        (editor.text().clone(), editor.cursor())
    });
    let response = cx
        .update_window(f.window, |_, window, cx| {
            let mut task = provider.completions(
                &rope,
                offset,
                lsp_types::CompletionContext {
                    trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                },
                window,
                cx,
            );
            let waker = futures::task::noop_waker();
            let mut context = std::task::Context::from_waker(&waker);
            match std::pin::Pin::new(&mut task).poll(&mut context) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            }
        })
        .unwrap()
        .expect("a ready response")
        .expect("no error");
    let items = match response {
        lsp_types::CompletionResponse::Array(items) => items,
        other => panic!("expected an array, got {other:?}"),
    };
    let column = items
        .iter()
        .find(|i| i.label == "i.id")
        .expect("the column of the aliased table is offered");
    assert_eq!(column.kind, Some(lsp_types::CompletionItemKind::FIELD));
    assert_eq!(column.detail.as_deref(), Some("integer"), "with its type");
    assert!(column.insert_text.is_none(), "insert_text would append");
    match column.text_edit.as_ref().expect("a text edit") {
        lsp_types::CompletionTextEdit::Edit(edit) => {
            assert_eq!(edit.new_text, "id");
            assert_eq!(edit.range.start.character, 9, "the word after the dot");
        }
        other => panic!("expected a plain edit, got {other:?}"),
    }
}

#[gpui_kit::test]
fn accepting_a_completion_replaces_the_typed_prefix_instead_of_appending(cx: &mut TestAppContext) {
    use fg_app::views::CompletionCatalog;

    // The bug this pins: typing `se` and accepting `SELECT` used to give `seselect`, because a
    // completion that only carries `insert_text` is inserted at the caret. The provider must give
    // the editor a `text_edit` covering the word.
    let f = open(cx, "se");
    cx.update_window(f.window, |_, window, cx| {
        f.view.update(cx, |t, cx| {
            t.set_catalog(CompletionCatalog::with_objects(Vec::new()), cx)
        });
        window.render_frame(cx);
    })
    .unwrap();

    let editor = cx.read_entity(&f.view, |t, _| t.editor_state().clone());
    cx.update_window(f.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        editor.update(cx, |editor, cx| editor.set_selected_range(2..2, cx));
        window.render_frame(cx);
    })
    .unwrap();

    let provider = cx
        .read_entity(&f.view, |t, cx| {
            t.editor_state().read(cx).lsp().completion_provider.clone()
        })
        .expect("the tab installs a provider");
    let (rope, offset) = cx.read_entity(&f.view, |t, cx| {
        let editor = t.editor_state().read(cx);
        (editor.text().clone(), editor.cursor())
    });
    let items = cx
        .update_window(f.window, |_, window, cx| {
            let mut task = provider.completions(
                &rope,
                offset,
                lsp_types::CompletionContext {
                    trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                },
                window,
                cx,
            );
            let waker = futures::task::noop_waker();
            let mut context = std::task::Context::from_waker(&waker);
            match std::pin::Pin::new(&mut task).poll(&mut context) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            }
        })
        .unwrap()
        .expect("a ready response")
        .expect("no error");
    let items = match items {
        lsp_types::CompletionResponse::Array(items) => items,
        other => panic!("expected an array, got {other:?}"),
    };
    let select = items
        .iter()
        .find(|i| i.label == "SELECT")
        .expect("the keyword is offered for `se`")
        .clone();

    // Apply it the way the editor does when the user presses Enter on a menu item.
    cx.update_window(f.window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.insert_completion(&select, offset..offset, window, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();

    let text = cx.read_entity(&f.view, |t, cx| t.script(cx));
    assert_eq!(text, "SELECT", "`se` was replaced, not appended to");
    assert!(!text.contains("seselect"), "the reported bug");
}

#[gpui_kit::test]
fn the_result_grid_has_the_same_header_and_status_geometry_as_the_table_grid(
    cx: &mut TestAppContext,
) {
    // A result and a table are the same widget with different data. When they differ, switching
    // between a table tab and a query tab moves the grid and the status bar jumps.
    let f = open(cx, "SELECT * FROM items;");
    feed(
        cx,
        &f,
        vec![
            Event::ResultStart {
                index: 0,
                columns: columns(),
            },
            Event::Rows(rows(3)),
            end(false),
        ],
    );

    // Like an unsorted table header (TablePlus style), a result header carries no sort arrow, so
    // the two line up. The header is drawn by the library without a test id, so assert on the
    // column definition the delegate hands it.
    let sortable = cx.read_entity(&f.view, |t, cx| {
        t.result_columns(cx)
            .first()
            .map(|column| column.sort.is_some())
    });
    assert_eq!(
        sortable,
        Some(false),
        "a result header shows no arrow, like an unsorted table header"
    );

    // The status bar is 32px, the table grid's height.
    let height: f32 = cx
        .update_window(f.window, |_, window, _| {
            f32::from(window.find("result-status-bar").bounds().size.height)
        })
        .unwrap();
    assert_eq!(height, 32.0, "the status bar matches the table grid's");
}

#[gpui_kit::test]
fn a_result_header_shows_the_column_name_and_nothing_else(cx: &mut TestAppContext) {
    // The table grid's header is the column name (plus a foreign-key arrow and a sort priority when
    // those apply). A result has neither, and it must not grow a type badge: the header is the
    // driver's column name, and `id integer` next to every column is noise.
    let f = open(cx, "SELECT * FROM items;");
    feed(
        cx,
        &f,
        vec![
            Event::ResultStart {
                index: 0,
                columns: columns(),
            },
            Event::Rows(rows(1)),
            end(false),
        ],
    );

    // The library paints the header cells without registering them, so the header's content is
    // checked where it is produced: the delegate's column definitions, whose `name` is what the
    // header draws. The types are still known (they drive alignment and cell rendering); they are
    // simply not drawn.
    let names = cx.read_entity(&f.view, |t, cx| t.result_header_text(cx));
    assert_eq!(
        names,
        vec!["id".to_owned(), "name".to_owned()],
        "column names only"
    );
    for name in &names {
        assert!(
            !name.contains("integer") && !name.contains("text"),
            "a header must not carry a type: {name}"
        );
    }
}
