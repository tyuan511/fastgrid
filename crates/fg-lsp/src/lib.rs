//! A SQL language server's analysis core: what is in scope where, and what the editor should offer
//! at a byte offset.
//!
//! This is the crate the M2 roadmap called for. Completion used to live in the editor as a prefix
//! matcher over a schema snapshot, which cannot resolve `t.col` to the right table, cannot see a
//! CTE's columns and cannot tell a subquery's alias from the outer table's. That analysis lives
//! here instead, so the editor stays a text widget, and the same answers can serve `fastgrid-cli`
//! and the MCP tools later.
//!
//! Layering: `fg-lsp` depends on `fg-core` (vocabulary) and `fg-sql` (the token stream). It has no
//! IO and no database handle - the caller supplies the schema it knows through [`scope::Catalog`],
//! which keeps this testable without a connection and keeps policy out of it.
//!
//! Design notes: `docs/roadmap.md` (M2, the language-server item).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod complete;
pub mod diagnostics;
pub mod protocol;
pub mod scope;
pub mod wire;

pub use complete::{Completion, CompletionKind, complete_at};
pub use diagnostics::{Diagnostic, diagnostic_for, diagnostic_for_script, failing_statement};
pub use protocol::{Server, ServerConfig, offset_of, position_of};
pub use scope::{Catalog, MapCatalog, Relation, Scope, scope_at};
pub use wire::{Message, read_message, write_message};
