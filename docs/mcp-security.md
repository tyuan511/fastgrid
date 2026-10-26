# MCP security model

External agents can run SQL, so safety is designed in from day one, not bolted on.

## Transports

- `fastgrid-cli mcp --stdio` (primary): headless, configurable in Claude Desktop, Cursor, Claude Code.
- Local Streamable HTTP, from the GUI's launcher panel or `fastgrid-cli mcp --http`: binds to
  127.0.0.1 only, bearer token from settings (rotatable from the GUI, no restart), plain JSON
  responses (no SSE: fastgrid sends no server-initiated streams). See "The HTTP transport (M7)".

## Policy

Enforced in `fg-service::run`, not left to the transport. The agent half of items 1, 2, 4 and 6
landed in M7; what stage 1 enforces is the next section.

1. **Read-only by default.** Still two layers by design: an engine read-only session
   (PostgreSQL `SET TRANSACTION READ ONLY`, MySQL `START TRANSACTION READ ONLY`) and `fg-sql`
   classification. Stage 1 applies both only when the connection's effective safe mode is already
   `SafeMode::ReadOnly` (including `Unknown`). `RunOptions::agent` does **not** turn a normal
   connection into a read-only session — but since M7 a write on a normal connection still cannot
   run without passing the gate in item 2, so "normal" no longer means "writable by agents".
2. **Writes are opt-in per connection, and per statement.** A write on an agent run needs both:
   the connection's `mcp.allow_write`, and a person approving that exact statement in the GUI.
   `run` files the request in the service's pending-write registry, broadcasts
   `ServiceEvent::McpWriteRequested`, and waits up to 5 minutes; a denial, a timeout, or no
   listener at all (headless: no GUI attached) refuses the write. Approval never overrides a
   read-only refusal, and resolving an unknown or already-resolved id changes nothing. The
   transport cannot approve anything: it never sees the registry, only the denial.
3. **No raw SQL fragments from agents in browse tools.** Filters are structured and bound as
   parameters; `sample_rows` browses with `BrowseOptions::allow_raw_where: false`.
4. **Limits**: row cap and statement timeout are enforced in `fg-service::run`, and since M7 the
   agent path also merges the connection's `McpAccess` caps there: a per-call number cannot
   loosen `mcp.max_rows` or `mcp.statement_timeout_secs`. Truncation is flagged on the result
   event (`truncated`). Transports still pass their own defaults (200 rows / 30 s) on top.
5. **Production-like connections** (`Environment::Production`, and any environment this build does
   not recognise) default to `mcp_exposed() == false` unless `McpAccess::exposed` is set. Hiding
   the connection is the transport's job, and both transports do it: a hidden connection and a
   missing one return the same error text from tools and resources alike.
6. **Audit log**: each non-editor `run` writes one row (actor, connection, statement, outcome,
   duration), encrypted like every other record, append-only, and kept after the connection is
   deleted. Editor calls write history only. A tool-level refusal that never enters `run` gets its
   row from the tool layer instead, and resource reads are audited the same way, so no call leaves
   the log without a row. The GUI's audit panel views, filters, and exports those rows.
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

## The stdio transport (M2.5)

`fastgrid-cli mcp --stdio` closes the gap above for the stdio case:

- **Read-only, both layers.** The process starts its `Service` with `ServiceOptions::force_read_only`.
  Every connection it opens has its effective safe mode overridden to `ReadOnly` in memory, before
  the pool is built: the engine session is read-only and `run` classifies against read-only, and
  confirmation cannot override either. The saved config is not rewritten. The GUI process is
  unaffected, so the same store can be open in both.
- **Identity.** `initialize`'s `clientInfo.name`, prefixed `stdio:`, is the audit actor. stdio has
  no authentication: whoever launched the process already holds the user's OS session and can read
  the store directly, so the name labels the log; it is not a credential. A call before
  `initialize` is refused. Authenticated identities arrive with the HTTP transport (M7).
- **Visibility.** Only readable connections with `mcp_exposed()` are listed or usable. A hidden
  connection and a missing one return the same error text.
- **Limits.** Default 200 rows, never above the connection's `mcp.max_rows` or 5000; default 30 s
  timeout, or the connection's `mcp.statement_timeout_secs`. An agent can lower the row cap, not
  raise it. Replies are cut at 256 KiB and say so. Truncation is flagged per result set.
- **No confirmation path.** The transport only builds `RunOptions::agent`, and the process has
  no GUI attached, so an `execute_write` meets the read-only refusal before the allow_write
  question even arises. The tool exists; on this transport it always refuses.
- **Audit.** `run_query` is recorded by `run` (including its refusals). Every other call, and a
  `run_query` stopped before `run` (hidden connection, bad arguments), gets one row from the tool
  layer, with the SQL when there is one. If that row cannot be written the call fails.
- **Prompt injection.** `run_query` replies carry a note that the content is data, and the server
  `instructions` say the same. That is a mitigation, not a guarantee.

Still open: PostgreSQL / MySQL engine-level read-only (M4 / M5).

## The HTTP transport (M7)

Hand-rolled over a tokio `TcpListener` in `fg-mcp/src/http.rs` — no HTTP framework, same house
style as the JSON-RPC layer. One request per connection, then close.

- **Bind.** 127.0.0.1 only, port 0 by default; the bound port is written back to
  `McpHttpSettings` so the GUI shows the real URL. Nothing here ever listens on a LAN address.
- **Auth.** Every request needs `Authorization: Bearer <token>`, compared without early exit.
  The token is generated on first start (32 random bytes, hex) and stored in settings; the GUI's
  launcher panel rotates it, and the server re-reads it per request, so rotation needs no
  restart and an old token stops working at once. A missing or wrong token is HTTP 401.
- **Sessions.** `initialize` without a `Mcp-Session-Id` is issued one; later calls must send it
  back. The audit actor is `http:<clientInfo.name>` (stdio keeps `stdio:`), and each session has
  its own identity: two HTTP clients never share one, unlike the single stdio client.
- **Methods.** Only `POST /mcp`; anything else is 404, any other method 405 (GET would be an SSE
  stream, and fastgrid sends none). Notifications get 202 with an empty body, never a fabricated
  error object.
- **Writes.** The HTTP server is not force-read-only, so the M7 gate is what a write meets: a
  connection without `allow_write` refuses at once; one with it waits for a person in the GUI —
  and a headless `fastgrid-cli mcp --http` has no listener, so it denies at once instead of
  stalling the agent for the 5-minute timeout.

## Client configuration

The server reads the same store as the GUI, so connections created in the GUI
appear here with no extra setup; `FASTGRID_DATA_DIR` overrides the store
location (same variable the GUI honours). The binary is the `fg-cli` package
(`cargo run -p fg-cli -- mcp --stdio` from a checkout, or the installed
`fastgrid-cli` next to the GUI build).

Claude Code (from a terminal):

```
claude mcp add fastgrid -- /path/to/fastgrid-cli mcp --stdio
```

Claude Desktop and Cursor (their MCP settings file, same shape):

```json
{
  "mcpServers": {
    "fastgrid": {
      "command": "/path/to/fastgrid-cli",
      "args": ["mcp", "--stdio"]
    }
  }
}
```

HTTP clients (the GUI's launcher panel or `fastgrid-cli mcp --http`
prints the URL and token; the token below comes from that panel):

```json
{
  "mcpServers": {
    "fastgrid": {
      "url": "http://127.0.0.1:PORT/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Interop check (2026-10-25): the official MCP TypeScript SDK client
(`@modelcontextprotocol/sdk` 1.32.2, the same protocol client Claude
Desktop and Claude Code speak) drove a debug `fastgrid-cli mcp --stdio`
over a real encrypted store end to end — `initialize`, `tools/list`
(five tools), `list_connections`, `list_tables`, `describe_table`, a
`SELECT` through `run_query` (rows returned, with the data-not-instructions
note), a `DROP TABLE` refused by policy, and an unknown connection refused
with the same text as a hidden one. The table still held all 100 rows
afterwards. The Claude apps themselves have not run against it: no Claude
Desktop or Claude Code client exists on the Linux VM where this was
checked, so that last step stays open for a machine that has one.

Interop check (2026-10-26, M7): the same SDK client drove both transports
over a fresh encrypted store. Stdio: 10 tools listed, `run_query`
returned the 100-row count, `execute_write` refused read-only,
`explain` and `sample_rows` ran, `resources/list` + `resources/read`
returned the schema snapshot. HTTP: the server printed its URL and
bearer token, the client authenticated with it, and the same calls ran —
with `execute_write` refused for a different, correct reason (no
`allow_write` on that connection, and the headless server has no GUI to
approve with). The table still held all 100 rows afterwards.

## Shipped surface (M7)

- Tools: `list_connections`, `list_namespaces`, `list_tables`, `describe_table`, `run_query`,
  `explain` (exactly one read statement; `EXPLAIN ANALYZE` of a write is refused, not planned),
  `sample_rows` (a peek, no raw `WHERE`), `list_relations`, `find_references` (the reverse:
  which tables point at this one), `execute_write` (gated per item 2 above).
- Resources: `fastgrid://<connection-id>/schema` per visible connection — namespaces, objects,
  and per-table detail, added whole until the 256 KiB budget runs out so the snapshot stays
  valid JSON, with a `truncated` flag when it is cut. Hidden connections are indistinguishable
  from missing ones here too.

Still not shipped: `SafeMode::Strict` password re-authentication (Strict is a stronger
confirmation gate, not a re-auth), and end-to-end demos against Claude Desktop / Cursor /
Claude Code themselves (no such client exists on the Linux VM where this was checked; the
official TypeScript SDK speaks the same protocol — see below).
