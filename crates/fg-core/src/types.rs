use serde::{Deserialize, Serialize};

/// Engine-independent classification of a column type. The grid uses it for alignment,
/// editors and sorting; MCP uses it to describe columns to agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TypeCategory {
    Bool,
    Integer,
    Float,
    Decimal,
    Text,
    Binary,
    Date,
    Time,
    Timestamp,
    Json,
    Uuid,
    Enum,
    Array,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataType {
    /// The type exactly as the engine reports it (`varchar(255)`, `timestamptz`, ...).
    pub native: String,
    pub category: TypeCategory,
}

impl DataType {
    pub fn new(native: impl Into<String>, category: TypeCategory) -> Self {
        Self {
            native: native.into(),
            category,
        }
    }

    pub fn unknown() -> Self {
        Self::new("", TypeCategory::Other)
    }
}

/// A column of a result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: DataType,
}
