# fast-table

Rust workspace: cross-platform DB client (MySQL/PostgreSQL first) on GPUI Kit, plus MCP server.
Read docs/architecture.md before changing crate boundaries.

## Rules
- Dependencies point downward; only `ft-app` may depend on gpui. `ft-mcp`/`ft-cli` must run headless.
- No `unwrap`/`expect` on production paths; return `ft_core::Error`.
- Secrets only in the OS keychain; never log credentials or query parameters containing them.
- MCP is read-only by default; any change weakening that needs an update to docs/mcp-security.md.
- Edition 2024, `unsafe_code = forbid`.

## Commands
- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo run -p ft-app`
