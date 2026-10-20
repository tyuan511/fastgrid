//! The whole stack in one test: a real SQLite file, the real encrypted store and service, the real
//! controller and views in a headless window. Opening a table must stream rows into the grid.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use fg_app::controller::open_connection_window;
use fg_core::{ConnectionConfig, ConnectionParams, DbKind, ObjectPath, Value};
use fg_driver::ExecOptions;
use fg_service::{KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service, ServiceOptions};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use fg_app::views::{TableTab, WorkspaceView};

struct Env {
    dir: std::path::PathBuf,
    keys: KeychainKeyProvider<MemoryBackend>,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fg-e2e-{}-{name}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Let background work finish: the service runs on its own threads, so poll in real time.
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
    path: ObjectPath,
}

/// A service with a seeded database, opened in a workspace window.
fn start(cx: &mut TestAppContext, env: &Env) -> (Service, App) {
    // The service runs on its own threads; GPUI's deterministic test scheduler must allow that.
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    let service = env.service();
    let mut config = ConnectionConfig::new(
        "shop",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: env.dir.join("shop.db").to_string_lossy().into_owned(),
        },
    );
    config.options.insert("create".into(), "true".into());

    // Seed through the real service on a throwaway runtime thread.
    let svc = service.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(async {
        let saved = svc.save_connection(config, PasswordUpdate::Keep).await.unwrap();
        let conn = svc.open(&saved.id).await.unwrap();
        conn.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER, note TEXT);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
             INSERT INTO items SELECT i, 'item' || i, (i * 7) % 10, CASE WHEN i % 100 = 0 THEN NULL ELSE 'note ' || i END FROM n;
             CREATE VIEW cheap AS SELECT * FROM items WHERE score < 3;",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
        conn
    });
    drop(rt);

    let (window, view) = cx
        .update(|cx| open_connection_window(service.clone(), conn, cx))
        .unwrap();
    (
        service,
        App {
            window,
            view,
            path: ObjectPath::new(Some("main"), "items"),
        },
    )
}

fn open_items(cx: &mut TestAppContext, app: &App) -> Entity<TableTab> {
    // The schema loads in the background; the sidebar lists the tables once it has.
    settle(cx, "the schema to load", |cx| {
        cx.update_window(app.window, |_, window, _| {
            window.try_find("schema-loading").is_none()
        })
        .unwrap()
            && cx.read_entity(&app.view, |v, _| v.object_exists(&app.path))
    });
    cx.update_window(app.window, |_, window, cx| {
        app.view
            .update(cx, |v, cx| v.open_table(&app.path, window, cx));
    })
    .unwrap();
    cx.read_entity(&app.view, |v, _| {
        let id = v.tabs().active_id().unwrap().clone();
        v.tab_view(&id).unwrap().clone()
    })
}

fn status(cx: &mut TestAppContext, app: &App) -> String {
    cx.update_window(app.window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find("grid-status")
            .and_then(|s| s.label().map(str::to_owned))
            .unwrap_or_default()
    })
    .unwrap()
}

#[gpui_kit::test]
fn opening_a_table_streams_its_first_page_into_the_grid(cx: &mut TestAppContext) {
    let env = Env::new("open");
    let (_service, app) = start(cx, &env);
    let tab = open_items(cx, &app);

    settle(cx, "the first page and the row count", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
    });
    cx.read_entity(&tab, |t, cx| {
        assert_eq!(t.column_names(cx), ["id", "name", "score", "note"]);
        assert_eq!(t.loaded_rows(), 300);
        assert_eq!(
            t.value_at(0, 0, cx),
            Some(Value::Int(1)),
            "ordered by the primary key"
        );
        assert_eq!(t.value_at(0, 1, cx), Some(Value::Text("item1".into())));
        assert_eq!(
            t.value_at(99, 3, cx),
            Some(Value::Null),
            "row 100 has a NULL note"
        );
    });
}

#[gpui_kit::test]
fn paging_and_sorting_fetch_the_right_rows(cx: &mut TestAppContext) {
    let env = Env::new("paging");
    let (_service, app) = start(cx, &env);
    let tab = open_items(cx, &app);
    settle(cx, "first page", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
    });

    // Next page.
    cx.update_window(app.window, |_, window, cx| window.click("page-next", cx))
        .unwrap();
    settle(cx, "page 2", |cx| {
        status(cx, &app).starts_with("Showing 301–600 of 1,000")
    });
    cx.read_entity(&tab, |t, cx| {
        assert_eq!(t.value_at(0, 0, cx), Some(Value::Int(301)))
    });

    // Sorting returns to page 1 and orders by score then by the primary key.
    cx.update_window(app.window, |_, _, cx| {
        tab.update(cx, |t, cx| t.toggle_sort("score", cx))
    })
    .unwrap();
    settle(cx, "sorted page 1", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
            && cx.read_entity(&tab, |t, cx| t.value_at(0, 2, cx) == Some(Value::Int(0)))
    });
    cx.read_entity(&tab, |t, cx| {
        let scores: Vec<i64> = (0..300)
            .map(|i| match t.value_at(i, 2, cx) {
                Some(Value::Int(n)) => n,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(
            scores.windows(2).all(|w| w[0] <= w[1]),
            "ascending by score"
        );
        // Within equal scores the primary key breaks ties, so paging cannot repeat or skip rows.
        let ids: Vec<i64> = (0..300)
            .filter(|i| scores[*i] == 0)
            .map(|i| match t.value_at(i, 0, cx) {
                Some(Value::Int(n)) => n,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    });
}

#[gpui_kit::test]
fn changing_the_page_size_shows_that_many_rows(cx: &mut TestAppContext) {
    let env = Env::new("pagesize");
    let (_service, app) = start(cx, &env);
    let tab = open_items(cx, &app);
    settle(cx, "first page", |cx| {
        status(cx, &app).starts_with("Showing 1–300")
    });

    cx.update_window(app.window, |_, window, cx| {
        window.click("page-size-100", cx)
    })
    .unwrap();
    settle(cx, "100 rows", |cx| {
        status(cx, &app).starts_with("Showing 1–100 of 1,000")
    });
    cx.read_entity(&tab, |t, _| assert_eq!(t.loaded_rows(), 100));

    cx.update_window(app.window, |_, window, cx| {
        window.click("page-size-1000", cx)
    })
    .unwrap();
    settle(cx, "all rows", |cx| {
        status(cx, &app).starts_with("Showing 1–1,000 of 1,000")
    });
    cx.update_window(app.window, |_, window, _| {
        assert_eq!(
            window.find("page-next").disabled(),
            Some(true),
            "nothing after the last row"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_missing_table_reports_the_error_instead_of_hanging(cx: &mut TestAppContext) {
    let env = Env::new("missing");
    let (_service, app) = start(cx, &env);
    settle(cx, "schema", |cx| {
        cx.read_entity(&app.view, |v, _| v.object_exists(&app.path))
    });
    let ghost = ObjectPath::new(Some("main"), "no_such_table");
    cx.update_window(app.window, |_, window, cx| {
        app.view
            .update(cx, |v, cx| v.open_table(&ghost, window, cx));
    })
    .unwrap();
    settle(cx, "the error", |cx| {
        cx.update_window(app.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find("grid-error").is_some()
        })
        .unwrap()
    });
}

#[gpui_kit::test]
fn the_session_is_saved_and_restored_with_table_view_state(cx: &mut TestAppContext) {
    let env = Env::new("session");
    let (service, app) = start(cx, &env);
    let tab = open_items(cx, &app);
    settle(cx, "first page", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
    });
    cx.update_window(app.window, |_, _, cx| {
        tab.update(cx, |t, cx| t.toggle_sort("name", cx))
    })
    .unwrap();
    cx.update_window(app.window, |_, window, cx| {
        window.click("page-size-100", cx)
    })
    .unwrap();
    settle(cx, "reloaded", |cx| {
        status(cx, &app).starts_with("Showing 1–100 of 1,000")
    });

    // The save is debounced by a timer, and GPUI's test clock only moves when told to.
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let id = service_connection_id(&service);
    settle(cx, "the workspace to be saved", |_| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let svc = service.clone();
        let id = id.clone();
        rt.block_on(async move {
            svc.load_workspace(&id)
                .await
                .unwrap()
                .is_some_and(|w| !w.tabs.is_empty())
        })
    });

    // A second window on the same connection comes back with the same tab and the same sorting.
    let service2 = service.clone();
    let conn = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let id = id.clone();
        rt.block_on(async move { service2.open(&id).await.unwrap() })
    };
    let (window2, view2) = cx
        .update(|cx| open_connection_window(service.clone(), conn, cx))
        .unwrap();
    settle(cx, "the tab to come back", |cx| {
        cx.read_entity(&view2, |v, _| !v.tabs().is_empty())
    });
    let tab2 = cx.read_entity(&view2, |v, _| {
        let id = v.tabs().active_id().unwrap().clone();
        v.tab_view(&id).unwrap().clone()
    });
    settle(cx, "restored view state", |cx| {
        cx.read_entity(&tab2, |t, _| {
            let q = t.query().to_table_query();
            q.limit == 100 && q.sort.first().is_some_and(|s| s.column == "name")
        }) && cx.read_entity(&tab2, |t, _| t.loaded_rows() == 100)
    });
    let _ = window2;
}

fn service_connection_id(service: &Service) -> fg_core::ConnectionId {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let svc = service.clone();
    rt.block_on(async move {
        match svc.connections().await.unwrap().remove(0) {
            fg_store::StoredConnection::Ok(c) => c.id,
            other => panic!("{other:?}"),
        }
    })
}

#[gpui_kit::test]
fn filters_applied_in_the_drawer_reach_the_database(cx: &mut TestAppContext) {
    use fg_app::model::{DraftRow, FilterDraft};
    use fg_core::FilterOp;

    let env = Env::new("filter");
    let (_service, app) = start(cx, &env);
    let tab = open_items(cx, &app);
    settle(cx, "first page", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
    });

    // score = (id * 7) % 10, so "score >= 8" keeps exactly these rows:
    let expected: Vec<i64> = (1..=1000).filter(|i| (i * 7) % 10 >= 8).collect();
    assert_eq!(expected.len(), 200);

    cx.update_window(app.window, |_, window, cx| {
        tab.update(cx, |t, cx| t.toggle_filters(cx));
        let drawer = tab.read(cx).filter_drawer().clone();
        drawer.update(cx, |d, cx| {
            d.load(
                FilterDraft {
                    rows: vec![DraftRow {
                        op: FilterOp::Ge,
                        value: "8".into(),
                        ..DraftRow::new("score")
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

    settle(cx, "the filtered page and count", |cx| {
        status(cx, &app).starts_with("Showing 1–200 of 200")
    });
    cx.read_entity(&tab, |t, cx| {
        assert_eq!(t.loaded_rows(), 200);
        for (row, id) in expected.iter().enumerate() {
            assert_eq!(t.value_at(row, 0, cx), Some(Value::Int(*id)), "row {row}");
        }
    });

    // A text filter combined with it: names containing "99" among those.
    cx.update_window(app.window, |_, window, cx| {
        let drawer = tab.read(cx).filter_drawer().clone();
        drawer.update(cx, |d, cx| {
            d.load(
                FilterDraft {
                    rows: vec![
                        DraftRow {
                            op: FilterOp::Ge,
                            value: "8".into(),
                            ..DraftRow::new("score")
                        },
                        DraftRow {
                            op: FilterOp::Contains,
                            value: "99".into(),
                            ..DraftRow::new("name")
                        },
                    ],
                    ..Default::default()
                },
                cx,
            )
        });
        window.render_frame(cx);
        window.click("filter-apply", cx);
    })
    .unwrap();
    let both: Vec<i64> = expected
        .iter()
        .copied()
        .filter(|i| i.to_string().contains("99"))
        .collect();
    settle(cx, "the combined filter", |cx| {
        status(cx, &app).starts_with(&format!("Showing 1–{} of {}", both.len(), both.len()))
    });

    // Raw SQL mode reaches the database too.
    cx.update_window(app.window, |_, window, cx| {
        let drawer = tab.read(cx).filter_drawer().clone();
        drawer.update(cx, |d, cx| {
            d.load(
                FilterDraft {
                    raw_mode: true,
                    raw_where: "id <= 10 AND note IS NULL".into(),
                    ..Default::default()
                },
                cx,
            )
        });
        window.render_frame(cx);
        window.click("filter-apply", cx);
    })
    .unwrap();
    // Only id 10 has i % 100 = 0 ... none of 1..=10 do, so nothing matches.
    settle(cx, "the raw filter", |cx| {
        status(cx, &app).starts_with("No rows")
    });
}

#[gpui_kit::test]
fn an_unsafe_raw_where_is_rejected_by_the_service_and_shown(cx: &mut TestAppContext) {
    use fg_app::model::FilterDraft;

    let env = Env::new("unsafe");
    let (_service, app) = start(cx, &env);
    let tab = open_items(cx, &app);
    settle(cx, "first page", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
    });

    cx.update_window(app.window, |_, window, cx| {
        tab.update(cx, |t, cx| t.toggle_filters(cx));
        let drawer = tab.read(cx).filter_drawer().clone();
        drawer.update(cx, |d, cx| {
            d.load(
                FilterDraft {
                    raw_mode: true,
                    raw_where: "1=1; DROP TABLE items".into(),
                    ..Default::default()
                },
                cx,
            )
        });
        window.render_frame(cx);
        window.click("filter-apply", cx);
    })
    .unwrap();
    settle(cx, "the error", |cx| {
        cx.update_window(app.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find("grid-error").is_some()
        })
        .unwrap()
    });
    // And the table is still there.
    cx.update_window(app.window, |_, _, cx| {
        tab.update(cx, |t, cx| t.refresh(cx));
    })
    .unwrap();
}

#[gpui_kit::test]
fn following_a_foreign_key_opens_the_referenced_row(cx: &mut TestAppContext) {
    let env = Env::new("fk");
    let (service, app) = start(cx, &env);

    // Two more tables, linked by a foreign key.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let svc = service.clone();
    rt.block_on(async move {
        let id = match svc.connections().await.unwrap().remove(0) {
            fg_store::StoredConnection::Ok(c) => c.id,
            other => panic!("{other:?}"),
        };
        let conn = svc.open(&id).await.unwrap();
        conn.execute(
            "CREATE TABLE authors (id INTEGER PRIMARY KEY, author_name TEXT);
             CREATE TABLE books (id INTEGER PRIMARY KEY, title TEXT, author_id INTEGER REFERENCES authors(id));
             INSERT INTO authors VALUES (1, 'Ann'), (2, 'Bob'), (3, 'Cy');
             INSERT INTO books VALUES (10, 'Rust', 2), (11, 'SQL', 3), (12, 'None', NULL);",
            &[],
            ExecOptions::default(),
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
        conn.refresh(&fg_service::Refresh::Everything);
    });
    // Reload the sidebar's schema so it lists the new tables.
    cx.update_window(app.window, |_, window, cx| {
        window.press("secondary-r", cx);
    })
    .unwrap();
    let books = ObjectPath::new(Some("main"), "books");
    settle(cx, "the new tables to be listed", |cx| {
        cx.read_entity(&app.view, |v, _| v.object_exists(&books))
    });

    cx.update_window(app.window, |_, window, cx| {
        app.view
            .update(cx, |v, cx| v.open_table(&books, window, cx));
    })
    .unwrap();
    let books_tab = cx.read_entity(&app.view, |v, _| {
        let id = v.tabs().active_id().unwrap().clone();
        v.tab_view(&id).unwrap().clone()
    });
    settle(cx, "books to load with its foreign key", |cx| {
        status(cx, &app).starts_with("Showing 1–3 of 3")
            && cx.read_entity(&books_tab, |t, cx| t.has_foreign_key("author_id", cx))
    });

    // books.author_id is the third column; the first row (id 10) points at author 2.
    cx.update_window(app.window, |_, window, cx| {
        window.render_frame(cx);
        window.click("fk-0-2", cx);
    })
    .unwrap();

    settle(cx, "authors to open filtered", |cx| {
        status(cx, &app).starts_with("Showing 1–1 of 1")
    });
    let active = cx
        .read_entity(&app.view, |v, _| v.tabs().active().cloned())
        .unwrap();
    assert!(matches!(&active.kind, fg_core::TabKind::Table { path, .. } if path.name == "authors"));
    let authors_tab = cx.read_entity(&app.view, |v, _| v.tab_view(&active.id).unwrap().clone());
    cx.read_entity(&authors_tab, |t, cx| {
        assert_eq!(t.value_at(0, 1, cx), Some(Value::Text("Bob".into())));
        assert_eq!(
            t.query().active_filter_count(),
            1,
            "the drawer shows the filter that was applied"
        );
    });
    // The books tab is still open behind it.
    assert_eq!(cx.read_entity(&app.view, |v, _| v.tabs().tabs().len()), 2);
}

#[gpui_kit::test]
fn hidden_columns_are_remembered_for_the_next_time(cx: &mut TestAppContext) {
    let env = Env::new("hidden");
    let (service, app) = start(cx, &env);
    let tab = open_items(cx, &app);
    settle(cx, "first page", |cx| {
        status(cx, &app).starts_with("Showing 1–300 of 1,000")
    });

    cx.update_window(app.window, |_, _, cx| {
        tab.update(cx, |t, cx| t.toggle_column("note", cx))
    })
    .unwrap();

    let id = service_connection_id(&service);
    let path = app.path.clone();
    settle(cx, "the hidden column to be saved", |_| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (svc, id, path) = (service.clone(), id.clone(), path.clone());
        rt.block_on(async move {
            svc.load_table_view(&id, &path)
                .await
                .unwrap()
                .is_some_and(|v| v.hidden_columns == ["note".to_string()])
        })
    });
}
