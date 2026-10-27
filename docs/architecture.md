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
           ^
         fg-lsp   (pure analysis: scope, completion, diagnostics)
```

Rule: dependencies point downward only. `fg-core` has no IO. `fg-app` is the only crate that
depends on GPUI. `fg-mcp` and `fg-cli` must work without a display.

`fg-lsp` is the SQL language server: analysis over `fg-core` and `fg-sql`'s token stream (what
relations are in scope at a byte offset, what to complete there, which statement a failure belongs
to), plus the LSP wire format and protocol state machine. Analysis has no IO and no database handle -
the caller passes the schema it knows - so the GUI editor and `fastgrid-cli lsp` share one answer.

Two layers inside it, kept apart on purpose: `protocol` is a pure request/response function (no IO,
testable without a process) and `wire` is the `Content-Length` framing. `fg-cli` is then a
`read -> handle -> write` loop and nothing else. An external editor (VS Code, Neovim) talks to
`fastgrid-cli lsp` over stdio; the GUI talks to the same `Server` in process.

The multi-database contracts (Driver / Session / Dialect / Capabilities) are described in
[driver-abstraction.md](driver-abstraction.md).

## Key decisions

1. **Runtimes**: GPUI keeps its own executor. Database IO runs on a dedicated tokio runtime owned by
   `fg-service`; results return to the UI over channels. The UI thread only renders and applies
   small state updates.
2. **Drivers**: SQLite first (`rusqlite`, bundled), then native `tokio-postgres`, `mysql_async` and
   `tiberius` (TDS, rustls backend) rather than sqlx. We need streaming rows
   and exact control over dynamic type decoding, not compile-time checked queries.
3. **Result sets**: typed `Value` cells, streamed in batches so the first rows render quickly.
   Large blobs / long text are loaded lazily.
4. **Large tables**: paged (default 1000 rows) with prefetch plus a virtualized grid. Never
   `SELECT *` the whole table.
5. **Cancellation**: every query is a task. `TaskCanceller` forwards the driver's cancel only while that
   task is running, so a late cancel cannot hit the next lease of the session. Dropping an unfinished
   `TaskStream` or `RunStream` cancels it. PostgreSQL: cancel token / `pg_cancel_backend`. MySQL:
   `KILL QUERY`. SQL Server: `KILL <spid>` on a side connection (TDS Attention desynchronises the
   connection and then silently returns zero rows, so it is not used). SQLite: `sqlite3_interrupt`,
   plus a generation flag so a cancel issued before the statement starts is not cleared by `reset`. A statement timeout is not the same mechanism: when
   `Capabilities::statement_timeout` is false, `fg-service::run` arms its own per-statement timer
   (reset at each `ResultStart`; time waiting on the consumer is not counted) and cancels that
   statement. Engines that enforce a timeout themselves still receive one value for the script.
6. **Metadata**: lazy schema tree with TTL cache and manual refresh; feeds completion and MCP resources.
7. **Encryption**: every record in the local store is encrypted with a per-device key held in the OS keychain (`keyring`); passwords are encrypted records too, never stored in the clear, never logged. Sync (M9) and backups use separate password-derived keys. See [encryption-and-sync.md](encryption-and-sync.md) and [data-model.md](data-model.md).
8. **TLS / SSH**: rustls everywhere (no OpenSSL), `russh` for tunnels.
9. **Crash safety**: unsaved editor content journaled to SQLite (WAL) and restored on restart.
10. **Arbitrary SQL**: `fg-sql` classifies; `fg-service` decides policy and records; drivers stay
    policy-blind. The checked entry point is `OpenConnection::run` (`plan` before a session is
    leased). Row caps and statement timeouts are enforced there, not in the UI: the effective
    value is the strictest `Some` among the per-call `RunOptions`, `ExecSettings` (settings key
    `exec`), and, for the timeout only, the connection's `timeouts.statement_secs`. A caller
    cannot raise a stored cap. If those settings cannot be read or decrypted into `ExecSettings`,
    the call fails closed instead of falling open to the default cap. `SafeMode::Strict` confirms
    every statement that may write; password re-authentication applies to editor runs confirmed
    through the GUI when a connection password is stored (grid batch commits and agent approvals
    still confirm without a password).
11. **Recording survives a dropped stream.** History and audit go through one process-lifetime
    writer thread (`fastgrid-record`). Dropping a stream only notifies that thread; it does not
    spawn, and it does not need the tokio runtime to still be alive, so a shutting-down runtime
    cannot panic the drop or skip the notify. A writer that has already exited is a failed record
    (warned on the drop path), not a silent success. `RunStream` also holds an `Arc` of the service
    runtime until the stream is dropped, so dropping `Service` first does not abandon a record the
    execution task has not finished writing.

## Error handling

All fallible paths return `fg_core::Error`. `unwrap` / `expect` are linted (warn) and must not be
used on production paths.

## Testing

- Integration tests against real servers with testcontainers: MySQL 5.7 / 8.x, MariaDB, PostgreSQL 12-17, SQL Server 2017 / 2019 / 2022.
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
- **Policy stays above the driver.** `fg-sql` classifies a script; `OpenConnection::run` applies
  safe mode, caps and recording; drivers do not see `SafeMode` or confirmation. `execute` is
  classification-guarded and has no confirmation path. A config save bumps a per-connection epoch
  and closes the open pool in the service's own task, so a stale open cannot keep running under
  the previous policy after the save has landed.
- **Metadata** (namespaces, objects, table details, relations) is cached with a TTL and refreshed
  explicitly (`Refresh`), which also broadcasts `ServiceEvent::MetadataChanged`.
- **Browsing** compiles a declarative `TableQuery` with `fg-sql` using the cached primary key as the
  `ORDER BY` tie-breaker, so offset paging over duplicate sort values neither repeats nor skips rows.
  Raw `WHERE` text is accepted from the GUI only (`BrowseOptions::allow_raw_where`).
- **MSRV**: `rust-version = 1.92`, set by the dependency tree (`oo7` via GPUI), not by our code.
