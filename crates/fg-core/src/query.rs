//! Declarative description of "show me this table": what the grid, saved filters and MCP all
//! produce, and what `fg-sql` compiles to dialect-specific SQL.

use serde::{Deserialize, Serialize};

use crate::{ObjectPath, Value, compat::lenient_vec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Raw `LIKE` pattern supplied by the user.
    Like,
    NotLike,
    /// Substring match; wildcards in the value are escaped.
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    In,
    NotIn,
    /// `values` = [low, high].
    Between,
    IsNull,
    IsNotNull,
    #[serde(other)]
    Unknown,
}

impl FilterOp {
    /// How many entries `Filter::values` needs: `Some(n)` exactly, `None` = one or more.
    pub fn arity(self) -> Option<usize> {
        match self {
            FilterOp::IsNull | FilterOp::IsNotNull => Some(0),
            FilterOp::Between => Some(2),
            FilterOp::In | FilterOp::NotIn => None,
            _ => Some(1),
        }
    }
}

fn yes() -> bool {
    true
}

/// One row of the filter drawer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    #[serde(default)]
    pub values: Vec<Value>,
    /// Disabled rules are kept but not applied.
    #[serde(default = "yes")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NullsOrder {
    First,
    Last,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
    #[serde(default)]
    pub nulls: Option<NullsOrder>,
}

/// Browse a table or view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableQuery {
    pub path: ObjectPath,
    /// `None` = all columns.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub filters: Vec<Filter>,
    /// Free-form `WHERE` body (the drawer's Raw SQL mode); combined with `filters` using AND.
    #[serde(default)]
    pub raw_where: Option<String>,
    #[serde(default)]
    pub sort: Vec<SortKey>,
    pub limit: u64,
    #[serde(default)]
    pub offset: u64,
}

impl TableQuery {
    pub fn new(path: ObjectPath, limit: u64) -> Self {
        Self {
            path,
            columns: None,
            filters: Vec::new(),
            raw_where: None,
            sort: Vec::new(),
            limit,
            offset: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arity() {
        assert_eq!(FilterOp::IsNull.arity(), Some(0));
        assert_eq!(FilterOp::Between.arity(), Some(2));
        assert_eq!(FilterOp::In.arity(), None);
        assert_eq!(FilterOp::Eq.arity(), Some(1));
    }

    #[test]
    fn saved_filters_survive_unknown_operators() {
        let json = r#"{"path":{"namespace":null,"name":"t"},"limit":10,"filters":[
            {"column":"a","op":"eq","values":[{"Int":1}]},
            {"column":"b","op":"fuzzy_match_2030","values":[]}]}"#;
        let q: TableQuery = serde_json::from_str(json).unwrap();
        // The unknown operator parses as `Unknown` (kept, but must not be executed).
        assert_eq!(q.filters.len(), 2);
        assert_eq!(q.filters[1].op, FilterOp::Unknown);
    }
}
