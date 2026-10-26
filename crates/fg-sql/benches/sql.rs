#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Benchmarks for the SQL front end: statement splitting, classification,
//! and analysis on a realistic multi-statement script. These exist so a
//! regression in the lexer shows up in CI timings, not in user-visible
//! editor latency; they assert nothing about results beyond a sanity
//! check that the splitter still sees many statements.

use criterion::{Criterion, criterion_group, criterion_main};
use fg_core::DbKind;
use fg_sql::{analyze, classify, split_statements};

const SCRIPT: &str = r"-- users and their orders; note the semicolon inside this comment;
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    email VARCHAR(255) UNIQUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    total DECIMAL(10, 2) NOT NULL DEFAULT 0,
    note TEXT,
    placed_at DATE
);
/* a block comment; with a semicolon and 'quoted' text */
INSERT INTO users (id, name, email) VALUES
    (1, 'Ann O''Brien', 'ann@example.com'),
    (2, 'Bob; the builder', 'bob@example.com'),
    (3, 'Cyd', 'cyd@example.com');
INSERT INTO orders (id, user_id, total, note) VALUES
    (1, 1, 19.99, 'first; order'),
    (2, 1, 5.50, NULL),
    (3, 2, 120.00, 'gift');
SELECT u.name, COUNT(o.id) AS order_count, SUM(o.total) AS spent
FROM users u LEFT JOIN orders o ON o.user_id = u.id
WHERE u.created_at >= '2026-01-01' AND o.total > 0
GROUP BY u.id, u.name
ORDER BY spent DESC
LIMIT 50;
SELECT * FROM orders WHERE note LIKE '%;%' ESCAPE '\\';
UPDATE users SET email = 'ann.new@example.com' WHERE id = 1;
DELETE FROM orders WHERE total < 0;
CREATE INDEX orders_user_idx ON orders (user_id);
DROP TABLE IF EXISTS temp_import;
";

const SELECT: &str = r"SELECT u.name, COUNT(o.id) AS order_count, SUM(o.total) AS spent
FROM users u LEFT JOIN orders o ON o.user_id = u.id
WHERE u.created_at >= '2026-01-01' AND o.total > 0
GROUP BY u.id, u.name
ORDER BY spent DESC
LIMIT 50";

const UPDATE: &str = "UPDATE users SET email = 'ann.new@example.com' WHERE id = 1";

const CREATE: &str = r"CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    email VARCHAR(255) UNIQUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
)";

fn bench_split(c: &mut Criterion) {
    // Sanity: the script holds exactly 10 statements; fewer would mean
    // the splitter merged some, and the timed loop would measure a
    // degenerate path.
    for kind in [DbKind::Sqlite, DbKind::Postgres, DbKind::MySql] {
        assert!(split_statements(SCRIPT, kind).len() >= 10);
    }
    let mut group = c.benchmark_group("split");
    for kind in [DbKind::Sqlite, DbKind::Postgres, DbKind::MySql] {
        group.bench_function(format!("{kind:?}"), |b| {
            b.iter(|| split_statements(std::hint::black_box(SCRIPT), std::hint::black_box(kind)))
        });
    }
    group.finish();
}

fn bench_classify(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify");
    for (name, sql) in [("select", SELECT), ("update", UPDATE), ("create", CREATE)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                classify(
                    std::hint::black_box(sql),
                    std::hint::black_box(DbKind::Sqlite),
                )
            })
        });
    }
    group.finish();
}

fn bench_analyze(c: &mut Criterion) {
    let mut group = c.benchmark_group("analyze");
    for (name, sql) in [("select", SELECT), ("update", UPDATE)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                analyze(
                    std::hint::black_box(sql),
                    std::hint::black_box(DbKind::Sqlite),
                )
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_split, bench_classify, bench_analyze);
criterion_main!(benches);
