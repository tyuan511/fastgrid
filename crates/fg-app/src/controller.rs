//! Wires views to `fg-service`: the part that loads, saves and opens things.
//!
//! Views stay dumb (data in, events out); this is where an event becomes a service call and the
//! answer goes back into a view.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use fg_core::{
    AppearanceSettings, ConnectionConfig, DbKind, GridSettings, ObjectInfo, ObjectPath,
    QueryDocument, Result, SafeMode, TabKind, UnixMillis, WorkspaceState,
};
use fg_driver::Event;
use fg_service::{
    BrowseOptions, OpenConnection, PasswordUpdate, PolicyRefusal, RunOptions, Service,
    TaskCanceller, plan,
};
use gpui_kit::*;

use crate::model::{QueryDocumentState, RunMode};
use crate::views::{
    Launcher, LauncherEvent, QueryTab, TabContent, TableTab, TableTabEvent, WorkspaceEvent,
    WorkspaceView, open_workspace_window, restore_query, rows_from,
};

/// Apply the saved light / dark / system choice (call once at startup).
pub fn apply_saved_appearance(service: Service, cx: &mut App) {
    cx.spawn(async move |cx| {
        let saved = service
            .setting::<AppearanceSettings>()
            .await
            .unwrap_or_default();
        cx.update(|cx| crate::theme_control::set_and_apply(saved.theme, cx));
    })
    .detach();
}

/// Switch to the next theme choice and remember it.
fn dismiss_backup_nudge(view: &Entity<Launcher>, service: &Service, cx: &mut App) {
    view.update(cx, |v, cx| v.set_backup_nudge(false, cx));
    let service = service.clone();
    cx.spawn(async move |_| {
        let _ = service
            .set_setting(fg_core::BackupNudge { dismissed: true })
            .await;
    })
    .detach();
}

fn cycle_theme(service: &Service, cx: &mut App) {
    let next = crate::theme_control::next(crate::theme_control::chosen(cx));
    crate::theme_control::set_and_apply(next, cx);
    let service = service.clone();
    cx.spawn(async move |_| {
        let mut saved = service
            .setting::<AppearanceSettings>()
            .await
            .unwrap_or_default();
        saved.theme = next;
        let _ = service.set_setting(saved).await;
    })
    .detach();
}

/// Create the launcher view and connect it to the service. Call from the window's builder.
pub fn launcher(service: Service, window: &mut Window, cx: &mut App) -> Entity<Launcher> {
    let view = cx.new(|cx| Launcher::new(window, cx));
    reload(&view, &service, cx);

    let events_service = service.clone();
    cx.subscribe(&view, move |view, event: &LauncherEvent, cx| match event {
        LauncherEvent::Open(id) => open(&view, &events_service, id.clone(), cx),
        LauncherEvent::Save(config) => save(&view, &events_service, (**config).clone(), cx),
        LauncherEvent::Delete(id) => delete(&view, &events_service, id.clone(), cx),
        LauncherEvent::CycleTheme => cycle_theme(&events_service, cx),
        LauncherEvent::DismissBackupNudge => dismiss_backup_nudge(&view, &events_service, cx),
        LauncherEvent::SaveGroup(group) => save_group(&view, &events_service, group.clone(), cx),
        LauncherEvent::DeleteGroup(id) => delete_group(&view, &events_service, id.clone(), cx),
    })
    .detach();

    // The first-launch banner: shown until the user closes it once.
    let (nudge_view, nudge_service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        let dismissed = nudge_service
            .setting::<fg_core::BackupNudge>()
            .await
            .map(|n| n.dismissed)
            .unwrap_or(false);
        let _ = nudge_view.update(cx, |view, cx| view.set_backup_nudge(!dismissed, cx));
    })
    .detach();

    // Keep the list current when connections change elsewhere.
    let mut events = service.subscribe();
    let (watch_view, watch_service) = (view.downgrade(), service);
    cx.spawn(async move |cx| {
        while let Ok(event) = events.recv().await {
            if matches!(event, fg_service::ServiceEvent::ConnectionsChanged) {
                let Some(view) = watch_view.upgrade() else {
                    break;
                };
                cx.update(|cx| reload(&view, &watch_service, cx));
            }
        }
    })
    .detach();
    view
}

fn reload(view: &Entity<Launcher>, service: &Service, cx: &mut App) {
    let (view, service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        let result = service.connections().await;
        let _ = view.update(cx, |view, cx| match result {
            Ok(list) => view.set_rows(rows_from(list), cx),
            Err(e) => view.set_message(Some(format!("Could not load connections: {e}")), cx),
        });
        let groups = service.groups().await;
        let _ = view.update(cx, |view, cx| match groups {
            Ok(groups) => view.set_groups(groups, cx),
            Err(e) => view.set_message(Some(format!("Could not load groups: {e}")), cx),
        });
    })
    .detach();
}

fn save_group(
    view: &Entity<Launcher>,
    service: &Service,
    group: fg_core::ConnectionGroup,
    cx: &mut App,
) {
    let (view, service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        let outcome = service.save_group(group).await;
        cx.update(|cx| {
            if let Some(view) = view.upgrade() {
                match outcome {
                    Ok(_) => reload(&view, &service, cx),
                    Err(e) => {
                        view.update(cx, |v, cx| {
                            v.set_message(Some(format!("Could not save the group: {e}")), cx)
                        });
                    }
                }
            }
        });
    })
    .detach();
}

fn delete_group(view: &Entity<Launcher>, service: &Service, id: fg_core::GroupId, cx: &mut App) {
    let (view, service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        // The store moves the group's connections to the top level on delete.
        let outcome = service.delete_group(id).await;
        cx.update(|cx| {
            if let Some(view) = view.upgrade() {
                match outcome {
                    Ok(_) => reload(&view, &service, cx),
                    Err(e) => {
                        view.update(cx, |v, cx| {
                            v.set_message(Some(format!("Could not delete the group: {e}")), cx)
                        });
                    }
                }
            }
        });
    })
    .detach();
}

fn open(view: &Entity<Launcher>, service: &Service, id: fg_core::ConnectionId, cx: &mut App) {
    view.update(cx, |v, cx| {
        v.set_message(None, cx);
        v.set_opening(Some(id.clone()), cx);
    });
    let (view, service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        let result = service.open(&id).await;
        cx.update(|cx| {
            let _ = view.update(cx, |v, cx| v.set_opening(None, cx));
            let outcome = match result {
                Ok(conn) => open_connection_window(service.clone(), conn, cx)
                    .map(|_| ())
                    .map_err(|e| format!("Could not open the window: {e}")),
                Err(e) => Err(format!("Could not connect: {e}")),
            };
            if let Err(message) = outcome {
                let _ = view.update(cx, |v, cx| v.set_message(Some(message), cx));
            }
        });
    })
    .detach();
}

/// Save a new, edited or duplicated connection.
fn save(view: &Entity<Launcher>, service: &Service, config: ConnectionConfig, cx: &mut App) {
    let (view, service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        let outcome = service.save_connection(config, PasswordUpdate::Keep).await;
        let _ = view.update(cx, |v, cx| match outcome {
            Ok(saved) => v.select(saved.id, cx),
            Err(e) => v.set_message(Some(format!("Could not save the connection: {e}")), cx),
        });
    })
    .detach();
}

fn delete(view: &Entity<Launcher>, service: &Service, id: fg_core::ConnectionId, cx: &mut App) {
    let (view, service) = (view.downgrade(), service.clone());
    cx.spawn(async move |cx| {
        if let Err(e) = service.delete_connection(&id).await {
            let _ = view.update(cx, |v, cx| {
                v.set_message(Some(format!("Could not delete the connection: {e}")), cx);
            });
        }
    })
    .detach();
}

// ---- a connection's window -------------------------------------------------------------------

/// Open the window for a connection and wire it to the service.
pub fn open_connection_window(
    service: Service,
    conn: Arc<OpenConnection>,
    cx: &mut App,
) -> std::result::Result<(AnyWindowHandle, Entity<WorkspaceView>), String> {
    let (window, view) = open_workspace_window(cx, conn.config().clone())?;
    wire_workspace(service, conn, window, view.clone(), cx);
    Ok((window, view))
}

fn wire_workspace(
    service: Service,
    conn: Arc<OpenConnection>,
    window: AnyWindowHandle,
    view: Entity<WorkspaceView>,
    cx: &mut App,
) {
    load_schema(&view, &conn, Some((window, service.clone())), cx);

    let save_timer: Rc<RefCell<Option<Task<()>>>> = Rc::new(RefCell::new(None));
    // One draft-save timer per tab: typing in two tabs must not cancel each other's save.
    let draft_timers: Rc<RefCell<HashMap<EntityId, Task<()>>>> = Rc::default();
    // One running statement per tab, plus its canceller so `⌘.` can reach it from the UI thread.
    let runs: Rc<RefCell<HashMap<EntityId, QueryRun>>> = Rc::default();
    cx.subscribe(&view, move |view, event: &WorkspaceEvent, cx| match event {
        WorkspaceEvent::TableOpened { path, tab } => {
            attach_table(&service, &conn, path.clone(), tab.clone(), cx);
        }
        WorkspaceEvent::QueryOpened { tab } => {
            load_draft(&service, &conn, tab.clone(), window, cx);
            feed_completion(&conn, &view, cx);
        }
        WorkspaceEvent::NewQueryRequested => {
            new_query(&service, &conn, &view, window, cx);
        }
        WorkspaceEvent::QueryRun { tab, mode } => {
            run_from_editor(&conn, &runs, tab, RunSubject::Mode(*mode), cx);
        }
        WorkspaceEvent::QueryConfirmed { tab, script } => {
            // `script` is the buffer text the prompt was built from; the tab hands the same string
            // back through `confirmation_target`, so the digest and the SQL cannot disagree.
            let _ = script;
            run_from_editor(&conn, &runs, tab, RunSubject::Confirmed, cx);
        }
        WorkspaceEvent::QueryCancel { tab } => cancel_run(&runs, tab, cx),
        WorkspaceEvent::QueryDraftChanged { tab } => {
            save_draft_debounced(&service, &draft_timers, tab, cx);
        }
        WorkspaceEvent::SnippetsRequested => load_snippets(&service, &conn, &view, cx),
        WorkspaceEvent::SnippetSave { title, sql } => {
            save_snippet(&service, &conn, &view, title.clone(), sql.clone(), cx);
        }
        WorkspaceEvent::SnippetDelete { id } => {
            delete_snippet(&service, &conn, &view, id.clone(), cx);
        }
        WorkspaceEvent::QueryExplain { tab } => {
            explain(&conn, &runs, tab, cx);
        }
        WorkspaceEvent::QueryResultCommit { tab } => {
            commit_result(&conn, tab, cx);
        }
        WorkspaceEvent::HistoryRequested { search } => {
            load_history(&service, &conn, &view, search.clone(), cx);
        }
        WorkspaceEvent::HistoryPicked { sql } => {
            insert_into_active_query(&view, sql, window, cx);
        }
        WorkspaceEvent::SafeModeChanged { mode } => {
            set_safe_mode(&service, &conn, &view, *mode, cx);
        }
        WorkspaceEvent::RefreshSchema => load_schema(&view, &conn, None, cx),
        WorkspaceEvent::CycleTheme => cycle_theme(&service, cx),
        WorkspaceEvent::SessionChanged => {
            // Save shortly after the last change, not on every click.
            let (service, id, weak) = (service.clone(), conn.id().clone(), view.downgrade());
            let task = cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(600))
                    .await;
                if let Ok(state) = weak.read_with(cx, |v, _| v.session()) {
                    let _ = service.save_workspace(&id, state).await;
                }
            });
            *save_timer.borrow_mut() = Some(task);
        }
    })
    .detach();
}

async fn list_all(
    conn: &Arc<OpenConnection>,
) -> Result<(Vec<String>, BTreeMap<String, Vec<ObjectInfo>>)> {
    let namespaces = conn.namespaces().await?;
    let mut objects = BTreeMap::new();
    for ns in &namespaces {
        objects.insert(ns.clone(), conn.objects(Some(ns)).await?.as_ref().clone());
    }
    Ok((namespaces, objects))
}

/// Read the schema into the sidebar. On the first load (`restore` given) also reopen the tabs
/// the user had last time.
fn load_schema(
    view: &Entity<WorkspaceView>,
    conn: &Arc<OpenConnection>,
    restore: Option<(AnyWindowHandle, Service)>,
    cx: &mut App,
) {
    let (view, conn) = (view.downgrade(), Arc::clone(conn));
    let _ = view.update(cx, |v, cx| v.set_schema_loading(cx));
    cx.spawn(async move |cx| {
        conn.refresh(&fg_service::Refresh::Everything);
        match list_all(&conn).await {
            Ok((namespaces, objects)) => {
                let for_completion = objects.clone();
                let _ = view.update(cx, |v, cx| v.set_schema(namespaces, objects, cx));
                // The schema just landed: hand it to completion now, so the first keystroke in a
                // query tab already knows the tables (and, as `describe` answers, the columns).
                let paths: Vec<ObjectPath> = for_completion
                    .values()
                    .flatten()
                    .filter(|o| {
                        matches!(
                            o.kind,
                            fg_core::ObjectKind::Table | fg_core::ObjectKind::View
                        )
                    })
                    .map(|o| o.path.clone())
                    .collect();
                let catalog = crate::views::CompletionCatalog::with_objects(paths.clone());
                let _ = view.update(cx, |v, cx| v.set_catalog_everywhere(catalog, cx));
                // Inlined rather than calling the helper: this task's context is an `AsyncApp`,
                // which can spawn but not take `&mut App`.
                let describe_view = view.clone();
                let describe_conn = Arc::clone(&conn);
                cx.spawn(async move |cx| {
                    describe_loop(describe_conn, describe_view, paths, cx).await;
                })
                .detach();
                if let Some((window, service)) = restore
                    && let Ok(Some(saved)) = service.load_workspace(conn.id()).await
                {
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = view.update(cx, |v, cx| v.restore(&saved, window, cx));
                    });
                    // Query tabs hold a document id, not a path, so they come back through the
                    // draft store rather than the schema (the schema check would drop them).
                    restore_query_tabs(&view, &conn, &saved, window, cx);
                }
            }
            Err(e) => {
                let _ = view.update(cx, |v, cx| {
                    v.set_schema_error(format!("Could not read the schema: {e}"), cx);
                });
            }
        }
    })
    .detach();
}

/// Give every query tab the schema it completes against: the object list first, then the columns of
/// each table as `describe` answers. Runs in the background; a slow schema does not block the tab.
fn feed_completion(conn: &Arc<OpenConnection>, view: &Entity<WorkspaceView>, cx: &mut App) {
    let paths = view.read(cx).visible_tables();
    let catalog = crate::views::CompletionCatalog::with_objects(paths.clone());
    view.update(cx, |v, cx| v.set_catalog_everywhere(catalog, cx));
    describe_into_completion(conn, view, paths, cx);
}

/// Describe each object and hand its columns to the tabs. Async so both a `spawn` body and a plain
/// `App` caller can use it without duplicating the loop.
async fn describe_loop(
    conn: Arc<OpenConnection>,
    view: WeakEntity<WorkspaceView>,
    paths: Vec<ObjectPath>,
    cx: &mut AsyncApp,
) {
    for path in paths {
        let Ok(detail) = conn.describe(&path).await else {
            continue;
        };
        let columns: Vec<fg_core::ColumnMeta> = detail
            .columns
            .iter()
            .map(|c| fg_core::ColumnMeta::new(c.name.clone(), c.data_type.clone()))
            .collect();
        let _ = view.update(cx, |v, cx| {
            v.set_columns_everywhere(path.clone(), columns.clone(), cx)
        });
    }
}

/// The `&mut App` entry point: spawn the same loop from a synchronous caller.
fn describe_into_completion(
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    paths: Vec<ObjectPath>,
    cx: &mut App,
) {
    let (conn, view) = (Arc::clone(conn), view.downgrade());
    cx.spawn(async move |cx| describe_loop(conn, view, paths, cx).await)
        .detach();
}

/// Everything a table tab keeps running; replacing a task cancels it (and its database query).
#[derive(Default)]
struct Loads {
    page: Option<Task<()>>,
    count: Option<Task<()>>,
    prefetch: Option<Task<()>>,
}

/// Connect a table tab to the database: restore its saved sorting and filters, load the first
/// page, and reload whenever the tab asks.
fn attach_table(
    service: &Service,
    conn: &Arc<OpenConnection>,
    path: ObjectPath,
    tab: Entity<TableTab>,
    cx: &mut App,
) {
    let loads: Rc<RefCell<Loads>> = Rc::default();

    let (reload_tab, reload_conn, reload_service, reload_loads) = (
        tab.clone(),
        Arc::clone(conn),
        service.clone(),
        Rc::clone(&loads),
    );
    cx.subscribe(&tab, move |_, event: &TableTabEvent, cx| match event {
        TableTabEvent::Reload => {
            run_load(
                &reload_service,
                &reload_conn,
                &reload_tab,
                &reload_loads,
                cx,
            );
        }
        // (Prefetching is triggered from run_load, not from an event, so the tab's public
        // event stream stays exactly the intents its owner acts on.)
        // The workspace window opens the referenced table; nothing to load for this tab.
        TableTabEvent::FollowForeignKey { .. } => {}
        TableTabEvent::ViewChanged => save_view(&reload_service, &reload_conn, &reload_tab, cx),
        TableTabEvent::CommitRequested => {
            let changes = reload_tab.read(cx).changes(cx);
            let path = reload_tab.read(cx).query().path().clone();
            let (conn, tab) = (Arc::clone(&reload_conn), reload_tab.downgrade());
            cx.spawn(async move |cx| {
                let result = conn
                    .commit_changes(&path, &changes)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                let _ = tab.update(cx, |t, cx| t.commit_finished(result, cx));
            })
            .detach();
        }
    })
    .detach();

    let (first_tab, first_conn, first_service) =
        (tab.downgrade(), Arc::clone(conn), service.clone());
    cx.spawn(async move |cx| {
        let saved = first_service
            .load_table_view(first_conn.id(), &path)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let page_size = first_service
            .setting::<GridSettings>()
            .await
            .map_or(crate::model::DEFAULT_PAGE_SIZE, |g| g.page_size);
        let hidden = saved.hidden_columns.clone();
        let engine = first_conn.kind();
        let targets = foreign_key_targets(&first_conn, &path).await;
        // A view, or a table without a primary key, comes back empty and stays read-only.
        let primary_key: Vec<String> = first_conn
            .describe(&path)
            .await
            .map(|d| d.primary_key().into_iter().map(str::to_owned).collect())
            .unwrap_or_default();
        let query = restore_query(path, page_size, &saved);
        cx.update(|cx| {
            let Some(tab) = first_tab.upgrade() else {
                return;
            };
            tab.update(cx, |t, cx| {
                t.set_foreign_keys(targets, cx);
                t.set_primary_key(primary_key, cx);
                t.set_engine(engine, cx);
                t.restore_query(query, cx);
                t.restore_hidden_columns(hidden, cx);
            });
            run_load(&first_service, &first_conn, &tab, &loads, cx);
        });
    })
    .detach();
}

/// Run the tab's current query: stream the page into the grid and count in the background.
fn run_load(
    service: &Service,
    conn: &Arc<OpenConnection>,
    tab: &Entity<TableTab>,
    loads: &Rc<RefCell<Loads>>,
    cx: &mut App,
) {
    let generation = tab.update(cx, |t, cx| t.begin_load(cx));
    let query = tab.read(cx).query().clone();
    let table_query = query.to_table_query();
    let need_total = query.page.total().is_none();

    // A page fetched ahead is served at once, with no new query: the columns and rows were
    // collected for exactly this query and page number, and anything else was dropped.
    let mut prefetch_key = table_query.clone();
    prefetch_key.offset = 0;
    let page_number = query.page.number();
    let hit = tab.update(cx, |t, _| t.take_prefetch(&prefetch_key, page_number));
    let served_from_prefetch = hit.is_some();
    if let Some(hit) = hit {
        tab.update(cx, |t, cx| {
            t.set_columns(generation, hit.columns, cx);
            t.append_rows(generation, hit.rows, cx);
            t.finish(generation, 0, cx);
        });
        // The page on screen is settled; fetch the next one before the user asks for it.
        spawn_prefetch(conn, tab, loads, cx);
    }

    let (page_tab, page_conn, page_service, page_query, page_loads) = (
        tab.downgrade(),
        Arc::clone(conn),
        service.clone(),
        table_query.clone(),
        Rc::clone(loads),
    );
    let page = if served_from_prefetch {
        None
    } else {
        Some(cx.spawn(async move |cx| {
            let mut stream = match page_conn
                .browse(&page_query, BrowseOptions::default())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = page_tab.update(cx, |t, cx| t.fail(generation, e.to_string(), cx));
                    return;
                }
            };
            while let Some(event) = stream.next().await {
                let keep_going = match event {
                    Ok(Event::ResultStart { columns, .. }) => page_tab
                        .update(cx, |t, cx| t.set_columns(generation, columns, cx))
                        .is_ok(),
                    Ok(Event::Rows(rows)) => page_tab
                        .update(cx, |t, cx| t.append_rows(generation, rows, cx))
                        .is_ok(),
                    Ok(Event::ResultEnd { elapsed_ms, .. }) => page_tab
                        .update(cx, |t, cx| t.finish(generation, elapsed_ms, cx))
                        .is_ok(),
                    Ok(_) => true,
                    Err(e) => {
                        let _ = page_tab.update(cx, |t, cx| t.fail(generation, e.to_string(), cx));
                        return;
                    }
                };
                if !keep_going {
                    return; // the tab was closed
                }
            }
            // Remember how this table was being viewed.
            if let Ok(state) = page_tab.read_with(cx, |t, cx| t.view_state(cx)) {
                let _ = page_service
                    .save_table_view(page_conn.id(), &page_query.path, state)
                    .await;
            }
            // The page on screen is settled; fetch the next one before the user asks for it.
            cx.update(|cx| {
                if let Some(tab) = page_tab.upgrade() {
                    spawn_prefetch(&page_conn, &tab, &page_loads, cx);
                }
            });
        }))
    };

    let count = need_total.then(|| {
        let (count_tab, count_conn, count_query) = (tab.downgrade(), Arc::clone(conn), table_query);
        cx.spawn(async move |cx| {
            if let Ok(total) = count_conn
                .count(&count_query, BrowseOptions::default())
                .await
            {
                let _ = count_tab.update(cx, |t, cx| t.set_total(generation, total, cx));
            }
        })
    });

    let mut loads = loads.borrow_mut();
    loads.page = page;
    loads.count = count;
}

/// Fetch the page after the one on screen into the tab's prefetch slot, so stepping forward
/// is instant. A no-op when there is no next page. If the tab moved on before the fetch
/// finished, the result is dropped instead of parked: parking it would overwrite the slot
/// the current page's own prefetch is about to fill.
fn spawn_prefetch(
    conn: &Arc<OpenConnection>,
    tab: &Entity<TableTab>,
    loads: &Rc<RefCell<Loads>>,
    cx: &mut App,
) {
    let (key, next, query) = {
        let q = tab.read(cx).query();
        if !q.page.has_next() {
            return;
        }
        let mut key = q.to_table_query();
        key.offset = 0;
        let next = q.page.number() + 1;
        let mut query = q.to_table_query();
        query.offset = next * query.limit;
        (key, next, query)
    };
    let (prefetch_tab, prefetch_conn) = (tab.downgrade(), Arc::clone(conn));
    let task = cx.spawn(async move |cx| {
        let mut stream = match prefetch_conn.browse(&query, BrowseOptions::default()).await {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut columns = None;
        let mut rows = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(Event::ResultStart { columns: c, .. }) => columns = Some(c),
                Ok(Event::Rows(rs)) => rows.extend(rs),
                // Do not break here: dropping the stream before it yields None
                // marks it abandoned, which cancels the session and poisons the
                // next statement leased from it (the page load that follows).
                Ok(Event::ResultEnd { .. }) => {}
                Ok(_) => {}
                Err(_) => return,
            }
        }
        let Some(columns) = columns else { return };
        let _ = prefetch_tab.update(cx, |t, _| {
            let mut current = t.query().to_table_query();
            current.offset = 0;
            if current == key && t.query().page.number() + 1 == next {
                t.set_prefetch(crate::views::PrefetchedPage {
                    key,
                    page: next,
                    columns,
                    rows,
                });
            }
        });
    });
    loads.borrow_mut().prefetch = Some(task);
}

/// Which columns of `path` can be followed to another table. A failure just means no arrows.
async fn foreign_key_targets(
    conn: &Arc<OpenConnection>,
    path: &ObjectPath,
) -> Vec<crate::model::JumpTarget> {
    let Ok(detail) = conn.describe(path).await else {
        return Vec::new();
    };
    // A key that names no column refers to the target's primary key: look those tables up first.
    let mut primary_keys = std::collections::HashMap::new();
    for target in crate::model::tables_needing_primary_key(&detail) {
        if let Ok(d) = conn.describe(&target).await
            && let [pk] = d.primary_key().as_slice()
        {
            primary_keys.insert(target, (*pk).to_owned());
        }
    }
    crate::model::jump_targets(&detail, |t| primary_keys.get(t).cloned())
}

/// Save what is remembered per table (sorting, filters, hidden columns).
fn save_view(service: &Service, conn: &Arc<OpenConnection>, tab: &Entity<TableTab>, cx: &mut App) {
    let path = tab.read(cx).query().path().clone();
    let state = tab.read(cx).view_state(cx);
    let (service, id) = (service.clone(), conn.id().clone());
    cx.spawn(async move |_| {
        let _ = service.save_table_view(&id, &path, state).await;
    })
    .detach();
}

// ---- query tabs -------------------------------------------------------------------------------
//
// The editor view emits intent; everything below turns it into a checked `fg-service` run and
// paints the answer back. The policy itself lives in the service (`plan` + `OpenConnection::run`):
// the UI only reads the plan to decide whether a human has to confirm first.

/// What a query tab keeps running. Replacing or dropping the task cancels the statement, which is
/// how a closed tab stops its query; `⌘.` uses the canceller instead so the outcome is recorded as
/// cancelled by the user.
struct QueryRun {
    /// Kept only so the run lives as long as the tab's current request.
    _task: Task<()>,
    /// The tab, so a closed tab's run can be pruned (and cancelled) on the next run.
    tab: WeakEntity<QueryTab>,
    /// Filled in once the stream exists; the UI thread needs it while the task is running.
    canceller: Arc<Mutex<Option<TaskCanceller>>>,
    generation: u64,
}

/// Why a run was asked for: a run mode, or the statement the user just confirmed.
enum RunSubject {
    Mode(RunMode),
    Confirmed,
}

/// `⌘T` / the SQL badge: a new draft, opened and persisted at once so a crash cannot lose the tab
/// the user just made.
fn new_query(
    service: &Service,
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let title = next_query_title(view, cx);
    let document = QueryDocumentState::new(conn.id().clone(), title, UnixMillis::now());
    let opened = window.update(cx, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.open_query(document, String::new(), window, cx)
        })
    });
    let Ok(tab) = opened else {
        return;
    };
    let (service, document) = (service.clone(), tab.read(cx).document().clone());
    cx.spawn(async move |_| {
        let now = UnixMillis::now();
        let _ = service.save_document(document.to_document("", now)).await;
    })
    .detach();
}

/// `Query`, `Query 2`, … without reusing the title of a tab that is still open.
fn next_query_title(view: &Entity<WorkspaceView>, cx: &App) -> String {
    let used: Vec<String> = view
        .read(cx)
        .tabs()
        .tabs()
        .iter()
        .filter_map(|tab| match &tab.kind {
            TabKind::Query { .. } => Some(
                view.read(cx)
                    .query_view(&tab.id)
                    .map(|t| t.read(cx).title())
                    .unwrap_or_else(|| "Query".to_owned()),
            ),
            TabKind::Table { .. } => None,
        })
        .collect();
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            "Query".to_owned()
        } else {
            format!("Query {n}")
        };
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Reopen the query tabs of a saved session. Their content comes later from the draft store, so a
/// draft deleted in the meantime still leaves the tab (empty) rather than silently dropping it.
fn restore_query_tabs(
    view: &WeakEntity<WorkspaceView>,
    conn: &Arc<OpenConnection>,
    saved: &WorkspaceState,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) {
    for tab in &saved.tabs {
        let TabKind::Query { document_id } = &tab.kind else {
            continue;
        };
        // A stand-in for the real record; `load_draft` replaces title and content when it finds it.
        let placeholder = QueryDocument {
            id: document_id.clone(),
            connection_id: conn.id().clone(),
            title: "Query".to_owned(),
            content: String::new(),
            saved_path: None,
            dirty: false,
            cursor_offset: None,
            created_at: UnixMillis(0),
            updated_at: UnixMillis(0),
        };
        let document = QueryDocumentState::from_document(&placeholder);
        let _ = window.update(cx, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.open_query(document, String::new(), window, cx)
            })
        });
    }
    if let Some(active) = &saved.active_tab {
        let _ = view.update(cx, |v, cx| v.activate_tab(active, cx));
    }
}

/// Read the draft a tab stands for: its title, its text, or the reason it cannot be read.
fn load_draft(
    service: &Service,
    conn: &Arc<OpenConnection>,
    tab: Entity<QueryTab>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let (tab, service, id, document_id) = (
        tab.downgrade(),
        service.clone(),
        conn.id().clone(),
        tab.read(cx).document_id().clone(),
    );
    cx.spawn(async move |cx| {
        let Ok(documents) = service.documents(&id).await else {
            return;
        };
        let Some(found) = documents.into_iter().find(|d| d.id() == &document_id) else {
            return;
        };
        let title = match &found {
            fg_store::StoredDocument::Ok(doc) => doc.title.clone(),
            // Shown and left alone: writing back would destroy a record we cannot parse.
            fg_store::StoredDocument::Unreadable { reason, .. } => {
                let _ = tab.update(cx, |t, cx| t.set_unavailable(reason.clone(), cx));
                return;
            }
        };
        let _ = tab.update(cx, |t, cx| t.set_title(title, cx));
        let fg_store::StoredDocument::Ok(doc) = found else {
            return;
        };
        if doc.content.is_empty() {
            return;
        }
        let _ = window.update(cx, |_, window, cx| {
            tab.update(cx, |t, cx| t.set_content(&doc.content, window, cx))
        });
    })
    .detach();
}

/// Run what the editor asks for. The script is read from the buffer here, so what reaches the
/// service is what is on screen at this instant - and, for a confirmed run, the byte-identical
/// string the confirmation digest was taken over.
fn run_from_editor(
    conn: &Arc<OpenConnection>,
    runs: &Rc<RefCell<HashMap<EntityId, QueryRun>>>,
    tab: &Entity<QueryTab>,
    subject: RunSubject,
    cx: &mut App,
) {
    let script = tab.read(cx).script(cx);
    let confirmed = matches!(subject, RunSubject::Confirmed);
    let Some((target, sql)) = (match &subject {
        RunSubject::Mode(mode) => tab.read(cx).target(*mode, cx).map(|target| {
            let sql = target.sql().to_owned();
            (target, sql)
        }),
        RunSubject::Confirmed => tab.read(cx).confirmation_target(),
    }) else {
        return;
    };
    if sql.trim().is_empty() {
        return;
    }

    let (kind, safe_mode) = (conn.kind(), conn.safe_mode());
    let plan = plan(&sql, kind, safe_mode);

    // Level 1 / 2: stop and ask a human before the service sees the script at all, so a declined
    // prompt never reaches the database. A read-only refusal still goes through `run` below, so the
    // attempt is recorded as `NotRun` by the service.
    if !confirmed
        && let Some(PolicyRefusal::NeedsConfirmation {
            statement, danger, ..
        }) = plan.refusal
    {
        let statement_text = plan
            .statements
            .get(statement)
            .map(|s| s.sql.clone())
            .unwrap_or_else(|| sql.clone());
        let production = conn.config().environment.is_production_like();
        tab.update(cx, |t, cx| {
            t.begin_run(target.clone(), script.clone(), cx);
            t.ask_confirmation(
                crate::views::PendingConfirmation {
                    script: sql,
                    statement: statement_text,
                    target,
                    reason: danger,
                    production,
                },
                cx,
            );
        });
        return;
    }

    let options = if confirmed {
        RunOptions::gui_confirmed(&sql, kind)
    } else {
        RunOptions::gui()
    };
    let generation = tab.update(cx, |t, cx| t.begin_run(target, script, cx));
    tab.update(cx, |t, cx| t.apply_plan(generation, &plan, cx));

    let canceller: Arc<Mutex<Option<TaskCanceller>>> = Arc::default();
    let (weak, conn, id) = (tab.downgrade(), Arc::clone(conn), tab.entity_id());
    let (task_tab, task_canceller, task_sql, task_plan) = (
        weak.clone(),
        Arc::clone(&canceller),
        sql.clone(),
        plan.clone(),
    );
    let task = cx.spawn(async move |cx| {
        let mut stream = match conn.run(&task_sql, &[], options).await {
            Ok(stream) => stream,
            Err(err) => {
                let _ = task_tab.update(cx, |t, cx| match (&err, task_plan.refusal) {
                    (fg_core::Error::PolicyDenied(_), Some(refusal)) => {
                        t.refuse(generation, &task_plan, refusal, cx);
                    }
                    _ => t.fail_run(generation, &err, cx),
                });
                return;
            }
        };
        if let Ok(mut slot) = task_canceller.lock() {
            *slot = Some(stream.canceller());
        }
        let mut failure = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    if task_tab
                        .update(cx, |t, cx| t.apply_event(generation, event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        // Truncation and record failures are per-run facts the service reports once the stream ends.
        let record_error = stream.record_error().map(str::to_owned);
        let succeeded = failure.is_none();
        let tables = task_tab
            .update(cx, |t, cx| {
                match failure {
                    Some(err) => t.fail_run(generation, &err, cx),
                    None => t.finish_run(generation, cx),
                }
                t.set_record_error(generation, record_error, cx);
                t.editable_result_tables()
            })
            .unwrap_or_default();
        // A finished run's single-table sets become editable once their primary key is known. A
        // description that fails (a view, a dropped table) just leaves that set read-only.
        if succeeded && !tables.is_empty() {
            let (describe_conn, describe_tab) = (Arc::clone(&conn), task_tab.clone());
            cx.spawn(async move |cx| {
                for (set_index, path) in tables {
                    let keys: Vec<String> = describe_conn
                        .describe(&path)
                        .await
                        .map(|d| d.primary_key().into_iter().map(str::to_owned).collect())
                        .unwrap_or_default();
                    if keys.is_empty() {
                        continue;
                    }
                    let done = describe_tab.update(cx, |t, cx| {
                        t.set_result_primary_key(set_index, keys, cx);
                    });
                    if done.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
    });

    runs.borrow_mut()
        .retain(|_, run| run.tab.upgrade().is_some());
    runs.borrow_mut().insert(
        id,
        QueryRun {
            _task: task,
            tab: weak,
            canceller,
            generation,
        },
    );
}

/// Commit an editable result set's staged edits through the same path a table tab uses, then
/// hand the outcome back to the tab: it folds committed values into its rows itself, without
/// re-running the query.
fn commit_result(conn: &Arc<OpenConnection>, tab: &Entity<QueryTab>, cx: &mut App) {
    let Some((path, changes)) = tab.read(cx).result_commit(cx) else {
        return;
    };
    let (conn, tab) = (Arc::clone(conn), tab.downgrade());
    cx.spawn(async move |cx| {
        let result = conn
            .commit_changes(&path, &changes)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        let _ = tab.update(cx, |t, cx| t.result_commit_finished(result, cx));
    })
    .detach();
}

/// `⌘.`: ask the engine to stop and show it as cancelled. The service records the outcome.
fn cancel_run(
    runs: &Rc<RefCell<HashMap<EntityId, QueryRun>>>,
    tab: &Entity<QueryTab>,
    cx: &mut App,
) {
    let id = tab.entity_id();
    let generation = {
        let all = runs.borrow();
        let Some(run) = all.get(&id) else {
            return;
        };
        if let Ok(slot) = run.canceller.lock()
            && let Some(canceller) = slot.as_ref()
        {
            canceller.cancel();
        }
        run.generation
    };
    tab.update(cx, |t, cx| t.mark_cancelled(generation, cx));
}

/// Persist the draft shortly after the last keystroke, not on every character.
fn save_draft_debounced(
    service: &Service,
    timers: &Rc<RefCell<HashMap<EntityId, Task<()>>>>,
    tab: &Entity<QueryTab>,
    cx: &mut App,
) {
    // An unreadable draft must never be written back over the newer record.
    if tab.read(cx).is_unavailable() {
        return;
    }
    let id = tab.entity_id();
    let (tab, service) = (tab.downgrade(), service.clone());
    let task = cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(600))
            .await;
        let Some((document, content)) = tab
            .read_with(cx, |t, cx| (t.document().clone(), t.script(cx)))
            .ok()
        else {
            return;
        };
        if !document.dirty() {
            return;
        }
        let now = UnixMillis::now();
        if service
            .save_document(document.to_document(&content, now))
            .await
            .is_ok()
        {
            let _ = tab.update(cx, |t, cx| t.mark_saved(cx));
        }
    });
    timers.borrow_mut().insert(id, task);
}

// ---- history and safe mode ---------------------------------------------------------------------

/// Load the recent statements of this connection into the `⌘Y` panel.
fn load_history(
    service: &Service,
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    search: String,
    cx: &mut App,
) {
    let (service, id, view) = (service.clone(), conn.id().clone(), view.downgrade());
    let search = search.trim().to_owned();
    cx.spawn(async move |cx| {
        let search = (!search.is_empty()).then_some(search);
        let entries = service
            .history(&id, search.as_deref(), HISTORY_LIMIT)
            .await
            .unwrap_or_default();
        let _ = view.update(cx, |v, cx| v.set_history(entries, cx));
    })
    .detach();
}

/// How many history entries the panel lists. Enough to scroll, small enough to decrypt quickly.
const HISTORY_LIMIT: usize = 200;

/// Put a statement the user picked into the active query tab, replacing what was there.
fn insert_into_active_query(
    view: &Entity<WorkspaceView>,
    sql: &str,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let target = view.read(cx).active_tab_view();
    let Some(TabContent::Query(tab)) = target else {
        return;
    };
    let sql = sql.to_owned();
    let _ = window.update(cx, |_, window, cx| {
        tab.update(cx, |t, cx| {
            t.set_content(&sql, window, cx);
            t.focus(window, cx);
        })
    });
}

/// Switch the guard level and persist it, so the next session starts with the same one.
fn set_safe_mode(
    service: &Service,
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    mode: SafeMode,
    cx: &mut App,
) {
    view.update(cx, |v, cx| v.set_safe_mode(mode, cx));
    // The service applies a safe mode when it opens a session, so an open connection keeps the old
    // one until it is reopened. Saving the config closes it, which is exactly what we want: the
    // next statement runs under the new level instead of the old one.
    let (service, id) = (service.clone(), conn.id().clone());
    cx.spawn(async move |_| {
        if let Ok(all) = service.connections().await
            && let Some(fg_store::StoredConnection::Ok(mut config)) =
                all.into_iter().find(|c| c.id() == &id)
        {
            config.safe_mode = Some(mode);
            let _ = service.save_connection(config, PasswordUpdate::Keep).await;
        }
    })
    .detach();
}

// ---- explain -----------------------------------------------------------------------------------

/// Show the plan for the statement under the caret instead of running it.
///
/// The plan comes from the same checked path as a query (`OpenConnection::run`): `EXPLAIN QUERY
/// PLAN` is classified as a read, so a read-only connection can still explain, and the attempt is
/// recorded in history like any other statement. Nothing here re-implements the engine's own plan
/// output - it is displayed as the rows the engine returned.
fn explain(
    conn: &Arc<OpenConnection>,
    runs: &Rc<RefCell<HashMap<EntityId, QueryRun>>>,
    tab: &Entity<QueryTab>,
    cx: &mut App,
) {
    let Some(target) = tab.read(cx).target(RunMode::CurrentStatement, cx) else {
        return;
    };
    let sql = target.sql().to_owned();
    if sql.trim().is_empty() {
        return;
    }
    // `EXPLAIN QUERY PLAN` on SQLite; the other engines take a bare `EXPLAIN`. The prefix is added
    // here rather than by the service because it is a presentation choice, not a policy one.
    let prefixed = explain_prefix(conn.kind(), &sql);
    // The plan query is what gets planned and run, so the service judges `EXPLAIN ...` (a read)
    // rather than the statement inside it. That is what makes explaining a write possible on a
    // read-only connection.
    let plan = fg_service::plan(&prefixed, conn.kind(), conn.safe_mode());
    // The generation the tab minted is the one every event of this run must carry.
    tab.update(cx, |t, cx| {
        let generation = t.begin_run(target.clone(), prefixed.clone(), cx);
        t.apply_plan(generation, &plan, cx);
    });
    let canceller: Arc<Mutex<Option<TaskCanceller>>> = Arc::default();
    let (weak, conn) = (tab.downgrade(), Arc::clone(conn));
    let task_tab = weak.clone();
    let (task_canceller, task_sql) = (Arc::clone(&canceller), prefixed);
    let id = tab.entity_id();
    let generation = tab.read(cx).results().generation();
    let task = cx.spawn(async move |cx| {
        let mut stream = match conn.run(&task_sql, &[], RunOptions::gui()).await {
            Ok(stream) => stream,
            Err(err) => {
                let _ = task_tab.update(cx, |t, cx| t.fail_run(generation, &err, cx));
                return;
            }
        };
        if let Ok(mut slot) = task_canceller.lock() {
            *slot = Some(stream.canceller());
        }
        let mut failure = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    if task_tab
                        .update(cx, |t, cx| t.apply_event(generation, event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        let record_error = stream.record_error().map(str::to_owned);
        let _ = task_tab.update(cx, |t, cx| {
            match failure {
                Some(err) => t.fail_run(generation, &err, cx),
                None => t.finish_run(generation, cx),
            }
            t.set_record_error(generation, record_error, cx);
        });
    });
    runs.borrow_mut()
        .retain(|_, run| run.tab.upgrade().is_some());
    runs.borrow_mut().insert(
        id,
        QueryRun {
            _task: task,
            tab: weak,
            canceller,
            generation,
        },
    );
}

/// The plan query for this engine. SQLite spells it `EXPLAIN QUERY PLAN`; PostgreSQL and MySQL take
/// a bare `EXPLAIN`, and MySQL needs `FORMAT=JSON` avoided so the output stays rows.
fn explain_prefix(kind: DbKind, sql: &str) -> String {
    match kind {
        DbKind::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
        _ => format!("EXPLAIN {sql}"),
    }
}

// ---- snippets ----------------------------------------------------------------------------------

/// Load the snippets visible for this connection (its own plus the global ones).
fn load_snippets(
    service: &Service,
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    cx: &mut App,
) {
    let (service, id, view) = (service.clone(), conn.id().clone(), view.downgrade());
    cx.spawn(async move |cx| {
        let stored = service.snippets(Some(&id)).await.unwrap_or_default();
        let mut readable = Vec::new();
        let mut unreadable = Vec::new();
        for entry in stored {
            match entry {
                fg_store::StoredSnippet::Ok(snippet) => readable.push(snippet),
                fg_store::StoredSnippet::Unreadable { id, .. } => unreadable.push(id),
            }
        }
        let _ = view.update(cx, |v, cx| v.set_snippets(readable, unreadable, cx));
    })
    .detach();
}

/// Save a snippet, then refresh the panel so the list shows it.
fn save_snippet(
    service: &Service,
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    title: String,
    sql: String,
    cx: &mut App,
) {
    let now = UnixMillis::now();
    let snippet = fg_core::Snippet {
        id: fg_core::SnippetId::new(),
        connection_id: Some(conn.id().clone()),
        title,
        sql,
        tags: Vec::new(),
        sort_order: 0,
        created_at: now,
        updated_at: now,
    };
    let (service, view) = (service.clone(), view.downgrade());
    let conn = Arc::clone(conn);
    cx.spawn(async move |cx| {
        let _ = service.save_snippet(snippet).await;
        // The panel's list comes from the store, so reload instead of patching it locally.
        let _ = view.update(cx, |v, cx| {
            v.request_snippets(cx);
        });
        let _ = conn;
    })
    .detach();
}

fn delete_snippet(
    service: &Service,
    conn: &Arc<OpenConnection>,
    view: &Entity<WorkspaceView>,
    id: fg_core::SnippetId,
    cx: &mut App,
) {
    let (service, view) = (service.clone(), view.downgrade());
    let conn = Arc::clone(conn);
    cx.spawn(async move |cx| {
        let _ = service.delete_snippet(&id).await;
        let _ = view.update(cx, |v, cx| v.request_snippets(cx));
        let _ = conn;
    })
    .detach();
}
