# Data-layer abstraction (multi-database)

Goal: adding an engine means implementing two traits and one dialect, with **no changes** to
`fg-service`, `fg-app`, or `fg-mcp`. Upper layers never match on `DbKind`; they ask the driver
for its `Capabilities` and `Dialect`.

## Layers

```
fg-core    vocabulary: Value, DataType, ObjectPath, TableDetail, Capabilities, Dialect, Error
fg-driver  contracts + implementations: Driver, Session, ExecutionStream, Batcher, sqlite/ (pg/, mysql/ later)
fg-sql     dialect-aware SQL building (browse queries, DML from pending edits) — pure, no IO
fg-service pooling, SSH tunnel / TLS, policy (read-only, limits), metadata cache, task lifecycle
```

## Contracts

| Piece | Role |
|---|---|
| `Driver` | Stateless per-engine factory: `kind`, `capabilities`, `dialect`, `connect(config, password)` |
| `Session` | One physical connection: `ping`, `server_version`, metadata, `execute`, `set_read_only`, `cancel_handle` |
| `Dialect` | Pure SQL syntax differences: identifier quoting, placeholders (`?` vs `$1`), `LIMIT/OFFSET` |
| `Capabilities` | Feature flags the UI/MCP consult (transactions, cancel, read-only session, DDL, ...) |

### Design decisions

1. **Session = one connection; pooling is not the driver's job.** Drivers stay small; `fg-service`
   pools any `Driver` generically, and a tab or MCP request can pin a session (needed for
   transactions and `SET` state).
2. **One `execute` for everything.** Rows stream as `Event::{ResultStart, Rows, ResultEnd}`
   per statement, so multi-statement batches, DML/DDL (empty columns + `affected_rows`) and
   SELECT share one code path. No separate `query`/`execute` split to keep in sync.
3. **Streaming with backpressure.** The stream is a bounded channel; a slow UI blocks the driver.
   Dropping the `ExecutionStream` stops the query. `Batcher` implements batch size and
   `max_rows` truncation once for all drivers (an extra row must exist to flag `truncated`).
4. **Cancellation is a handle, not a future.** `CancelHandle` is cheap to clone and callable from
   any task. SQLite: `sqlite3_interrupt` plus a flag so cancels arriving before the statement
   starts are not lost. PostgreSQL: cancel token. MySQL: `KILL QUERY` on a side connection.
5. **Namespace instead of catalog/schema.** `ObjectPath { namespace, name }`, where namespace is
   the first tree level under a connection: MySQL database, PostgreSQL schema, SQLite `main`/
   attached DB. PostgreSQL's *databases* are separate connections (`list_databases`,
   `Capabilities::multiple_databases`). Engines needing a third level can add it to `ObjectPath`
   without breaking callers.
6. **Normalized types.** `DataType { native, category }`: the raw type string for display and
   DDL, a `TypeCategory` for behaviour (alignment, editors, sorting, MCP descriptions). `Value`
   has an `Other { type_name, text }` escape hatch so exotic types are never dropped or mis-decoded.
7. **Structured errors.** `QueryError { message, code, position }` so the editor can underline
   the failing token and MCP can return SQLSTATE.
8. **Transport is above the driver.** SSH tunnels and TLS termination are `fg-service` concerns;
   drivers see a plain host/port (or file path). `ConnectionConfig.options` carries engine-specific
   knobs (`readonly`, `create`, ...); unknown keys are ignored.
9. **Secrets are arguments, not config.** The password is passed to `connect` (read by `fg-service` from the
   encrypted store) and is never part of `ConnectionConfig`.
10. **Read-only is layered.** `Session::set_read_only` is the engine-level guard; engines that
    can't offer it return `Unsupported` and `fg-service` relies on `fg-sql` statement
    classification as well (see `mcp-security.md`).

## Adding a driver (checklist)

1. `DbKind` variant (+ dialect in `fg-core::dialect`, with unit tests).
2. `fg-driver/src/<engine>/` behind a cargo feature; implement `Driver` and `Session`;
   register in `driver_for`.
3. Map native types to `TypeCategory` and values to `Value` (unknown → `Value::Other`).
4. Use `Batcher` for batching/limits; map native errors to `QueryError` (with code/position).
5. Declare honest `Capabilities`.
6. Run the shared behavioural tests (same scenarios as `tests/sqlite.rs`: multi-statement,
   params, streaming batches, `max_rows`, structured errors, metadata, read-only, cancel).
   Network engines run them against testcontainers.

## Known gaps (decide when the second driver lands)

- Async engines (PG/MySQL) need an async variant of the send helpers; the SQLite driver uses
  `blocking_send` from the blocking pool.
- Large cells: `ExecOptions::max_cell_bytes` turns oversized text/bytes/JSON into `Value::Truncated`
  (preview + full size) in the shared `Batcher`. Loading the full value on demand is `fg-service`'s job (M3 inspector).
- Not yet modelled: routines, triggers, sequences, users/privileges, EXPLAIN plans, notices/warnings.
- SSL/SSH settings will become typed fields on `ConnectionConfig` when `fg-service` implements them.
