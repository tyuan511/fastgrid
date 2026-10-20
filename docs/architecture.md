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

## Key decisions

1. **Runtimes**: GPUI keeps its own executor. Database IO runs on a dedicated tokio runtime owned by
   `fg-service`; results return to the UI over channels. The UI thread only renders and applies
   small state updates.
2. **Drivers**: native `tokio-postgres` and `mysql_async` rather than sqlx. We need streaming rows
   and exact control over dynamic type decoding, not compile-time checked queries.
3. **Result sets**: typed `Value` cells, streamed in batches so the first rows render quickly.
   Large blobs / long text are loaded lazily.
4. **Large tables**: paged (default 1000 rows) with prefetch plus a virtualized grid. Never
   `SELECT *` the whole table.
5. **Cancellation**: every query is a task. PostgreSQL: cancel token / `pg_cancel_backend`.
   MySQL: `KILL QUERY`.
6. **Metadata**: lazy schema tree with TTL cache and manual refresh; feeds completion and MCP resources.
7. **Secrets**: OS keychain via `keyring`; never in the SQLite store or logs.
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
