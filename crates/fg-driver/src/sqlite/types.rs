use fg_core::{DataType, TypeCategory, Value};
use rusqlite::types::{Value as SqlValue, ValueRef};

/// Classify a declared column type using SQLite's own affinity rules, plus the common
/// conventions (BOOLEAN, DATE, JSON...) that SQLite stores with NUMERIC affinity.
pub fn data_type(decl: Option<&str>) -> DataType {
    let Some(decl) = decl.filter(|d| !d.is_empty()) else {
        return DataType::unknown();
    };
    let up = decl.to_ascii_uppercase();
    let has = |needles: &[&str]| needles.iter().any(|n| up.contains(n));
    let category = if has(&["BOOL"]) {
        TypeCategory::Bool
    } else if has(&["JSON"]) {
        TypeCategory::Json
    } else if has(&["UUID"]) {
        TypeCategory::Uuid
    } else if has(&["TIMESTAMP", "DATETIME"]) {
        TypeCategory::Timestamp
    } else if has(&["DATE"]) {
        TypeCategory::Date
    } else if has(&["TIME"]) {
        TypeCategory::Time
    } else if has(&["DECIMAL", "NUMERIC"]) {
        TypeCategory::Decimal
    } else if has(&["INT"]) {
        TypeCategory::Integer
    } else if has(&["CHAR", "CLOB", "TEXT"]) {
        TypeCategory::Text
    } else if has(&["BLOB"]) {
        TypeCategory::Binary
    } else if has(&["REAL", "FLOA", "DOUB"]) {
        TypeCategory::Float
    } else {
        TypeCategory::Other
    };
    DataType::new(decl, category)
}

pub fn to_value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

pub fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        // SQLite integers are signed 64-bit; larger values fall back to text.
        Value::UInt(u) => {
            i64::try_from(*u).map_or_else(|_| SqlValue::Text(u.to_string()), SqlValue::Integer)
        }
        Value::Float(f) => SqlValue::Real(*f),
        Value::Decimal(s) | Value::Text(s) | Value::Json(s) | Value::Temporal(s) => {
            SqlValue::Text(s.clone())
        }
        Value::Bytes(b) => SqlValue::Blob(b.clone()),
        Value::Other { text, .. } => SqlValue::Text(text.clone()),
    }
}
