use fg_core::{DataType, Error, QueryError, TypeCategory, TypeParams, Value};
use rusqlite::types::{Value as SqlValue, ValueRef};

/// Classify a declared column type using SQLite's own affinity rules, plus the common
/// conventions (BOOLEAN, DATE, JSON...) that SQLite stores with NUMERIC affinity.
pub fn data_type(decl: Option<&str>) -> DataType {
    let Some(decl) = decl.map(str::trim).filter(|d| !d.is_empty()) else {
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
    DataType::new(decl, category).with_params(params(&up, category))
}

/// `VARCHAR(100)` -> length 100, `DECIMAL(8,2)` -> precision 8 / scale 2.
fn params(upper_decl: &str, category: TypeCategory) -> TypeParams {
    let mut p = TypeParams {
        unsigned: upper_decl.contains("UNSIGNED"),
        ..TypeParams::default()
    };
    let args = upper_decl
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(inside, _)| {
            inside
                .split(',')
                .map(|n| n.trim().parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match (category, args.as_slice()) {
        (TypeCategory::Decimal, [precision, scale]) => {
            p.precision = *precision;
            p.scale = *scale;
        }
        (TypeCategory::Decimal, [precision]) => p.precision = *precision,
        (_, [length]) => p.length = *length,
        _ => {}
    }
    p
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

pub fn to_sql(v: &Value) -> Result<SqlValue, Error> {
    Ok(match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        // SQLite integers are signed 64-bit; larger values fall back to text.
        Value::UInt(u) => {
            i64::try_from(*u).map_or_else(|_| SqlValue::Text(u.to_string()), SqlValue::Integer)
        }
        Value::Float(f) => SqlValue::Real(*f),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => SqlValue::Text(s.clone()),
        Value::Bytes(b) => SqlValue::Blob(b.clone()),
        Value::Other { text, .. } => SqlValue::Text(text.clone()),
        Value::Array(_) => {
            return Err(Error::Query(QueryError::new(
                "SQLite has no array type; pass JSON text instead",
            )));
        }
        Value::Truncated(_) => {
            return Err(Error::Query(QueryError::new(
                "refusing to write a truncated value; load the full value first",
            )));
        }
        _ => {
            return Err(Error::Unsupported("value type".into()));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_type_parameters() {
        let t = data_type(Some("VARCHAR(100)"));
        assert_eq!(t.category, TypeCategory::Text);
        assert_eq!(t.params.length, Some(100));

        let d = data_type(Some("DECIMAL(8, 2)"));
        assert_eq!((d.params.precision, d.params.scale), (Some(8), Some(2)));

        assert!(data_type(Some("INT UNSIGNED")).params.unsigned);
        assert_eq!(data_type(None), DataType::unknown());
    }

    #[test]
    fn unsafe_values_are_not_written() {
        assert!(to_sql(&Value::Array(vec![])).is_err());
        assert!(to_sql(&Value::Int(1)).is_ok());
    }
}
