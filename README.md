# fast-table

A fast, stable, cross-platform database client written in Rust on [GPUI Kit](https://gpui-kit.com),
with a built-in MCP server so external AI agents can query your databases safely.

> Status: pre-alpha, project skeleton only. See [docs/roadmap.md](docs/roadmap.md).

## Goals

- **Interaction** modeled on TablePlus: tabs, schema tree, inline-editable grid, command palette.
- **Features** aiming at dbx-level coverage: SQL editor, structure editing, import/export.
- **Performance and stability** first: virtualized grid, streamed results, cancellable queries.
- **MCP for external agents**: `fast-table-cli mcp --stdio`, read-only by default, audited.
- macOS, Windows, Linux. Apache-2.0, free.

Phase 1 databases: MySQL (and MariaDB), PostgreSQL.

## Layout

| Crate | Role |
|---|---|
| `ft-core` | Domain types (connection, value, error). No IO. |
| `ft-driver` | `Driver` trait + MySQL / PostgreSQL backends |
| `ft-store` | Local SQLite store, keychain access |
| `ft-sql` | Statement splitting and classification, completion |
| `ft-service` | Pools, query tasks, metadata cache, policy; shared by GUI and MCP |
| `ft-mcp` | MCP server (stdio + local HTTP) |
| `ft-cli` | Headless binary `fast-table-cli` |
| `ft-app` | GPUI desktop app `fast-table` |

More: [architecture](docs/architecture.md) · [roadmap](docs/roadmap.md) · [MCP security](docs/mcp-security.md)

## Develop

```bash
cargo run -p ft-app          # GUI
cargo run -p ft-cli          # headless
cargo test --workspace
```
