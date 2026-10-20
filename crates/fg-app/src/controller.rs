//! Wires views to `fg-service`: the part that loads, saves and opens things.
//!
//! Views stay dumb (data in, events out); this is where an event becomes a service call and the
//! answer goes back into a view.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fg_core::{AppearanceSettings, ConnectionConfig, GridSettings, ObjectInfo, ObjectPath, Result};
use fg_driver::Event;
use fg_service::{BrowseOptions, OpenConnection, PasswordUpdate, Service};
use gpui_kit::*;

use crate::views::{
    Launcher, LauncherEvent, TableTab, TableTabEvent, WorkspaceEvent, WorkspaceView,
    open_workspace_window, restore_query, rows_from,
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
    cx.subscribe(&view, move |view, event: &WorkspaceEvent, cx| match event {
        WorkspaceEvent::TableOpened { path, tab } => {
            attach_table(&service, &conn, path.clone(), tab.clone(), cx);
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
                let _ = view.update(cx, |v, cx| v.set_schema(namespaces, objects, cx));
                if let Some((window, service)) = restore
                    && let Ok(Some(saved)) = service.load_workspace(conn.id()).await
                {
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = view.update(cx, |v, cx| v.restore(&saved, window, cx));
                    });
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

/// Everything a table tab keeps running; replacing a task cancels it (and its database query).
#[derive(Default)]
struct Loads {
    page: Option<Task<()>>,
    count: Option<Task<()>>,
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
        // The workspace window opens the referenced table; nothing to load for this tab.
        TableTabEvent::FollowForeignKey { .. } => {}
        TableTabEvent::ViewChanged => save_view(&reload_service, &reload_conn, &reload_tab, cx),
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
        let targets = foreign_key_targets(&first_conn, &path).await;
        let query = restore_query(path, page_size, &saved);
        cx.update(|cx| {
            let Some(tab) = first_tab.upgrade() else {
                return;
            };
            tab.update(cx, |t, cx| {
                t.set_foreign_keys(targets, cx);
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

    let (page_tab, page_conn, page_service, page_query) = (
        tab.downgrade(),
        Arc::clone(conn),
        service.clone(),
        table_query.clone(),
    );
    let page = cx.spawn(async move |cx| {
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
    });

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
    loads.page = Some(page);
    loads.count = count;
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
