use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DataType;

/// Address of a database object.
///
/// `namespace` is whatever the engine uses as the first level under a connection:
/// a MySQL database, a PostgreSQL schema, a SQLite attached database (`main`).
/// `None` means "the session's default namespace".
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
    Sequence,
    /// User-defined type (enum, composite, domain).
    Type,
    #[serde(other)]
    Other,
}

impl ObjectKind {
    /// Has rows that can be browsed.
    pub fn is_relation(self) -> bool {
        matches!(
            self,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    #[serde(default)]
    pub comment: Option<String>,
    /// Cheap planner estimate, never an exact count. `None` when the engine has no cheap source.
    #[serde(default)]
    pub row_estimate: Option<u64>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

impl ObjectInfo {
    pub fn new(path: ObjectPath, kind: ObjectKind) -> Self {
        Self {
            path,
            kind,
            comment: None,
            row_estimate: None,
            size_bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedColumn {
    pub expression: String,
    /// `true` = STORED, `false` = VIRTUAL.
    pub stored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    /// 1-based position in the table.
    pub ordinal: u32,
    pub data_type: DataType,
    pub nullable: bool,
    /// Default expression as the engine reports it.
    #[serde(default)]
    pub default: Option<String>,
    /// 1-based position within the primary key, `None` if not part of it.
    #[serde(default)]
    pub primary_key_position: Option<u32>,
    /// AUTO_INCREMENT / SERIAL / IDENTITY.
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default)]
    pub generated: Option<GeneratedColumn>,
    #[serde(default)]
    pub collation: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    /// Engine-specific attributes that have no first-class field yet
    /// (e.g. MySQL `on_update`, PostgreSQL `identity`). Prefer promoting a key to a real field
    /// once two engines need it.
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl ColumnInfo {
    pub fn new(name: impl Into<String>, ordinal: u32, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            ordinal,
            data_type,
            nullable: true,
            default: None,
            primary_key_position: None,
            auto_increment: false,
            generated: None,
            collation: None,
            comment: None,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexColumn {
    /// Column name, or the expression text for expression indexes.
    pub name: String,
    #[serde(default)]
    pub expression: bool,
    #[serde(default)]
    pub descending: bool,
}

impl IndexColumn {
    pub fn column(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            expression: false,
            descending: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<IndexColumn>,
    pub unique: bool,
    pub primary: bool,
    /// Access method (`btree`, `hash`, `gin`, `fulltext`...).
    #[serde(default)]
    pub method: Option<String>,
    /// Partial-index predicate.
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl IndexInfo {
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ReferentialAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
    /// Anything the engine reports that we do not model, verbatim.
    Other(String),
}

impl ReferentialAction {
    /// Parse the engine's text (`"CASCADE"`, `"SET NULL"`, case-insensitive).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "NO ACTION" => Self::NoAction,
            "RESTRICT" => Self::Restrict,
            "CASCADE" => Self::Cascade,
            "SET NULL" => Self::SetNull,
            "SET DEFAULT" => Self::SetDefault,
            _ => Self::Other(s.to_owned()),
        }
    }

    pub fn sql(&self) -> &str {
        match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
            Self::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub referenced: ObjectPath,
    /// Empty when the engine references the target's primary key implicitly.
    #[serde(default)]
    pub referenced_columns: Vec<String>,
    #[serde(default)]
    pub on_update: Option<ReferentialAction>,
    #[serde(default)]
    pub on_delete: Option<ReferentialAction>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// A foreign key together with the table that owns it (schema-wide relation listing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub from: ObjectPath,
    pub key: ForeignKeyInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: Option<String>,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub name: String,
    /// `BEFORE` / `AFTER` / `INSTEAD OF`, as reported.
    pub timing: String,
    /// `INSERT` / `UPDATE` / `DELETE` (possibly several, as reported).
    pub event: String,
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDetail {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    #[serde(default)]
    pub comment: Option<String>,
    pub columns: Vec<ColumnInfo>,
    #[serde(default)]
    pub indexes: Vec<IndexInfo>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyInfo>,
    #[serde(default)]
    pub checks: Vec<CheckConstraint>,
    #[serde(default)]
    pub triggers: Vec<TriggerInfo>,
    #[serde(default)]
    pub row_estimate: Option<u64>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Table-level engine options (`engine`, `charset`, `collation`, `row_format`,
    /// `without_rowid`, `partitioned`...).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
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

    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TypeCategory;

    #[test]
    fn referential_action_roundtrip() {
        for s in [
            "NO ACTION",
            "RESTRICT",
            "CASCADE",
            "SET NULL",
            "SET DEFAULT",
        ] {
            assert_eq!(ReferentialAction::parse(s).sql(), s);
        }
        assert_eq!(
            ReferentialAction::parse("cascade"),
            ReferentialAction::Cascade
        );
        assert_eq!(ReferentialAction::parse("weird").sql(), "weird");
    }

    #[test]
    fn primary_key_is_ordered_by_position() {
        let mut a = ColumnInfo::new("a", 1, DataType::new("int", TypeCategory::Integer));
        let mut b = ColumnInfo::new("b", 2, DataType::new("int", TypeCategory::Integer));
        a.primary_key_position = Some(2);
        b.primary_key_position = Some(1);
        let d = TableDetail {
            path: ObjectPath::new(None, "t"),
            kind: ObjectKind::Table,
            comment: None,
            columns: vec![a, b],
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            row_estimate: None,
            size_bytes: None,
            options: BTreeMap::new(),
        };
        assert_eq!(d.primary_key(), vec!["b", "a"]);
    }
}
