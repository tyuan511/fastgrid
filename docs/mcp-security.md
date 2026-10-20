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
3. **Limits**: max rows and statement timeout enforced server-side; truncated results are flagged to the agent.
4. **Production connections** (`is_production`) are hidden from MCP unless explicitly exposed.
5. **Audit log**: every call (agent, connection, statement, outcome, duration) is stored locally and
   viewable in the GUI.
6. **Secrets** never appear in tool results, resources, or logs.

## Planned surface

- Tools: `list_connections`, `list_schemas`, `describe_table`, `run_query`, `explain`, `sample_rows`,
  `execute_write` (gated).
- Resources: schema snapshots per connection.
