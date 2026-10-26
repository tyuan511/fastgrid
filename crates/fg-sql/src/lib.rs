//! SQL utilities that need no database: splitting scripts into statements, classifying them
//! (read / write / DDL, "dangerous"), and compiling declarative table queries to SQL.
//!
//! Shared by the editor, the grid and the MCP safety layer. Pure functions only - no IO.
//! Design notes: `docs/driver-abstraction.md`, `docs/mcp-security.md`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod browse;
mod classify;
mod comments;
mod ddl;
mod dml;
mod format;
pub mod lex;
mod split;

pub use browse::{BuildOptions, BuiltQuery, build_count, build_select, validate_raw_where};
pub use classify::{Analysis, StatementInfo, analyze, classify};
pub use comments::{CommentEdit, toggle_line_comments};
pub use ddl::{
    SchemaChange, SchemaStatement, coerce_text, compile_schema_changes, render_create_table,
    render_create_table_as,
};
pub use dml::{CellChange, DmlStatement, RowChange, compile_changes};
pub use format::{format_sql, is_formatted};
pub use split::{StatementSpan, split_statements, statement_at};
// The token stream, for `fg-lsp`: scope analysis needs to know where strings, comments and
// identifiers begin and end, and duplicating a tolerant lexer would be a second source of truth.
// `Flavor`/`Kind`/`Token` stay `pub(crate)`-ish in spirit: the module is public, the internals are
// only as public as they have to be.
