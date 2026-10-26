//! The query tab against the real stack: a SQLite file, the encrypted store, `fg-service`'s checked
//! run path, the controller and the views - all headless. What this pins is that the UI drives the
//! policy rather than reimplementing it: a refusal comes from the service, a confirmed write only
//! runs after a click, and a run lands in the query history.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use fg_app::controller::open_connection_window;
use fg_app::model::{QueryDocumentState, RunStatus};
use fg_app::views::{QueryTab, WorkspaceView};
use fg_core::{
    ConnectionConfig, ConnectionId, ConnectionParams, DbKind, ObjectPath, Outcome, QuerySource,
    SafeMode, UnixMillis, Value,
};
use fg_driver::ExecOptions;
use fg_service::{KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service, ServiceOptions};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

struct Env {
    dir: std::path::PathBuf,
    keys: KeychainKeyProvider<MemoryBackend>,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fg-query-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            keys: KeychainKeyProvider::with_backend(MemoryBackend::default()),
        }
    }

    fn service(&self) -> Service {
        Service::start(ServiceOptions::new(self.dir.join("store.db")), &self.keys).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Close the store before deleting: Windows will not remove a file that is still open.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The service runs on its own threads, so wait in real time rather than only pumping the clock.
fn settle(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct App {
    window: AnyWindowHandle,
    view: Entity<WorkspaceView>,
    connection_id: ConnectionId,
}

/// Open a workspace window on a seeded database under the given safe mode.
/// `password`, when given, is stored on the connection (for Strict-mode
/// re-authentication tests); the second save keeps it.
fn start(
    cx: &mut TestAppContext,
    env: &Env,
    safe_mode: SafeMode,
    password: Option<&str>,
) -> (Service, App) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        fg_app::views::bind_keys(cx);
    });
    let service = env.service();
    let mut config = ConnectionConfig::new(
        "shop",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: env.dir.join("shop.db").to_string_lossy().into_owned(),
        },
    );
    config.options.insert("create".into(), "true".into());
    config.safe_mode = Some(safe_mode);

    let svc = service.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(async {
        // Seed as Normal first: creating the table is a write, and the safe mode under test would
        // (correctly) refuse it. The mode is applied afterwards, which also exercises invalidating
        // an already-open connection when its config changes.
        let mut permissive = config.clone();
        permissive.safe_mode = Some(SafeMode::Normal);
        let saved = svc
            .save_connection(
                permissive,
                password
                    .map(|p| PasswordUpdate::Set(p.to_owned()))
                    .unwrap_or(PasswordUpdate::Keep),
            )
            .await
            .unwrap();
        let conn = svc.open(&saved.id).await.unwrap();
        conn.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO items VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
        drop(conn);
        let mut strict = config.clone();
        strict.safe_mode = Some(safe_mode);
        let saved = svc
            .save_connection(strict, PasswordUpdate::Keep)
            .await
            .unwrap();
        svc.open(&saved.id).await.unwrap()
    });
    drop(rt);

    let connection_id = conn.id().clone();
    let (window, view) = cx
        .update(|cx| open_connection_window(service.clone(), conn, cx))
        .unwrap();
    // The first schema load runs in the background; the sidebar is the signal it is done.
    settle(cx, "the schema to load", |cx| {
        cx.read_entity(&view, |v, _| v.visible_tables().is_empty())
    });
    (
        service,
        App {
            window,
            view,
            connection_id,
        },
    )
}

/// Open a query tab on `sql`, exactly as the controller's `⌘T` path would, and hand back its tab.
fn query_tab(cx: &mut TestAppContext, app: &App, sql: &str) -> Entity<QueryTab> {
    let document = QueryDocumentState::new(app.connection_id.clone(), "Query", UnixMillis::now());
    let sql = sql.to_owned();
    cx.update_window(app.window, |_, window, cx| {
        app.view
            .update(cx, |v, cx| v.open_query(document, sql, window, cx))
    })
    .unwrap()
}

/// Draw a frame. Headless assertions read the last painted tree, so after a change that only
/// updates state (a stream event arriving from the service runtime) the frame must be redrawn. The
/// real app paints continuously and has no such step.
fn paint(cx: &mut TestAppContext, app: &App) {
    cx.update_window(app.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn click(cx: &mut TestAppContext, app: &App, id: &'static str) {
    cx.update_window(app.window, |_, window, cx| {
        window.click(id, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

/// Focus the input behind `id` and type into it.
fn type_into(cx: &mut TestAppContext, app: &App, id: &'static str, text: &str) {
    cx.update_window(app.window, |_, window, cx| {
        window.click(id, cx);
        window.input(text, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn result_label(cx: &mut TestAppContext, app: &App, index: usize) -> String {
    cx.update_window(app.window, |_, window, _| {
        window
            .find(format!("result-tab-{index}"))
            .label()
            .unwrap_or_default()
            .to_owned()
    })
    .unwrap()
}

/// Click Run and wait for both halves of the answer: the tab reporting a terminal state (the
/// stream is drained on the UI side) and the service having written a history row for this attempt
/// (written on the recorder thread). Then repaint, because the assertions below read the painted
/// tree rather than the model.
fn run_and_wait(cx: &mut TestAppContext, service: &Service, app: &App) {
    click(cx, app, "run-statement");
    let seen = block_on_history(service, &app.connection_id).len();
    settle(cx, "the run to finish", |cx| {
        cx.run_until_parked();
        let terminal = cx.read_entity(&app.view, |v, cx| {
            v.tabs().tabs().iter().any(|t| {
                v.query_view(&t.id).is_some_and(|tab| {
                    matches!(
                        tab.read(cx).results().status(),
                        RunStatus::Finished | RunStatus::Failed | RunStatus::Refused
                    )
                })
            })
        });
        terminal && block_on_history(service, &app.connection_id).len() > seen
    });
    paint(cx, app);
}

#[gpui_kit::test]
fn running_a_query_paints_the_result_and_records_history(cx: &mut TestAppContext) {
    let env = Env::new("paint");
    let (service, app) = start(cx, &env, SafeMode::Normal, None);
    let tab = query_tab(cx, &app, "SELECT id, name FROM items;");

    run_and_wait(cx, &service, &app);

    assert_eq!(result_label(cx, &app, 0), "Result 1 · 3 rows");
    let status = cx
        .update_window(app.window, |_, window, _| {
            window.find("run-status").label().unwrap().to_owned()
        })
        .unwrap();
    assert!(
        status.contains("3 rows") && status.contains("1 of 1 statement"),
        "the status line summarises the run: {status}"
    );

    let entries = block_on_history(&service, &app.connection_id);
    let run = entries
        .iter()
        .find(|e| e.sql.contains("SELECT id, name FROM items"))
        .expect("the run is in the history");
    assert_eq!(run.outcome, Outcome::Ok);
    assert_eq!(run.row_count, Some(3));
    assert_eq!(run.source, QuerySource::Editor);
    let _ = tab;
}

#[gpui_kit::test]
fn creating_a_table_refreshes_the_sidebar_without_a_manual_reload(cx: &mut TestAppContext) {
    let env = Env::new("ddl-refresh");
    let (service, app) = start(cx, &env, SafeMode::Normal, None);
    let path = ObjectPath::new(Some("main"), "fresh");
    assert!(
        !cx.read_entity(&app.view, |v, _| v.object_exists(&path)),
        "the table does not exist before the run"
    );

    let _tab = query_tab(cx, &app, "CREATE TABLE fresh (id INTEGER PRIMARY KEY);");
    run_and_wait(cx, &service, &app);

    // No ⌘R anywhere in this test: the successful DDL run itself reloads the schema.
    settle(cx, "the sidebar to pick up the new table", |cx| {
        cx.read_entity(&app.view, |v, _| v.object_exists(&path))
    });
}

#[gpui_kit::test]
fn a_delete_without_where_waits_for_a_click_and_then_runs(cx: &mut TestAppContext) {
    let env = Env::new("confirm");
    let (service, app) = start(cx, &env, SafeMode::Alert, None);
    query_tab(cx, &app, "DELETE FROM items;");

    click(cx, &app, "run-statement");
    settle(cx, "the prompt to appear", |cx| {
        cx.run_until_parked();
        cx.read_entity(&app.view, |v, cx| {
            v.tabs().tabs().iter().any(|t| {
                v.query_view(&t.id)
                    .is_some_and(|tab| tab.read(cx).pending_confirmation().is_some())
            })
        })
    });
    paint(cx, &app);

    let prompted = cx.read_entity(&app.view, |v, cx| {
        v.tabs()
            .tabs()
            .iter()
            .find_map(|t| {
                v.query_view(&t.id)
                    .map(|tab| tab.read(cx).pending_confirmation().cloned())
            })
            .flatten()
    });
    let prompt = prompted.expect("level 1 asks before a delete without WHERE");
    assert_eq!(prompt.statement, "DELETE FROM items");
    assert_eq!(
        fg_service::plan(&prompt.script, DbKind::Sqlite, SafeMode::Alert)
            .refusal
            .as_ref()
            .map(|r| matches!(r, fg_service::PolicyRefusal::NeedsConfirmation { .. })),
        Some(true),
        "the confirmed string is the one the plan rejected"
    );

    // Nothing reached the database while the prompt was up.
    let count = block_on_count(&service, &app.connection_id, "SELECT count(*) FROM items;");
    assert_eq!(count, 3, "a pending run cannot have executed");

    let shown = cx
        .update_window(app.window, |_, window, _| {
            window
                .find("confirm-statement")
                .label()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap();
    assert_eq!(shown, "DELETE FROM items", "the user reads the statement");

    click(cx, &app, "confirm-run-yes");
    settle(cx, "the confirmed delete to finish", |cx| {
        cx.run_until_parked();
        cx.read_entity(&app.view, |v, cx| {
            v.tabs().tabs().iter().any(|t| {
                v.query_view(&t.id)
                    .is_some_and(|tab| tab.read(cx).results().status() == RunStatus::Finished)
            })
        })
    });

    let left = block_on_count(&service, &app.connection_id, "SELECT count(*) FROM items;");
    assert_eq!(left, 0, "and after the click it really ran");
}

#[gpui_kit::test]
fn strict_mode_password_prompt_blocks_a_wrong_password_and_runs_on_the_right_one(
    cx: &mut TestAppContext,
) {
    let env = Env::new("strict-password");
    let (service, app) = start(cx, &env, SafeMode::Strict, Some("s3cret"));
    query_tab(cx, &app, "DELETE FROM items;");

    click(cx, &app, "run-statement");
    settle(cx, "the password prompt to appear", |cx| {
        cx.run_until_parked();
        cx.read_entity(&app.view, |v, cx| {
            v.tabs().tabs().iter().any(|t| {
                v.query_view(&t.id).is_some_and(|tab| {
                    tab.read(cx)
                        .pending_confirmation()
                        .is_some_and(|p| p.require_password)
                })
            })
        })
    });
    paint(cx, &app);

    // A wrong password refuses, runs nothing, and leaves the prompt up.
    type_into(cx, &app, "confirm-password", "wrong");
    click(cx, &app, "confirm-run-yes");
    settle(cx, "the password refusal to appear", |cx| {
        cx.run_until_parked();
        cx.read_entity(&app.view, |v, cx| {
            v.tabs().tabs().iter().any(|t| {
                v.query_view(&t.id)
                    .is_some_and(|tab| tab.read(cx).confirm_password_error().is_some())
            })
        })
    });
    let count = block_on_count(&service, &app.connection_id, "SELECT count(*) FROM items;");
    assert_eq!(count, 3, "a wrong password runs nothing");

    // The right password runs the statement for real.
    type_into(cx, &app, "confirm-password", "s3cret");
    click(cx, &app, "confirm-run-yes");
    settle(cx, "the confirmed delete to finish", |cx| {
        cx.run_until_parked();
        cx.read_entity(&app.view, |v, cx| {
            v.tabs().tabs().iter().any(|t| {
                v.query_view(&t.id)
                    .is_some_and(|tab| tab.read(cx).results().status() == RunStatus::Finished)
            })
        })
    });
    let left = block_on_count(&service, &app.connection_id, "SELECT count(*) FROM items;");
    assert_eq!(left, 0, "and after the right password it really ran");
}

#[gpui_kit::test]
fn a_read_only_connection_refuses_the_write_and_changes_nothing(cx: &mut TestAppContext) {
    let env = Env::new("readonly");
    let (service, app) = start(cx, &env, SafeMode::ReadOnly, None);
    query_tab(cx, &app, "UPDATE items SET name = 'zz';");

    // The refusal is recorded by the service and painted by the tab; wait for both.
    run_and_wait(cx, &service, &app);

    let refusal = cx
        .update_window(app.window, |_, window, _| {
            window
                .try_find("run-refused")
                .map(|s| s.label().unwrap_or_default().to_owned())
        })
        .unwrap();
    let refusal = refusal.expect("the refusal is shown");
    assert!(refusal.contains("read-only"), "and says why: {refusal}");

    let left = block_on_count(
        &service,
        &app.connection_id,
        "SELECT count(*) FROM items WHERE name = 'zz';",
    );
    assert_eq!(left, 0, "the write never happened");

    let entries = block_on_history(&service, &app.connection_id);
    let attempt = entries
        .iter()
        .find(|e| e.sql.contains("UPDATE items SET name"))
        .expect("the attempt is still recorded");
    assert_eq!(attempt.outcome, Outcome::NotRun);
}

#[gpui_kit::test]
fn typing_saves_the_draft_and_a_reopen_reads_it_back(cx: &mut TestAppContext) {
    let env = Env::new("draft");
    let (service, app) = start(cx, &env, SafeMode::Normal, None);
    let tab = query_tab(cx, &app, "");

    let editor = cx.read_entity(&tab, |t, _| t.editor_state().clone());
    cx.update_window(app.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        window.input("SELECT 1;", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        cx.read_entity(&tab, |t, _| t.is_dirty()),
        "typing marks the draft dirty"
    );

    // The debounce is virtual time: advance it, then wait for the tab to learn that its draft is
    // on disk. The row appears before the save future resumes, so polling the store alone would
    // race the `mark_saved` that follows it.
    let id = cx.read_entity(&tab, |t, _| t.document_id().clone());
    cx.executor().advance_clock(Duration::from_millis(700));
    settle(cx, "the draft to be saved", |cx| {
        cx.run_until_parked();
        !cx.read_entity(&tab, |t, _| t.is_dirty())
    });
    let docs = block_on_docs(&service, &app.connection_id);
    assert!(
        docs.iter()
            .any(|d| d.id() == &id && d.content_of().as_deref() == Some("SELECT 1;")),
        "the draft is on disk"
    );

    // Reopening the same document (empty buffer, as a restore does) reads the stored text back.
    let state = cx.read_entity(&tab, |t, _| t.document().clone());
    let reopened = cx
        .update_window(app.window, |_, window, cx| {
            app.view
                .update(cx, |v, cx| v.open_query(state, String::new(), window, cx))
        })
        .unwrap();
    settle(cx, "the draft to load back", |cx| {
        cx.run_until_parked();
        cx.read_entity(&reopened, |t, cx| t.script(cx) == "SELECT 1;")
    });
    assert!(
        !cx.read_entity(&reopened, |t, _| t.is_dirty()),
        "restoring a draft is not an edit"
    );
}

#[gpui_kit::test]
fn a_journal_newer_than_the_saved_draft_is_restored_on_reopen(cx: &mut TestAppContext) {
    let env = Env::new("journal-restore");
    let (service, app) = start(cx, &env, SafeMode::Normal, None);
    let tab = query_tab(cx, &app, "");
    let state = cx.read_entity(&tab, |t, _| t.document().clone());

    // A saved draft from before the crash, and a journal entry holding newer, unsaved work.
    block_on_save_doc(&service, state.to_document("SELECT 1;", UnixMillis(1000)));
    block_on_save_journal(&service, state.to_document("SELECT 2;", UnixMillis(2000)));

    let reopened = cx
        .update_window(app.window, |_, window, cx| {
            app.view
                .update(cx, |v, cx| v.open_query(state, String::new(), window, cx))
        })
        .unwrap();
    settle(cx, "the journal content to be restored", |cx| {
        cx.run_until_parked();
        cx.read_entity(&reopened, |t, cx| t.script(cx) == "SELECT 2;")
    });
    assert!(
        cx.read_entity(&reopened, |t, _| t.is_dirty()),
        "restored journal content is unsaved work, not a clean draft"
    );
}

#[gpui_kit::test]
fn typing_writes_the_journal_and_a_save_clears_it(cx: &mut TestAppContext) {
    let env = Env::new("journal-write");
    let (service, app) = start(cx, &env, SafeMode::Normal, None);
    let tab = query_tab(cx, &app, "");

    let editor = cx.read_entity(&tab, |t, _| t.editor_state().clone());
    cx.update_window(app.window, |_, window, cx| {
        window.click(("input", editor.entity_id()), cx);
        window.input("SELECT 3;", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let id = cx.read_entity(&tab, |t, _| t.document_id().clone());

    // The journal lands without waiting for the 600ms debounced save.
    settle(cx, "the journal to hold the typed text", |cx| {
        cx.run_until_parked();
        block_on_journal(&service, &id).as_deref() == Some("SELECT 3;")
    });

    cx.executor().advance_clock(Duration::from_millis(700));
    settle(cx, "the draft to be saved and the journal cleared", |cx| {
        cx.run_until_parked();
        !cx.read_entity(&tab, |t, _| t.is_dirty()) && block_on_journal(&service, &id).is_none()
    });
}

#[gpui_kit::test]
fn the_query_editor_wires_hover_and_definition_providers(cx: &mut TestAppContext) {
    let env = Env::new("inspect-wiring");
    let (_service, app) = start(cx, &env, SafeMode::Normal, None);
    let tab = query_tab(cx, &app, "");
    let editor = cx.read_entity(&tab, |t, _| t.editor_state().clone());
    let (hover, definition) = cx.read_entity(&editor, |e, _| {
        let lsp = e.lsp();
        (
            lsp.hover_provider.is_some(),
            lsp.definition_provider.is_some(),
        )
    });
    assert!(hover, "the editor has a hover provider");
    assert!(definition, "the editor has a definition provider");
}

// ---- blocking helpers ---------------------------------------------------------------------------
//
// The service API is async and runs on its own runtime; tests read the store through it by driving
// the future to completion on a throwaway runtime, which is what `settle` then polls.

fn block_on_history(service: &Service, id: &ConnectionId) -> Vec<fg_core::QueryHistoryEntry> {
    let (service, id) = (service.clone(), id.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async { service.history(&id, None, 50).await.unwrap() })
}

fn block_on_save_doc(service: &Service, doc: fg_core::QueryDocument) {
    let service = service.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async { service.save_document(doc).await.unwrap() });
}

fn block_on_save_journal(service: &Service, doc: fg_core::QueryDocument) {
    let service = service.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async { service.save_journal(doc).await.unwrap() });
}

/// The journal entry's content, if a readable one exists.
fn block_on_journal(service: &Service, id: &fg_core::DocumentId) -> Option<String> {
    let (service, id) = (service.clone(), id.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async { service.journal(&id).await.unwrap() })
        .and_then(|item| match item {
            fg_store::StoredDocument::Ok(doc) => Some(doc.content),
            fg_store::StoredDocument::Unreadable { .. } => None,
        })
}

fn block_on_docs(service: &Service, id: &ConnectionId) -> Vec<Draft> {
    let (service, id) = (service.clone(), id.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async { service.documents(&id).await.unwrap() })
        .into_iter()
        .map(Draft::from_stored)
        .collect()
}

/// A draft, readable or not, in a shape tests can assert on.
struct Draft(fg_store::StoredDocument);

impl Draft {
    fn from_stored(doc: fg_store::StoredDocument) -> Self {
        Self(doc)
    }

    fn id(&self) -> &fg_core::DocumentId {
        self.0.id()
    }

    fn content_of(&self) -> Option<String> {
        match &self.0 {
            fg_store::StoredDocument::Ok(doc) => Some(doc.content.clone()),
            fg_store::StoredDocument::Unreadable { .. } => None,
        }
    }
}

/// Run a counting query on a throwaway runtime, outside the connection under test.
fn block_on_count(service: &Service, id: &ConnectionId, sql: &str) -> i64 {
    let (service, id, sql) = (service.clone(), id.clone(), sql.to_owned());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let conn = service.open(&id).await.unwrap();
        let sets = conn
            .execute(&sql, &[], ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        match sets[0].rows[0][0] {
            Value::Int(n) => n,
            Value::UInt(n) => i64::try_from(n).unwrap_or(i64::MAX),
            ref other => panic!("expected a count, got {other:?}"),
        }
    })
}
