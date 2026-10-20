use serde::{Deserialize, Serialize};

/// Engine-independent classification of a column type. The grid uses it for alignment,
/// editors and sorting; MCP uses it to describe columns to agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    TimestampTz,
    Interval,
    Json,
    Uuid,
    Enum,
    Set,
    Array,
    Geometry,
    Network,
    Bit,
    #[serde(other)]
    Other,
}

/// Numeric details of a type; all optional and engine-reported.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TypeParams {
    /// Character / byte length (`varchar(255)`).
    #[serde(default)]
    pub length: Option<u32>,
    #[serde(default)]
    pub precision: Option<u32>,
    #[serde(default)]
    pub scale: Option<u32>,
    #[serde(default)]
    pub unsigned: bool,
    /// Array dimensions (PostgreSQL `int[][]` = 2); 0 = not an array.
    #[serde(default)]
    pub array_dims: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataType {
    /// The type exactly as the engine reports it (`varchar(255)`, `timestamptz`, ...).
    pub native: String,
    pub category: TypeCategory,
    #[serde(default)]
    pub params: TypeParams,
    /// Allowed values for `Enum` / `Set` types (drives the editor dropdown).
    #[serde(default)]
    pub variants: Vec<String>,
}

impl DataType {
    pub fn new(native: impl Into<String>, category: TypeCategory) -> Self {
        Self {
            native: native.into(),
            category,
            params: TypeParams::default(),
            variants: Vec::new(),
        }
    }

    pub fn unknown() -> Self {
        Self::new("", TypeCategory::Other)
    }

    pub fn with_params(mut self, params: TypeParams) -> Self {
        self.params = params;
        self
    }

    pub fn with_variants(mut self, variants: Vec<String>) -> Self {
        self.variants = variants;
        self
    }
}

/// Where a result column physically comes from. Present only when the column maps directly to a
/// table column (not for expressions/aggregates); required to make query results editable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnOrigin {
    pub namespace: Option<String>,
    pub table: String,
    pub column: String,
}

/// A column of a result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: DataType,
    /// `None` when the engine does not say (expressions, most result sets).
    #[serde(default)]
    pub nullable: Option<bool>,
    #[serde(default)]
    pub origin: Option<ColumnOrigin>,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: None,
            origin: None,
        }
    }
}
