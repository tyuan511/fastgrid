use serde::{Deserialize, Serialize};

/// A typed cell value, the common currency between drivers, the grid and MCP.
///
/// Exotic engine types that have no dedicated variant travel as [`Value::Other`] so that
/// nothing is silently lost or mis-decoded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Arbitrary-precision numbers are kept as text to avoid precision loss.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Json(String),
    /// Temporal types as ISO-8601 text until a dedicated representation is chosen.
    Temporal(String),
    /// Anything else, as the engine's textual form plus its native type name.
    Other {
        type_name: String,
        text: String,
    },
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}
