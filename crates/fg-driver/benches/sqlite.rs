#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Benchmarks for the SQLite row path: a full-table scan of 10k rows,
//! decoded and drained through the driver stream. This is the decode
//! benchmark the roadmap asks for: a regression in value decoding or
//! batching shows up here before it shows up in grid scroll latency.

use criterion::{Criterion, criterion_group, criterion_main};
use fg_core::{ConnectionConfig, ConnectionParams, DbKind};
use fg_driver::{ExecOptions, Session, driver_for};

const ROWS: usize = 10_000;

fn seeded(rt: &tokio::runtime::Runtime) -> Box<dyn Session> {
    let config = ConnectionConfig::new(
        "bench",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: ":memory:".into(),
        },
    );
    let session = rt
        .block_on(driver_for(DbKind::Sqlite).unwrap().connect(&config, None))
        .unwrap();
    rt.block_on(async {
        session
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, price REAL, payload TEXT)",
                &[],
                ExecOptions::default(),
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        // 100 statements of 100 rows each: multi-row VALUES keeps setup
        // fast without tripping SQLite's variable limit.
        for batch in 0..(ROWS / 100) {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..100 {
                let id = batch * 100 + i;
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!(
                    "({id}, 'name-{id}', {}.99, 'payload-{id}-xxxxxxxxxxxxxxxxxxxx')",
                    id % 100
                ));
            }
            session
                .execute(&sql, &[], ExecOptions::default())
                .await
                .unwrap()
                .collect()
                .await
                .unwrap();
        }
    });
    session
}

fn bench_scan(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let session = seeded(&rt);
    // Sanity: the fixture really holds ROWS rows, otherwise the timed
    // loop would measure a short scan.
    let sets = rt.block_on(async {
        session
            .execute(
                "SELECT id, name, price, payload FROM t",
                &[],
                ExecOptions::default(),
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
    });
    assert_eq!(sets.iter().map(|set| set.rows.len()).sum::<usize>(), ROWS);
    c.bench_function("sqlite_scan_10k", |b| {
        b.iter(|| {
            let sets = rt.block_on(async {
                session
                    .execute(
                        std::hint::black_box("SELECT id, name, price, payload FROM t"),
                        &[],
                        ExecOptions::default(),
                    )
                    .await
                    .unwrap()
                    .collect()
                    .await
                    .unwrap()
            });
            std::hint::black_box(sets);
        })
    });
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
