//! SQL utilities: statement splitting, read-only / dangerous statement classification,
//! completion support. Shared by the editor and the MCP safety layer.

/// How risky a statement is, used by the MCP policy layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementClass {
    ReadOnly,
    Write,
    Ddl,
    Unknown,
}
