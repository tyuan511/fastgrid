# MCP security model

External agents can run SQL, so safety is designed in from day one, not bolted on.

## Transports

- `fastgrid-cli mcp --stdio` (primary): headless, configurable in Claude Desktop, Cursor, Claude Code.
- Local streamable HTTP while the GUI runs: binds to 127.0.0.1 only, random bearer token.

## Policy

1. **Read-only by default.** Enforced twice: session level (PostgreSQL `SET TRANSACTION READ ONLY`,
   MySQL `START TRANSACTION READ ONLY`) and statement classification in `fg-sql`.
2. **Writes are opt-in per connection** and require explicit confirmation in the GUI for each
   statement. Headless mode denies writes.
3. **No raw SQL fragments from agents in browse tools.** Filters are structured and bound as parameters;
   `BrowseOptions::allow_raw_where` is false for agents (`fg-service`).
4. **Limits**: max rows and statement timeout enforced server-side; truncated results are flagged to the agent.
5. **Production-like connections** (`Environment::Production`, and any environment this build does not
   recognise) are hidden from MCP unless explicitly exposed (`McpAccess::exposed`).
6. **Audit log**: every call (agent, connection, statement, outcome, duration) is stored locally
   (encrypted like all other data, append-only) and viewable in the GUI. Entries outlive their connection.
7. **Secrets** never appear in tool results, resources, or logs.

## Planned surface

- Tools: `list_connections`, `list_schemas`, `describe_table`, `run_query`, `explain`, `sample_rows`,
  `execute_write` (gated).
- Resources: schema snapshots per connection.
