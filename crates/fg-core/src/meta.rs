use serde::{Deserialize, Serialize};

use crate::DataType;

/// Address of a table-like object.
///
/// `namespace` is whatever the engine uses as the first level under a connection:
/// a MySQL database, a PostgreSQL schema, a SQLite attached database (`main`).
/// `None` means "the session's default namespace".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectPath {
    pub namespace: Option<String>,
    pub name: String,
}

impl ObjectPath {
    pub fn new(namespace: Option<&str>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.map(str::to_owned),
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    /// Default expression as the engine reports it.
    pub default: Option<String>,
    /// 1-based position within the primary key, `None` if not part of it.
    pub primary_key_position: Option<u32>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub referenced: ObjectPath,
    /// Empty when the engine references the target's primary key implicitly.
    pub referenced_columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDetail {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

impl TableDetail {
    /// Primary-key column names in key order.
    pub fn primary_key(&self) -> Vec<&str> {
        let mut cols: Vec<_> = self
            .columns
            .iter()
            .filter_map(|c| c.primary_key_position.map(|p| (p, c.name.as_str())))
            .collect();
        cols.sort_by_key(|(p, _)| *p);
        cols.into_iter().map(|(_, n)| n).collect()
    }
}
