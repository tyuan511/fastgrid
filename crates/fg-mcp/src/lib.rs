//! MCP server exposing fg-service to external agents (stdio today, local HTTP later).
//!
//! Read-only by default; see `docs/mcp-security.md`. The protocol is a small hand-written
//! JSON-RPC 2.0 subset (`initialize`, `ping`, `tools/list`, `tools/call`), the same approach as
//! `fg-lsp`: the surface is four requests, and owning it keeps the policy checks in one place
//! instead of behind a macro-generated router.
//!
//! [`McpServer::handle`] is a pure "message in, message out" function over an [`fg_service::Service`];
//! the byte transport (newline-delimited JSON on stdin/stdout) lives in `fg-cli`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod server;
mod tools;

pub use server::McpServer;
