# Roadmap

Estimates assume one full-time developer.

| Milestone | Scope | Time |
|---|---|---|
| **M0** Skeleton | Workspace, 3-OS CI, grid performance spike (1M rows x 200 cols) | 1w |
| **M1a** Data layer + SQLite | Driver abstraction (see driver-abstraction.md), SQLite driver, connection manager, schema tree, data grid (sort/filter/page), multi-tab | 2w |
| **M1b** PostgreSQL & MySQL | Both drivers against the same contracts, SSL/SSH tunnel, shared behavioural tests via testcontainers | 3w |
| **M2** SQL editor | Highlighting, multi-statement execution, completion, multiple result sets, EXPLAIN, history/favorites | 3w |
| **M2.5** MCP read-only MVP | `fastgrid-cli mcp --stdio` with list/describe/read-only query | 1w |
| **M3** Edit & structure | Inline edit with pending-changes area and SQL preview, structure editing with DDL preview, import/export (streamed) | 3w |
| **M4** MCP complete | Local HTTP endpoint with token, write tools with GUI confirmation, audit log, resources | 3w |
| **M5** Polish & release | Shortcuts, command palette, crash recovery, signed packages for 3 OSes, auto-update, docs site | 3w |

## M0 acceptance

- `cargo run -p fg-app` opens a window on macOS, Windows, Linux.
- Mock-data grid: 1M rows x 200 cols scrolls at 60fps with bounded memory.
- CI green on all three OSes (fmt, clippy, test, cargo-deny).

## Out of scope for phase 1

Other databases, cloud sync, team features, plugin system, in-app AI chat (NL2SQL; planned phase 2).

## Later

ER diagram, data diff, users/privileges, process list, more drivers (SQLite, ClickHouse, ...).
