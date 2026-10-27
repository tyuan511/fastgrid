# fastgrid

A fast, stable, cross-platform database client written in Rust on [GPUI Kit](https://gpui-kit.com),
with a built-in MCP server so external AI agents can query your databases safely.

> Status: pre-alpha. SQLite, PostgreSQL, MySQL/MariaDB and SQL Server work end
> to end (browse, edit, structure, import/export, MCP), and M8 (polish and
> release) is in progress. See [docs/roadmap.md](docs/roadmap.md) and the
> [docs index](docs/index.md).

## Goals

- **Interaction** modeled on TablePlus: tabs, schema tree, inline-editable grid, command palette.
- **Features** aiming at dbx-level coverage: SQL editor, structure editing, import/export.
- **Performance and stability** first: virtualized grid, streamed results, cancellable queries.
- **MCP for external agents**: `fastgrid-cli mcp --stdio` (and loopback HTTP), read-only by default, audited.
- macOS, Windows, Linux. Apache-2.0, free.

Phase 1 databases: SQLite, PostgreSQL, MySQL/MariaDB and SQL Server. All engines plug in through one driver abstraction ([docs/driver-abstraction.md](docs/driver-abstraction.md)).

## Layout

| Crate | Role |
|---|---|
| `fg-core` | Domain types (connection, value, error). No IO. |
| `fg-driver` | `Driver` / `Session` traits + SQLite, PostgreSQL, MySQL/MariaDB, SQL Server |
| `fg-crypto` | Envelope encryption, password-protected files (no IO) |
| `fg-store` | Encrypted local store (connections, history, ...), migrations, backup/sync primitives |
| `fg-sql` | Statement splitting and classification, completion |
| `fg-service` | Pools, query tasks, metadata cache, policy; shared by GUI and MCP |
| `fg-mcp` | MCP server (stdio + local HTTP) |
| `fg-cli` | Headless binary `fastgrid-cli` |
| `fg-app` | GPUI desktop app `fastgrid` |

More: [architecture](docs/architecture.md) · [driver abstraction](docs/driver-abstraction.md) · [roadmap](docs/roadmap.md) · [MCP security](docs/mcp-security.md)

## Develop

```bash
cargo run -p fg-app          # GUI
cargo run -p fg-cli          # headless
cargo test --workspace
```
