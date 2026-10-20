use serde::{Deserialize, Serialize};

/// A typed cell value. Large payloads are expected to be loaded lazily by the driver.
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
}
