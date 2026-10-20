//! SQL utilities that need no database: splitting scripts into statements, classifying them
//! (read / write / DDL, "dangerous"), and compiling declarative table queries to SQL.
//!
//! Shared by the editor, the grid and the MCP safety layer. Pure functions only - no IO.
//! Design notes: `docs/driver-abstraction.md`, `docs/mcp-security.md`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod browse;
mod classify;
mod lex;
mod split;

pub use browse::{BuildOptions, BuiltQuery, build_count, build_select, validate_raw_where};
pub use classify::{Analysis, StatementInfo, analyze, classify};
pub use split::{StatementSpan, split_statements, statement_at};
