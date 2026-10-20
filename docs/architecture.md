# Architecture

## Layering

```
fg-app (GPUI)   fg-cli (headless)
      \            /
       fg-service  <---- fg-mcp (stdio / local HTTP)
      /    |     \
fg-driver fg-store fg-sql
      \    |     /
        fg-core
```

Rule: dependencies point downward only. `fg-core` has no IO. `fg-app` is the only crate that
depends on GPUI. `fg-mcp` and `fg-cli` must work without a display.

The multi-database contracts (Driver / Session / Dialect / Capabilities) are described in
[driver-abstraction.md](driver-abstraction.md).

## Key decisions

1. **Runtimes**: GPUI keeps its own executor. Database IO runs on a dedicated tokio runtime owned by
   `fg-service`; results return to the UI over channels. The UI thread only renders and applies
   small state updates.
2. **Drivers**: SQLite first (`rusqlite`, bundled), then native `tokio-postgres` and `mysql_async` rather than sqlx. We need streaming rows
   and exact control over dynamic type decoding, not compile-time checked queries.
3. **Result sets**: typed `Value` cells, streamed in batches so the first rows render quickly.
   Large blobs / long text are loaded lazily.
4. **Large tables**: paged (default 1000 rows) with prefetch plus a virtualized grid. Never
   `SELECT *` the whole table.
5. **Cancellation**: every query is a task. PostgreSQL: cancel token / `pg_cancel_backend`.
   MySQL: `KILL QUERY`.
6. **Metadata**: lazy schema tree with TTL cache and manual refresh; feeds completion and MCP resources.
7. **Encryption**: every record in the local store is encrypted with a per-device key held in the OS keychain (`keyring`); passwords are encrypted records too, never stored in the clear, never logged. Sync (M9) and backups use separate password-derived keys. See [encryption-and-sync.md](encryption-and-sync.md) and [data-model.md](data-model.md).
8. **TLS / SSH**: rustls everywhere (no OpenSSL), `russh` for tunnels.
9. **Crash safety**: unsaved editor content journaled to SQLite (WAL) and restored on restart.

## Error handling

All fallible paths return `fg_core::Error`. `unwrap` / `expect` are linted (warn) and must not be
used on production paths.

## Testing

- Integration tests against real servers with testcontainers: MySQL 5.7 / 8.x, MariaDB, PostgreSQL 12-17.
- Property tests for value decoding (numeric, timestamptz, json, arrays, enums, bit, geometry).
- Benchmarks (criterion) for decoding; fixed-dataset scroll and memory checks run in CI.
- UI tests via `gpui_kit::test` (headless).

## Service layer (`fg-service`)

`fg-service` is the only crate that owns the tokio runtime; the GUI and MCP are thin clients.

- **Foreign executors**: every public method is `async` but spawns its real work onto the service
  runtime and awaits the join handle, so GPUI's executor (or any other) can await it. Driver code
  needs a tokio context (`spawn_blocking`); this is how it gets one.
- **Sessions are leased, not shared.** A connection has a small pool (default 4). A task holds its
  session exclusively until it finishes, so a grid page, a row count and a schema listing can run
  together. In-memory SQLite is limited to 1 session (each session would be a separate database).
- **Cancel is per task.** The driver's cancel is session-wide; `TaskCanceller` forwards it only while
  the task is `Running`, under the same lock the task takes before releasing its lease, and the
  session is not reused before that. Dropping an unfinished `TaskStream` cancels it. Regression
  tests: `crates/fg-service/tests/e2e.rs` (`a_late_cancel_cannot_hit_the_next_task`).
- **Metadata** (namespaces, objects, table details, relations) is cached with a TTL and refreshed
  explicitly (`Refresh`), which also broadcasts `ServiceEvent::MetadataChanged`.
- **Browsing** compiles a declarative `TableQuery` with `fg-sql` using the cached primary key as the
  `ORDER BY` tie-breaker, so offset paging over duplicate sort values neither repeats nor skips rows.
  Raw `WHERE` text is accepted from the GUI only (`BrowseOptions::allow_raw_where`).
- **MSRV**: `rust-version = 1.92`, set by the dependency tree (`oo7` via GPUI), not by our code.
