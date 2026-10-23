//! The filter drawer's rows as the user edits them (text boxes), and their conversion into the
//! typed [`Filter`]s the service understands.
//!
//! A row with a missing value is simply not applied yet (the user is still typing); a value that
//! cannot be read as the column's type (letters in a number column) is an error shown beside the
//! row, so it is never silently turned into a different query.

use fg_core::{ColumnMeta, Filter, FilterOp, TypeCategory, Value};

/// Operators offered in the drawer, in menu order.
pub const OPERATORS: &[(FilterOp, &str)] = &[
    (FilterOp::Eq, "="),
    (FilterOp::Ne, "≠"),
    (FilterOp::Lt, "<"),
    (FilterOp::Le, "≤"),
    (FilterOp::Gt, ">"),
    (FilterOp::Ge, "≥"),
    (FilterOp::Contains, "contains"),
    (FilterOp::NotContains, "does not contain"),
    (FilterOp::StartsWith, "starts with"),
    (FilterOp::EndsWith, "ends with"),
    (FilterOp::Like, "like"),
    (FilterOp::NotLike, "not like"),
    (FilterOp::In, "in"),
    (FilterOp::NotIn, "not in"),
    (FilterOp::Between, "between"),
    (FilterOp::IsNull, "is null"),
    (FilterOp::IsNotNull, "is not null"),
];

pub fn op_label(op: FilterOp) -> &'static str {
    OPERATORS
        .iter()
        .find(|(o, _)| *o == op)
        .map_or("?", |(_, l)| l)
}

pub fn op_from_label(label: &str) -> Option<FilterOp> {
    OPERATORS.iter().find(|(_, l)| *l == label).map(|(o, _)| *o)
}

/// How many value boxes an operator needs.
pub fn value_slots(op: FilterOp) -> usize {
    match op {
        FilterOp::IsNull | FilterOp::IsNotNull => 0,
        FilterOp::Between => 2,
        _ => 1,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    pub enabled: bool,
    pub column: String,
    pub op: FilterOp,
    pub value: String,
    /// Second value, for `between`.
    pub value2: String,
}

impl DraftRow {
    pub fn new(column: impl Into<String>) -> Self {
        Self {
            enabled: true,
            column: column.into(),
            op: FilterOp::Eq,
            value: String::new(),
            value2: String::new(),
        }
    }
}

/// What applying the drawer produces.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub filters: Vec<Filter>,
    pub raw_where: Option<String>,
}

/// A problem with one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowError {
    pub row: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterDraft {
    pub rows: Vec<DraftRow>,
    /// "Raw SQL" mode: one free-form `WHERE` expression instead of rows.
    pub raw_mode: bool,
    pub raw_where: String,
}

impl FilterDraft {
    /// Rebuild the drawer from the filters currently applied (text values from typed ones).
    pub fn from_applied(filters: &[Filter], raw_where: Option<&str>) -> Self {
        let rows = filters
            .iter()
            .map(|f| DraftRow {
                enabled: f.enabled,
                column: f.column.clone(),
                op: f.op,
                value: f.values.first().map(value_text).unwrap_or_default(),
                value2: f.values.get(1).map(value_text).unwrap_or_default(),
            })
            .collect();
        Self {
            rows,
            raw_mode: raw_where.is_some_and(|r| !r.trim().is_empty()),
            raw_where: raw_where.unwrap_or_default().to_owned(),
        }
    }

    /// Turn the drawer into query parts. `columns` supply the types used to read the values.
    pub fn to_applied(&self, columns: &[ColumnMeta]) -> Result<Applied, Vec<RowError>> {
        if self.raw_mode {
            let raw = self.raw_where.trim();
            return Ok(Applied {
                filters: Vec::new(),
                raw_where: (!raw.is_empty()).then(|| raw.to_owned()),
            });
        }
        let mut filters = Vec::new();
        let mut errors = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            match row_to_filter(row, columns) {
                Ok(Some(f)) => filters.push(f),
                Ok(None) => {}
                Err(message) => errors.push(RowError { row: i, message }),
            }
        }
        if errors.is_empty() {
            Ok(Applied {
                filters,
                raw_where: None,
            })
        } else {
            Err(errors)
        }
    }
}

fn category_of(columns: &[ColumnMeta], name: &str) -> TypeCategory {
    columns
        .iter()
        .find(|c| c.name == name)
        .map_or(TypeCategory::Other, |c| c.data_type.category)
}

/// `Ok(None)`: not complete yet, leave it out.
fn row_to_filter(row: &DraftRow, columns: &[ColumnMeta]) -> Result<Option<Filter>, String> {
    let category = category_of(columns, &row.column);
    let values = match row.op {
        FilterOp::IsNull | FilterOp::IsNotNull => Vec::new(),
        FilterOp::Between => {
            if row.value.trim().is_empty() || row.value2.trim().is_empty() {
                return Ok(None);
            }
            vec![
                parse_value(&row.value, category)?,
                parse_value(&row.value2, category)?,
            ]
        }
        FilterOp::In | FilterOp::NotIn => {
            let items: Vec<&str> = row
                .value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                return Ok(None);
            }
            items
                .into_iter()
                .map(|s| parse_value(s, category))
                .collect::<Result<_, _>>()?
        }
        // Text operators always work on the text as typed.
        FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith => {
            if row.value.is_empty() {
                return Ok(None);
            }
            vec![Value::Text(row.value.clone())]
        }
        FilterOp::Like | FilterOp::NotLike => {
            if row.value.is_empty() {
                return Ok(None);
            }
            vec![Value::Text(row.value.clone())]
        }
        _ => {
            if row.value.trim().is_empty() {
                return Ok(None);
            }
            vec![parse_value(&row.value, category)?]
        }
    };
    Ok(Some(Filter {
        column: row.column.clone(),
        op: row.op,
        values,
        enabled: row.enabled,
    }))
}

/// Read typed text as a value for a column of this type. `NULL` (any case) means SQL NULL.
fn parse_value(text: &str, category: TypeCategory) -> Result<Value, String> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    match category {
        TypeCategory::Integer => t
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| format!("“{t}” is not a whole number")),
        TypeCategory::Float => t
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("“{t}” is not a number")),
        TypeCategory::Decimal => {
            if t.parse::<f64>().is_ok() {
                Ok(Value::Decimal(t.to_owned()))
            } else {
                Err(format!("“{t}” is not a number"))
            }
        }
        TypeCategory::Bool => match t.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" => Ok(Value::Bool(true)),
            "false" | "f" | "0" | "no" | "n" => Ok(Value::Bool(false)),
            _ => Err(format!("“{t}” is not true or false")),
        },
        _ => Ok(Value::Text(text.to_owned())),
    }
}

/// Read text typed into a grid cell as a value for a column of this type.
///
/// Unlike a filter box, a cell keeps text exactly as typed (so `null` in a name column is the
/// four letters), and only the exact word `NULL` means SQL NULL. Everything else follows the
/// column type; text and unknown types accept anything.
pub fn parse_cell_text(text: &str, category: TypeCategory) -> Result<Value, String> {
    if text == "NULL" {
        return Ok(Value::Null);
    }
    match category {
        TypeCategory::Integer
        | TypeCategory::Float
        | TypeCategory::Decimal
        | TypeCategory::Bool => {
            // An empty numeric cell is a mistake, not NULL.
            if text.trim().is_empty() {
                return Err("enter a value, or NULL".into());
            }
            parse_value(text, category)
        }
        _ => Ok(Value::Text(text.to_owned())),
    }
}

/// How a value is shown back in a text box.
fn value_text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        other => super::format::copy_text(other),
    }
}

#[cfg(test)]
mod tests {
    use fg_core::DataType;

    use super::*;

    #[test]
    fn cell_text_keeps_text_verbatim_and_types_numbers() {
        assert_eq!(
            parse_cell_text("null", TypeCategory::Text),
            Ok(Value::Text("null".into()))
        );
        assert_eq!(parse_cell_text("NULL", TypeCategory::Text), Ok(Value::Null));
        assert_eq!(
            parse_cell_text("  padded ", TypeCategory::Text),
            Ok(Value::Text("  padded ".into()))
        );
        assert_eq!(
            parse_cell_text("42", TypeCategory::Integer),
            Ok(Value::Int(42))
        );
        assert!(parse_cell_text("4x", TypeCategory::Integer).is_err());
        assert!(parse_cell_text("", TypeCategory::Integer).is_err());
        assert_eq!(
            parse_cell_text("", TypeCategory::Text),
            Ok(Value::Text(String::new()))
        );
    }

    fn cols() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
            ColumnMeta::new("name", DataType::new("TEXT", TypeCategory::Text)),
            ColumnMeta::new("price", DataType::new("REAL", TypeCategory::Float)),
            ColumnMeta::new("active", DataType::new("BOOLEAN", TypeCategory::Bool)),
        ]
    }

    fn row(column: &str, op: FilterOp, value: &str) -> DraftRow {
        DraftRow {
            enabled: true,
            column: column.into(),
            op,
            value: value.into(),
            value2: String::new(),
        }
    }

    fn apply(rows: Vec<DraftRow>) -> Result<Applied, Vec<RowError>> {
        FilterDraft {
            rows,
            ..Default::default()
        }
        .to_applied(&cols())
    }

    #[test]
    fn values_are_read_with_the_columns_type() {
        let a = apply(vec![
            row("id", FilterOp::Ge, " 42 "),
            row("name", FilterOp::Eq, "42"),
            row("price", FilterOp::Lt, "9.5"),
            row("active", FilterOp::Eq, "Yes"),
        ])
        .unwrap();
        let vals: Vec<_> = a.filters.iter().map(|f| f.values[0].clone()).collect();
        assert_eq!(
            vals,
            [
                Value::Int(42),
                Value::Text("42".into()),
                Value::Float(9.5),
                Value::Bool(true)
            ]
        );
    }

    #[test]
    fn unreadable_values_are_reported_per_row_not_guessed() {
        let errors = apply(vec![
            row("id", FilterOp::Eq, "1"),
            row("id", FilterOp::Eq, "abc"),
            row("active", FilterOp::Eq, "maybe"),
        ])
        .unwrap_err();
        assert_eq!(errors.iter().map(|e| e.row).collect::<Vec<_>>(), [1, 2]);
        assert!(errors[0].message.contains("abc"));
    }

    #[test]
    fn rows_still_being_typed_are_left_out() {
        let a = apply(vec![
            row("id", FilterOp::Eq, ""),
            row("name", FilterOp::Contains, ""),
            row("id", FilterOp::In, " , "),
            row("id", FilterOp::Between, "1"),
            row("id", FilterOp::Gt, "5"),
        ])
        .unwrap();
        assert_eq!(a.filters.len(), 1);
        assert_eq!(a.filters[0].op, FilterOp::Gt);
    }

    #[test]
    fn list_between_and_null_operators() {
        let mut between = row("id", FilterOp::Between, "1");
        between.value2 = "9".into();
        let a = apply(vec![
            row("id", FilterOp::In, "1, 2,3"),
            between,
            row("name", FilterOp::IsNull, ""),
            row("name", FilterOp::Eq, "null"),
        ])
        .unwrap();
        assert_eq!(
            a.filters[0].values,
            vec![Value::Int(1), Value::Int(2), Value::Int(3)]
        );
        assert_eq!(a.filters[1].values, vec![Value::Int(1), Value::Int(9)]);
        assert!(a.filters[2].values.is_empty(), "is null takes no value");
        assert_eq!(
            a.filters[3].values,
            vec![Value::Null],
            "typing NULL compares with null"
        );
    }

    #[test]
    fn text_operators_use_the_text_as_typed() {
        let a = apply(vec![row("id", FilterOp::Contains, "4x")]).unwrap();
        assert_eq!(
            a.filters[0].values,
            vec![Value::Text("4x".into())],
            "even on a number column"
        );
    }

    #[test]
    fn disabled_rows_are_kept_but_flagged() {
        let mut r = row("id", FilterOp::Eq, "1");
        r.enabled = false;
        let a = apply(vec![r]).unwrap();
        assert!(!a.filters[0].enabled);
    }

    #[test]
    fn raw_mode_replaces_the_rows() {
        let d = FilterDraft {
            rows: vec![row("id", FilterOp::Eq, "abc")],
            raw_mode: true,
            raw_where: "  status = 'x'  ".into(),
        };
        // The bad row is irrelevant in raw mode.
        let a = d.to_applied(&cols()).unwrap();
        assert_eq!(
            a,
            Applied {
                filters: vec![],
                raw_where: Some("status = 'x'".into())
            }
        );
        let empty = FilterDraft {
            raw_mode: true,
            ..Default::default()
        };
        assert_eq!(empty.to_applied(&cols()).unwrap().raw_where, None);
    }

    #[test]
    fn the_drawer_is_rebuilt_from_applied_filters() {
        let applied = apply(vec![
            row("id", FilterOp::Ge, "42"),
            row("name", FilterOp::IsNull, ""),
        ])
        .unwrap();
        let d = FilterDraft::from_applied(&applied.filters, None);
        assert_eq!(d.rows[0].value, "42");
        assert_eq!(d.rows[0].column, "id");
        assert!(!d.raw_mode);
        let raw = FilterDraft::from_applied(&[], Some("a = 1"));
        assert!(raw.raw_mode && raw.raw_where == "a = 1");
    }

    #[test]
    fn operator_labels_roundtrip_and_slots() {
        for (op, label) in OPERATORS {
            assert_eq!(op_from_label(label), Some(*op));
            assert_eq!(op_label(*op), *label);
        }
        assert_eq!(value_slots(FilterOp::Between), 2);
        assert_eq!(value_slots(FilterOp::IsNull), 0);
        assert_eq!(value_slots(FilterOp::Eq), 1);
        assert_eq!(op_from_label("nonsense"), None);
    }
}
