# MCP security model

External agents can run SQL, so safety is designed in from day one, not bolted on.

## Transports

- `fastgrid-cli mcp --stdio` (primary): headless, configurable in Claude Desktop, Cursor, Claude Code.
- Local streamable HTTP while the GUI runs: binds to 127.0.0.1 only, random bearer token.

## Policy

Target model. The transport does not exist yet. The agent half of items 1, 2, 4, 5 and 6 is **not**
enforced by the in-process API. What stage 1 does enforce is the next section. The M2.5 gap is a
gap, not protection.

1. **Read-only by default (target).** Still two layers by design: an engine read-only session
   (PostgreSQL `SET TRANSACTION READ ONLY`, MySQL `START TRANSACTION READ ONLY`) and `fg-sql`
   classification. Stage 1 applies both only when the connection's effective safe mode is already
   `SafeMode::ReadOnly` (including `Unknown`). `RunOptions::agent` does **not** turn a normal
   connection into a read-only session.
2. **Writes are opt-in per connection (target).** GUI confirmation is a UI capability. A headless
   or agent caller cannot supply it. "Headless mode denies writes" is a job for the transport,
   which does not exist yet. It is not what `run()` does today.
3. **No raw SQL fragments from agents in browse tools.** Filters are structured and bound as
   parameters; `BrowseOptions::allow_raw_where` must be false for agents. There is no agent browse
   entry point yet, so this flag is a transport obligation, not a live check.
4. **Limits**: row cap and statement timeout are enforced in `fg-service::run`. Truncation is
   flagged on the result event (`truncated`). `McpAccess` caps are not merged in automatically;
   the transport must pass its own.
5. **Production-like connections** (`Environment::Production`, and any environment this build does
   not recognise) default to `mcp_exposed() == false` unless `McpAccess::exposed` is set. The
   field exists. Hiding the connection is the future transport's job, not `run()`'s.
6. **Audit log**: each non-editor `run` writes one row (actor, connection, statement, outcome,
   duration), encrypted like every other record, append-only, and kept after the connection is
   deleted. Editor calls write history only. A tool-level refusal that never enters `run` is not
   audited yet.
7. **Secrets** never appear in tool results, resources, or logs.

## What stage 1 enforces

The only checked entry point for arbitrary SQL is `OpenConnection::run`
(`crates/fg-service/src/run.rs`). `plan` is pure: per statement it returns class, danger,
whether confirmation is required, and the byte offset into the original script (`Debug` redacts
the SQL). A refusal returns `Error::PolicyDenied` before a session is leased.

`OpenConnection::execute` uses the same classification and has **no confirmation path**. An
alert/strict write, or a write on a read-only connection, is `PolicyDenied`. Only then does it
delegate to crate-private `execute_unchecked` (browse and `COUNT` use that; they build the SQL).
Layering: `fg-sql` classifies, `fg-service` decides policy and records, drivers stay policy-blind.

Read-only, and only when the effective safe mode is already read-only, is enforced in both places:

- Classification runs before the lease. Confirmation cannot override a read-only refusal. A
  pure-read pragma query (for example `PRAGMA table_info`) is not a write. Assignment pragmas
  and `SET` are still refused.
- The engine session calls `set_read_only(true)` **before** startup SQL, again after startup SQL,
  and on every `acquire`. What stops a header or durability write is not that re-assert. Startup
  SQL is an allow-list of session settings. Anything else (SQLite `synchronous`, `journal_mode`,
  `user_version`, and any `query_only`) is `PolicyDenied` before it is sent.

Caps are the strictest `Some`: the per-call `RunOptions`, `ExecSettings` (settings key `exec`:
`max_rows`, `statement_timeout_secs`, `history_keep`), and the connection's
`timeouts.statement_secs` (timeout only). A larger per-call number cannot loosen a stored cap.
`exec` is read with `setting_strict`. A missing row is still the default. A row that decrypts but
does not deserialize, or a read that fails, makes `run` and `Service::exec_settings` return
`Error::Storage`. They do not fall open to `max_rows = Some(100_000)`. Appearance and grid
settings stay on the lenient `setting()`. When the engine has no `statement_timeout` (SQLite),
the service times each statement, resets at every `ResultStart`, and does not count time spent
waiting on the consumer.

Each `run` attempt writes exactly one history row: `Ok` / `Error` / `Cancelled` / `NotRun`. Each
non-editor call also writes exactly one audit row. `Actor::Mcp { client }` carries the client id
passed to `RunOptions::agent`. An empty id is rejected before the run starts; it cannot hide
behind a generic `"mcp"` label. A refusal on `execute` writes history only, not audit (that path
has no agent identity). Audit insert rejects a duplicate id, including a tombstone, so a replay
cannot overwrite the row.

`SafeMode::Strict` means confirm every statement that may write. Password re-authentication is
the intended policy and is **not implemented**. Until it is, Strict is a stronger confirmation
gate than Alert, not a re-auth. Read-only is still stricter, because confirmation cannot
authorize a write.

## M2.5 gap (not protection)

`RunOptions::agent` only removes the confirmation capability and carries identity. On a
Development or Normal connection an in-process caller can still write through `run()`. There is
no in-process API that issues an engine-level read-only lease without changing the saved config.
The trust boundary is the transport, and the transport does not exist yet.

The MCP transport must enforce all of the following. Missing any one of them means stage 1 is
not agent read-only:

- Authenticated client identity. An agent-supplied string is not, by itself, that identity.
- Per-access checks of `mcp_exposed`, `environment` (production and unknown stay hidden unless
  explicitly exposed), and `McpAccess::allow_write`.
- Its own row cap and timeout. `McpAccess::max_rows` and `statement_timeout_secs` are not merged
  into `run` automatically.
- No agent-supplied confirmation. Do not call `RunOptions::gui_confirmed`, and do not fill a
  confirmation digest from the agent request.
- An audit row for tool-level refusals that never reach `run`.
- An engine-level read-only session in addition to classification. Classification alone is not
  enough.

## Planned surface

- Tools: `list_connections`, `list_schemas`, `describe_table`, `run_query`, `explain`, `sample_rows`,
  `execute_write` (gated).
- Resources: schema snapshots per connection.
