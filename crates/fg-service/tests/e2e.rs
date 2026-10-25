//! Service-level tests against real SQLite files and the real encrypted store. The service owns
//! its own tokio runtime while these tests run on another one, which also checks that the public
//! API works when awaited from a foreign executor.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fg_core::{
    ConnectionConfig, ConnectionGroup, ConnectionParams, DbKind, Error, Filter, FilterOp,
    GridSettings, NullsOrder, ObjectPath, SafeMode, SortKey, TableQuery, Value,
};
use fg_driver::{Event, ExecOptions};
use fg_service::{
    BrowseOptions, KeychainKeyProvider, MemoryBackend, OpenConnection, PasswordUpdate, Refresh,
    Service, ServiceEvent, ServiceOptions,
};
use fg_store::StoredConnection;

struct Env {
    dir: PathBuf,
    keys: KeychainKeyProvider<MemoryBackend>,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("fg-svc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            keys: KeychainKeyProvider::with_backend(MemoryBackend::default()),
        }
    }

    fn service(&self) -> Service {
        self.service_with(|_| {})
    }

    fn service_with(&self, tweak: impl FnOnce(&mut ServiceOptions)) -> Service {
        let mut options = ServiceOptions::new(self.dir.join("store.db"));
        tweak(&mut options);
        Service::start(options, &self.keys).unwrap()
    }

    fn sqlite(&self, name: &str) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(
            name,
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self
                    .dir
                    .join(format!("{name}.db"))
                    .to_string_lossy()
                    .into_owned(),
            },
        );
        c.options.insert("create".into(), "true".into());
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn run(conn: &Arc<OpenConnection>, sql: &str) -> Vec<fg_driver::ResultSet> {
    conn.execute(sql, &[], ExecOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
}

/// A connection with 1000 rows: `score` has many duplicates (to exercise stable paging).
async fn seeded(env: &Env, svc: &Service, name: &str) -> Arc<OpenConnection> {
    let saved = svc
        .save_connection(env.sqlite(name), PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&saved.id).await.unwrap();
    run(
        &conn,
        "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER, note TEXT);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
         INSERT INTO items SELECT i, 'item' || i, i % 10, CASE WHEN i % 100 = 0 THEN NULL ELSE 'note ' || i END FROM n;",
    )
    .await;
    conn
}

fn items() -> TableQuery {
    TableQuery::new(ObjectPath::new(Some("main"), "items"), 100)
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => panic!("not an int: {other:?}"),
    }
}

// ---- connections, secrets, lifecycle ----------------------------------------------------------

#[tokio::test]
async fn saved_connections_survive_a_restart_and_keep_their_password_encrypted() {
    let env = Env::new("restart");
    let id = {
        let svc = env.service();
        let saved = svc
            .save_connection(env.sqlite("main"), PasswordUpdate::Set("hunter2".into()))
            .await
            .unwrap();
        let g = svc.save_group(ConnectionGroup::new("Work")).await.unwrap();
        assert_eq!(svc.groups().await.unwrap(), vec![g]);
        saved.id
    };
    // The password never reaches the disk in clear.
    for entry in std::fs::read_dir(&env.dir).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"hunter2"));
    }
    let svc = env.service();
    let list = svc.connections().await.unwrap();
    assert!(matches!(&list[..], [StoredConnection::Ok(c)] if c.id == id && c.name == "main"));
}

#[tokio::test]
async fn a_wiped_keychain_is_reported_and_nothing_is_overwritten() {
    let env = Env::new("keychain");
    drop(env.service());
    let wiped = KeychainKeyProvider::with_backend(MemoryBackend::default());
    let err = Service::start(ServiceOptions::new(env.dir.join("store.db")), &wiped).err();
    assert!(matches!(err, Some(Error::KeyUnavailable(_))), "{err:?}");
    // The original keychain still opens it.
    drop(env.service());
}

#[tokio::test]
async fn open_emits_events_and_close_forgets_the_connection() {
    let env = Env::new("events");
    let svc = env.service();
    let mut events = svc.subscribe();
    let saved = svc
        .save_connection(env.sqlite("a"), PasswordUpdate::Keep)
        .await
        .unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        ServiceEvent::ConnectionsChanged
    );

    svc.open(&saved.id).await.unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        ServiceEvent::Connected(saved.id.clone())
    );
    assert_eq!(svc.open_connections(), vec![saved.id.clone()]);

    svc.close(&saved.id);
    assert_eq!(
        events.recv().await.unwrap(),
        ServiceEvent::Disconnected(saved.id.clone())
    );
    assert!(svc.open_connections().is_empty());
}

#[tokio::test]
async fn editing_or_deleting_a_connection_closes_it() {
    let env = Env::new("edit");
    let svc = env.service();
    let saved = svc
        .save_connection(env.sqlite("a"), PasswordUpdate::Keep)
        .await
        .unwrap();
    svc.open(&saved.id).await.unwrap();

    let mut renamed = saved.clone();
    renamed.name = "renamed".into();
    svc.save_connection(renamed, PasswordUpdate::Keep)
        .await
        .unwrap();
    assert!(
        svc.open_connections().is_empty(),
        "the old settings must not stay in use"
    );

    svc.open(&saved.id).await.unwrap();
    assert!(svc.delete_connection(&saved.id).await.unwrap());
    assert!(svc.open(&saved.id).await.is_err());
}

#[tokio::test]
async fn test_connection_reports_version_and_failures() {
    let env = Env::new("test");
    let svc = env.service();
    let ok = svc
        .test_connection(env.sqlite("fresh"), None, None)
        .await
        .unwrap();
    assert!(ok.server_version.starts_with("SQLite 3"));

    let mut missing = env.sqlite("missing");
    missing.options.clear(); // no create=true: a typo'd path must fail, not create a file
    assert!(svc.test_connection(missing, None, None).await.is_err());
}

#[tokio::test]
async fn opening_a_connection_that_cannot_connect_fails_immediately() {
    let env = Env::new("badopen");
    let svc = env.service();
    let mut c = env.sqlite("nope");
    c.options.clear();
    let saved = svc.save_connection(c, PasswordUpdate::Keep).await.unwrap();
    assert!(svc.open(&saved.id).await.is_err());
    assert!(svc.open_connections().is_empty());
}

// ---- per-connection behaviour ----------------------------------------------------------------

#[tokio::test]
async fn startup_sql_runs_on_every_session() {
    let env = Env::new("startup");
    let svc = env.service_with(|o| o.max_sessions_per_connection = 1);
    let mut cfg = env.sqlite("boot");
    // Startup SQL may set session state. It may not create tables: that is a write, and a
    // read-only connection must be able to refuse it. A pragma still proves the script ran.
    cfg.startup_sql = vec!["PRAGMA cache_size = 1234".into()];
    let saved = svc
        .save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&saved.id).await.unwrap();
    let sets = run(&conn, "PRAGMA cache_size").await;
    assert_eq!(int(&sets[0].rows[0][0]).abs(), 1234);
}

#[tokio::test]
async fn read_only_safe_mode_is_enforced_by_the_session() {
    let env = Env::new("readonly");
    let svc = env.service();
    let seeded_conn = seeded(&env, &svc, "ro").await;
    let id = seeded_conn.id().clone();
    let mut cfg = match svc.connections().await.unwrap().remove(0) {
        StoredConnection::Ok(c) => c,
        other => panic!("{other:?}"),
    };
    cfg.safe_mode = Some(SafeMode::ReadOnly);
    svc.save_connection(cfg, PasswordUpdate::Keep)
        .await
        .unwrap();

    let conn = svc.open(&id).await.unwrap();
    assert_eq!(
        int(&run(&conn, "SELECT count(*) FROM items").await[0].rows[0][0]),
        1000
    );
    let err = match conn
        .execute("DELETE FROM items", &[], ExecOptions::default())
        .await
    {
        Err(err) => err,
        Ok(_) => panic!("a read-only write must be refused before the engine"),
    };
    assert!(
        matches!(err, Error::PolicyDenied(_)),
        "a read-only write is refused before the engine: {err:?}"
    );
}

#[tokio::test]
async fn metadata_is_cached_until_refreshed() {
    let env = Env::new("cache");
    let svc = env.service();
    let conn = seeded(&env, &svc, "meta").await;

    assert_eq!(conn.namespaces().await.unwrap(), vec!["main"]);
    let before = conn.objects(Some("main")).await.unwrap();
    assert_eq!(before.len(), 1);

    run(
        &conn,
        "CREATE TABLE extra (id INTEGER PRIMARY KEY, item_id INTEGER REFERENCES items(id))",
    )
    .await;
    assert_eq!(
        conn.objects(Some("main")).await.unwrap().len(),
        1,
        "still cached"
    );

    let mut events = svc.subscribe();
    conn.refresh(&Refresh::Everything);
    assert!(matches!(
        events.recv().await.unwrap(),
        ServiceEvent::MetadataChanged { .. }
    ));
    assert_eq!(conn.objects(Some("main")).await.unwrap().len(), 2);

    let detail = conn
        .describe(&ObjectPath::new(Some("main"), "items"))
        .await
        .unwrap();
    assert_eq!(detail.primary_key(), vec!["id"]);
    let rels = conn.relations(Some("main")).await.unwrap();
    assert_eq!((rels.len(), rels[0].from.name.as_str()), (1, "extra"));
    assert!(
        conn.ddl(&ObjectPath::new(Some("main"), "items"))
            .await
            .unwrap()
            .unwrap()
            .contains("CREATE TABLE items")
    );
}

// ---- browsing --------------------------------------------------------------------------------

#[tokio::test]
async fn browse_filters_sorts_and_counts() {
    let env = Env::new("browse");
    let svc = env.service();
    let conn = seeded(&env, &svc, "b").await;

    let mut q = items();
    q.filters = vec![
        Filter {
            column: "score".into(),
            op: FilterOp::Ge,
            values: vec![Value::Int(8)],
            enabled: true,
        },
        Filter {
            column: "name".into(),
            op: FilterOp::Contains,
            values: vec![Value::Text("9".into())],
            enabled: true,
        },
    ];
    q.sort = vec![SortKey {
        column: "id".into(),
        descending: true,
        nulls: None,
    }];
    q.limit = 5;

    let sets = conn
        .browse(&q, BrowseOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let ids: Vec<i64> = sets[0].rows.iter().map(|r| int(&r[0])).collect();
    assert_eq!(ids.len(), 5);
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "descending: {ids:?}");
    assert!(sets[0].rows.iter().all(|r| int(&r[2]) >= 8));
    assert_eq!(
        sets[0].columns[0].origin.as_ref().map(|o| o.table.as_str()),
        Some("items")
    );

    let total = conn.count(&q, BrowseOptions::default()).await.unwrap();
    assert!(total > 5 && total < 1000);
    assert_eq!(
        conn.count(&items(), BrowseOptions::default())
            .await
            .unwrap(),
        1000
    );
}

#[tokio::test]
async fn paging_an_unsorted_or_duplicate_heavy_sort_is_stable() {
    let env = Env::new("paging");
    let svc = env.service();
    let conn = seeded(&env, &svc, "p").await;

    // `score` has only 10 distinct values: without the primary-key tie-breaker, pages would
    // overlap and skip rows.
    let mut seen = Vec::new();
    for page in 0..10u64 {
        let mut q = items();
        q.sort = vec![SortKey {
            column: "score".into(),
            descending: false,
            nulls: Some(NullsOrder::Last),
        }];
        q.offset = page * 100;
        let sets = conn
            .browse(&q, BrowseOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        seen.extend(sets[0].rows.iter().map(|r| int(&r[0])));
    }
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!((seen.len(), unique.len()), (1000, 1000));
}

#[tokio::test]
async fn user_values_are_parameters_never_sql() {
    let env = Env::new("inject");
    let svc = env.service();
    let conn = seeded(&env, &svc, "i").await;
    let evil = "x'; DROP TABLE items; --";
    conn.execute(
        "UPDATE items SET name = ?1 WHERE id = 1",
        &[Value::Text(evil.into())],
        ExecOptions::default(),
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();

    let mut q = items();
    q.filters = vec![Filter {
        column: "name".into(),
        op: FilterOp::Eq,
        values: vec![Value::Text(evil.into())],
        enabled: true,
    }];
    let sets = conn
        .browse(&q, BrowseOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(sets[0].rows.len(), 1);
    assert_eq!(
        conn.count(&items(), BrowseOptions::default())
            .await
            .unwrap(),
        1000,
        "table intact"
    );
}

#[tokio::test]
async fn raw_where_is_validated_and_can_be_forbidden() {
    let env = Env::new("raw");
    let svc = env.service();
    let conn = seeded(&env, &svc, "r").await;

    let mut q = items();
    q.raw_where = Some("score > 7 AND name LIKE 'item9%'".into());
    let ok = conn
        .browse(&q, BrowseOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert!(!ok[0].rows.is_empty());

    let agent = BrowseOptions {
        allow_raw_where: false,
    };
    assert!(matches!(
        conn.browse(&q, agent).await.err(),
        Some(Error::PolicyDenied(_))
    ));

    q.raw_where = Some("1=1; DROP TABLE items".into());
    assert!(matches!(
        conn.browse(&q, BrowseOptions::default()).await.err(),
        Some(Error::InvalidInput(_))
    ));
    assert_eq!(
        conn.count(&items(), BrowseOptions::default())
            .await
            .unwrap(),
        1000
    );
}

#[tokio::test]
async fn oversized_cells_arrive_truncated_according_to_the_setting() {
    let env = Env::new("cells");
    let svc = env.service();
    let conn = seeded(&env, &svc, "c").await;
    svc.set_grid_settings(GridSettings {
        max_cell_bytes: 6,
        ..GridSettings::default()
    })
    .await
    .unwrap();

    let mut q = items();
    q.limit = 1;
    let sets = conn
        .browse(&q, BrowseOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    // "item1" fits, "note 1" does not fit in... 6 bytes exactly fits; use a longer row instead.
    assert!(matches!(sets[0].rows[0][1], Value::Text(_)));
    let mut q = items();
    q.filters = vec![Filter {
        column: "id".into(),
        op: FilterOp::Eq,
        values: vec![Value::Int(1000)],
        enabled: true,
    }];
    let sets = conn
        .browse(&q, BrowseOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert!(
        matches!(sets[0].rows[0][1], Value::Truncated(ref t) if t.total_len == 8),
        "{:?}",
        sets[0].rows[0]
    );
}

// ---- tasks and cancellation ------------------------------------------------------------------

const LONG: &str = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000000000) SELECT sum(i) FROM n";

#[tokio::test]
async fn cancelling_a_task_stops_it_and_the_session_is_immediately_reusable() {
    let env = Env::new("cancel");
    let svc = env.service_with(|o| o.max_sessions_per_connection = 1);
    let conn = seeded(&env, &svc, "x").await;

    for delay_ms in [0u64, 1, 5, 20, 60] {
        let task = conn
            .execute(LONG, &[], ExecOptions::default())
            .await
            .unwrap();
        let canceller = task.canceller();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            canceller.cancel();
        });
        let result = tokio::time::timeout(Duration::from_secs(10), task.collect())
            .await
            .expect("not cancelled");
        assert!(
            matches!(result, Err(Error::Cancelled)),
            "delay {delay_ms}: {result:?}"
        );

        // The next task on the same (only) session must be unaffected.
        let sets = tokio::time::timeout(Duration::from_secs(5), run(&conn, "SELECT 1"))
            .await
            .unwrap();
        assert_eq!(int(&sets[0].rows[0][0]), 1);
    }
}

#[tokio::test]
async fn a_late_cancel_cannot_hit_the_next_task() {
    let env = Env::new("late");
    let svc = env.service_with(|o| o.max_sessions_per_connection = 1);
    let conn = seeded(&env, &svc, "x").await;

    for _ in 0..25 {
        let first = conn
            .execute("SELECT 1", &[], ExecOptions::default())
            .await
            .unwrap();
        let stale = first.canceller();
        first.collect().await.unwrap();

        // A new task starts on the same session, and only then does the stale cancel fire.
        let second = conn
            .execute("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300000) SELECT count(*) FROM n", &[], ExecOptions::default())
            .await
            .unwrap();
        stale.cancel();
        let sets = second
            .collect()
            .await
            .expect("a finished task's cancel must not affect the next one");
        assert_eq!(int(&sets[0].rows[0][0]), 300_000);
    }
}

#[tokio::test]
async fn dropping_an_unfinished_stream_cancels_it_and_frees_the_session() {
    let env = Env::new("abandon");
    let svc = env.service_with(|o| o.max_sessions_per_connection = 1);
    let conn = seeded(&env, &svc, "x").await;

    let task = conn
        .execute(LONG, &[], ExecOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(task);

    let sets = tokio::time::timeout(Duration::from_secs(10), run(&conn, "SELECT 2"))
        .await
        .expect("the abandoned query kept the session busy");
    assert_eq!(int(&sets[0].rows[0][0]), 2);
}

#[tokio::test]
async fn independent_tasks_run_in_parallel_on_a_pool() {
    let env = Env::new("parallel");
    let svc = env.service();
    let conn = seeded(&env, &svc, "x").await;

    // Hold one session with a long query; metadata and a page must still come back.
    let busy = conn
        .execute(LONG, &[], ExecOptions::default())
        .await
        .unwrap();
    let page = tokio::time::timeout(
        Duration::from_secs(5),
        conn.browse(&items(), BrowseOptions::default())
            .await
            .unwrap()
            .collect(),
    )
    .await
    .expect("a free session should serve the page")
    .unwrap();
    assert_eq!(page[0].rows.len(), 100);
    assert_eq!(conn.objects(Some("main")).await.unwrap().len(), 1);
    busy.cancel();
}

#[tokio::test]
async fn streaming_delivers_rows_in_batches_before_the_end() {
    let env = Env::new("stream");
    let svc = env.service();
    let conn = seeded(&env, &svc, "x").await;
    let mut task = conn
        .execute(
            "SELECT id FROM items",
            &[],
            ExecOptions {
                batch_size: 300,
                ..ExecOptions::default()
            },
        )
        .await
        .unwrap();
    let mut sizes = Vec::new();
    while let Some(ev) = task.next().await {
        if let Event::Rows(rows) = ev.unwrap() {
            sizes.push(rows.len());
        }
    }
    assert_eq!(sizes, vec![300, 300, 300, 100]);
}

// ---- UI state --------------------------------------------------------------------------------

#[tokio::test]
async fn workspace_and_table_view_state_persist_per_connection() {
    let env = Env::new("ui");
    let svc = env.service();
    let a = svc
        .save_connection(env.sqlite("a"), PasswordUpdate::Keep)
        .await
        .unwrap();
    let path = ObjectPath::new(Some("main"), "items");

    assert_eq!(svc.load_workspace(&a.id).await.unwrap(), None);
    let ws = fg_core::WorkspaceState {
        sidebar_width: Some(260.0),
        ..Default::default()
    };
    svc.save_workspace(&a.id, ws.clone()).await.unwrap();
    assert_eq!(svc.load_workspace(&a.id).await.unwrap(), Some(ws));

    let view = fg_core::TableViewState {
        pinned_columns: 2,
        ..Default::default()
    };
    svc.save_table_view(&a.id, &path, view.clone())
        .await
        .unwrap();
    assert_eq!(svc.load_table_view(&a.id, &path).await.unwrap(), Some(view));

    // And across a restart.
    drop(svc);
    let again = env.service();
    assert!(again.load_workspace(&a.id).await.unwrap().is_some());
}

/// Roadmap M1 acceptance: first screen of a 1M-row table in under 200 ms.
/// Run by hand: `cargo test -p fg-service --release --test e2e -- --ignored --nocapture perf_`
#[tokio::test]
#[ignore = "performance measurement, run by hand in release mode"]
async fn perf_first_page_of_a_million_rows() {
    use std::time::Instant;

    let env = Env::new("perf");
    let svc = env.service();
    let saved = svc
        .save_connection(env.sqlite("big"), PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = svc.open(&saved.id).await.unwrap();
    let seeding = Instant::now();
    run(
        &conn,
        "CREATE TABLE big (id INTEGER PRIMARY KEY, name TEXT, score INTEGER, note TEXT);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000000)
         INSERT INTO big SELECT i, 'name ' || i, i % 1000, 'some note text for row ' || i FROM n;",
    )
    .await;
    eprintln!("seeded 1,000,000 rows in {:?}", seeding.elapsed());
    // Warm the metadata cache the way opening the sidebar would.
    conn.describe(&ObjectPath::new(Some("main"), "big"))
        .await
        .unwrap();

    let query = TableQuery::new(ObjectPath::new(Some("main"), "big"), 300);
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let mut stream = conn.browse(&query, BrowseOptions::default()).await.unwrap();
        let mut first_rows = None;
        while let Some(ev) = stream.next().await {
            if let Event::Rows(_) = ev.unwrap() {
                first_rows.get_or_insert(started.elapsed());
            }
        }
        samples.push((first_rows.unwrap(), started.elapsed()));
    }
    for (first, total) in &samples {
        eprintln!("first rows after {first:?}, page complete after {total:?}");
    }
    let count_started = Instant::now();
    let total = conn.count(&query, BrowseOptions::default()).await.unwrap();
    eprintln!("COUNT(*) = {total} in {:?}", count_started.elapsed());

    // A page far into the table, sorted by an unindexed column: the slow, honest case.
    let mut deep = query.clone();
    deep.offset = 900_000;
    let started = Instant::now();
    conn.browse(&deep, BrowseOptions::default())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    eprintln!(
        "page at offset 900,000 (primary-key order) in {:?}",
        started.elapsed()
    );

    let worst_first = samples.iter().map(|(f, _)| *f).max().unwrap();
    assert!(
        worst_first < Duration::from_millis(200),
        "first rows took {worst_first:?}"
    );
}
