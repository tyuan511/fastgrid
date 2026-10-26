//! MySQL / MariaDB value decoding, parameter encoding, and type mapping.
//!
//! The wire values come from `mysql_common::Value`, but what a value *means*
//! depends on the result column that produced it: the same `Bytes` payload is
//! text for a `VARCHAR`, binary for a `BLOB`, a decimal string for
//! `DECIMAL`, and JSON text for `JSON`. Decoding therefore always takes the
//! `Column` alongside the value, mirroring `postgres/types.rs`.
//!
//! Two conventions worth knowing:
//!
//! * `TINYINT(1)` decodes to `Value::Bool`. MySQL has no boolean type;
//!   `TINYINT(1)` is the universal boolean convention (used by ORMs and by
//!   TablePlus-style clients), so a one-digit-wide tiny integer reads as a
//!   boolean. Any other tiny integer reads as an integer.
//! * Zero dates (`0000-00-00`, `0000-00-00 00:00:00`) are not errors: they
//!   decode to the same all-zero text, so the roundtrip is lossless. The
//!   server still rejects them on strict-mode writes; that is the server's
//!   decision, not the decoder's.

use fg_core::{DataType, TypeCategory, Value};
use mysql_common::Value as MysqlValue;
use mysql_common::constants::{ColumnFlags, ColumnType};
use mysql_common::packets::Column;

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decode one wire value with its result column into an `fg-core` value.
pub fn decode(col: &Column, value: MysqlValue) -> Value {
    use ColumnType::*;
    if let MysqlValue::NULL = value {
        return Value::Null;
    }
    let flags = col.flags();
    let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = flags.contains(ColumnFlags::BINARY_FLAG);
    match col.column_type() {
        MYSQL_TYPE_TINY if col.column_length() == 1 => match value {
            MysqlValue::Int(v) => Value::Bool(v != 0),
            MysqlValue::UInt(v) => Value::Bool(v != 0),
            // The text protocol sends integers as text.
            MysqlValue::Bytes(b) => {
                let s = text(&b);
                s.parse::<i64>()
                    .map(|v| Value::Bool(v != 0))
                    .unwrap_or(Value::Text(s))
            }
            other => Value::Other {
                type_name: "tinyint".into(),
                text: format!("{other:?}"),
            },
        },
        // YEAR is inherently unsigned on the wire (the server sets the
        // UNSIGNED flag), but a year is an integer, not an unsigned
        // quantity: always decode to Int.
        MYSQL_TYPE_YEAR => match value {
            MysqlValue::Int(v) => Value::Int(v),
            MysqlValue::UInt(v) => Value::Int(v as i64),
            MysqlValue::Bytes(b) => {
                let s = text(&b);
                s.parse::<i64>().map(Value::Int).unwrap_or(Value::Text(s))
            }
            other => Value::Other {
                type_name: "year".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_INT24 => match value {
            MysqlValue::Int(v) => Value::Int(v),
            MysqlValue::UInt(v) => Value::UInt(v),
            // The server may send a signed value for an unsigned column when
            // the text protocol is in play; keep it exact either way.
            MysqlValue::Bytes(b) => {
                let s = text(&b);
                if unsigned {
                    s.parse::<u64>().map(Value::UInt).unwrap_or(Value::Text(s))
                } else {
                    s.parse::<i64>().map(Value::Int).unwrap_or(Value::Text(s))
                }
            }
            other => Value::Other {
                type_name: type_name(col).into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => match value {
            MysqlValue::Float(v) => Value::Float(v as f64),
            MysqlValue::Double(v) => Value::Float(v),
            MysqlValue::Bytes(b) => {
                let s = text(&b);
                s.parse::<f64>().map(Value::Float).unwrap_or(Value::Text(s))
            }
            other => Value::Other {
                type_name: type_name(col).into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => match value {
            MysqlValue::Bytes(b) => Value::Decimal(text(&b)),
            other => Value::Other {
                type_name: type_name(col).into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_JSON => match value {
            MysqlValue::Bytes(b) => Value::Json(text(&b)),
            other => Value::Other {
                type_name: "json".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET => match value {
            MysqlValue::Bytes(b) => Value::Text(text(&b)),
            other => Value::Other {
                type_name: type_name(col).into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => match value {
            MysqlValue::Date(y, mo, d, _, _, _, _) => Value::Date(format!("{y:04}-{mo:02}-{d:02}")),
            MysqlValue::Bytes(b) => Value::Date(text(&b)),
            other => Value::Other {
                type_name: "date".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => match value {
            MysqlValue::Date(y, mo, d, h, mi, s, us) => {
                Value::Timestamp(format_datetime(y, mo, d, h, mi, s, us))
            }
            MysqlValue::Bytes(b) => Value::Timestamp(normalize_datetime(text(&b))),
            other => Value::Other {
                type_name: "datetime".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => match value {
            MysqlValue::Date(y, mo, d, h, mi, s, us) => {
                // The server already converted the stored UTC instant to the
                // session time zone, so this string is session-local time.
                // Like the PostgreSQL driver's UTC rendering, the instant is
                // exact; only the zone label is missing.
                Value::TimestampTz(format_datetime(y, mo, d, h, mi, s, us))
            }
            MysqlValue::Bytes(b) => Value::TimestampTz(normalize_datetime(text(&b))),
            other => Value::Other {
                type_name: "timestamp".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => match value {
            MysqlValue::Time(neg, days, h, mi, s, us) => {
                Value::Time(format_time(neg, days, h, mi, s, us))
            }
            MysqlValue::Bytes(b) => Value::Time(text(&b)),
            other => Value::Other {
                type_name: "time".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_BIT => match value {
            MysqlValue::Bytes(b) => Value::Bytes(b),
            other => Value::Other {
                type_name: "bit".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_GEOMETRY => match value {
            MysqlValue::Bytes(b) => Value::Bytes(b),
            other => Value::Other {
                type_name: "geometry".into(),
                text: format!("{other:?}"),
            },
        },
        MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB => match value {
            MysqlValue::Bytes(b) => {
                if binary {
                    Value::Bytes(b)
                } else {
                    Value::Text(text(&b))
                }
            }
            other => Value::Other {
                type_name: type_name(col).into(),
                text: format!("{other:?}"),
            },
        },
        _ => match value {
            MysqlValue::Bytes(b) => Value::Other {
                type_name: type_name(col).into(),
                text: text(&b),
            },
            other => Value::Other {
                type_name: type_name(col).into(),
                text: format!("{other:?}"),
            },
        },
    }
}

/// The text protocol renders datetimes with a space
/// (`2026-10-26 13:14:15`); fg-core's convention is the T form, which the
/// typed path already produces. Normalize the text form to match.
fn normalize_datetime(s: String) -> String {
    let mut bytes = s.into_bytes();
    if bytes.len() > 10 && bytes[10] == b' ' {
        bytes[10] = b'T';
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn format_datetime(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> String {
    if us == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{us:06}")
    }
}

/// MySQL `TIME` values can exceed 24 hours (up to 838:59:59); the wire form
/// carries whole days separately, so fold them back into total hours, which
/// is the server's own text rendering.
fn format_time(neg: bool, days: u32, h: u8, mi: u8, s: u8, us: u32) -> String {
    let total_hours = days * 24 + u32::from(h);
    let sign = if neg { "-" } else { "" };
    if us == 0 {
        format!("{sign}{total_hours:02}:{mi:02}:{s:02}")
    } else {
        format!("{sign}{total_hours:02}:{mi:02}:{s:02}.{us:06}")
    }
}

/// Base type name for a result column, for display and for `Value::Other`.
fn type_name(col: &Column) -> &'static str {
    use ColumnType::*;
    match col.column_type() {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_VARCHAR => "varchar",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_TINY_BLOB => "tinyblob",
        MYSQL_TYPE_MEDIUM_BLOB => "mediumblob",
        MYSQL_TYPE_LONG_BLOB => "longblob",
        MYSQL_TYPE_BLOB => "blob",
        MYSQL_TYPE_VAR_STRING => "varchar",
        MYSQL_TYPE_STRING => "char",
        MYSQL_TYPE_GEOMETRY => "geometry",
        _ => "unknown",
    }
}

/// `DataType` for a result column. The native string is the base name only:
/// result metadata does not carry the full declaration (`varchar(255)`,
/// `decimal(10,3)`), so `describe` (which reads `information_schema`) is the
/// authoritative source for exact declarations.
pub fn data_type(col: &Column) -> DataType {
    use ColumnType::*;
    let flags = col.flags();
    let binary = flags.contains(ColumnFlags::BINARY_FLAG);
    // The server reports ENUM / SET result columns as VAR_STRING with a
    // flag, not as their own column type — the flag is the only signal.
    if flags.contains(ColumnFlags::ENUM_FLAG) {
        return DataType::new("enum", TypeCategory::Enum);
    }
    if flags.contains(ColumnFlags::SET_FLAG) {
        return DataType::new("set", TypeCategory::Set);
    }
    let (name, category) = match col.column_type() {
        MYSQL_TYPE_TINY if col.column_length() == 1 => ("tinyint", TypeCategory::Bool),
        MYSQL_TYPE_TINY => ("tinyint", TypeCategory::Integer),
        MYSQL_TYPE_SHORT => ("smallint", TypeCategory::Integer),
        MYSQL_TYPE_LONG => ("int", TypeCategory::Integer),
        MYSQL_TYPE_LONGLONG => ("bigint", TypeCategory::Integer),
        MYSQL_TYPE_INT24 => ("mediumint", TypeCategory::Integer),
        MYSQL_TYPE_YEAR => ("year", TypeCategory::Integer),
        MYSQL_TYPE_FLOAT => ("float", TypeCategory::Float),
        MYSQL_TYPE_DOUBLE => ("double", TypeCategory::Float),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => ("decimal", TypeCategory::Decimal),
        MYSQL_TYPE_JSON => ("json", TypeCategory::Json),
        MYSQL_TYPE_ENUM => ("enum", TypeCategory::Enum),
        MYSQL_TYPE_SET => ("set", TypeCategory::Set),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ("date", TypeCategory::Date),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => ("time", TypeCategory::Time),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => ("datetime", TypeCategory::Timestamp),
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => ("timestamp", TypeCategory::TimestampTz),
        MYSQL_TYPE_BIT => ("bit", TypeCategory::Bit),
        MYSQL_TYPE_GEOMETRY => ("geometry", TypeCategory::Geometry),
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => {
            if binary {
                ("binary", TypeCategory::Binary)
            } else {
                (type_name(col), TypeCategory::Text)
            }
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                ("blob", TypeCategory::Binary)
            } else {
                // TEXT columns arrive as a blob type with a text charset.
                ("text", TypeCategory::Text)
            }
        }
        _ => ("unknown", TypeCategory::Other),
    };
    DataType::new(name, category)
}

/// `DataType` from an `information_schema.COLUMNS.COLUMN_TYPE` string, e.g.
/// `varchar(255)`, `decimal(10,3)`, `enum('a','b')`, `bigint unsigned`.
/// Enum and set members are parsed out as variants.
pub fn data_type_from_native(native: &str) -> DataType {
    let lower = native.to_lowercase();
    let (base, rest) = match lower.split_once('(') {
        Some((b, r)) => (b.trim(), Some(r)),
        None => (lower.as_str(), None),
    };
    let (base, unsigned) = match base.strip_suffix(" unsigned") {
        Some(b) => (b, true),
        None => (base, false),
    };
    let mut data_type = match base {
        "tinyint" => {
            // `tinyint(1)` is the boolean convention; anything wider is an
            // integer. A bare `tinyint` (no width) is an integer.
            let width = rest
                .and_then(|r| r.split(')').next())
                .and_then(|w| w.parse::<u32>().ok());
            if width == Some(1) {
                DataType::new(native, TypeCategory::Bool)
            } else {
                DataType::new(native, TypeCategory::Integer)
            }
        }
        "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            DataType::new(native, TypeCategory::Integer)
        }
        "float" | "double" | "real" => DataType::new(native, TypeCategory::Float),
        "decimal" | "numeric" | "fixed" => DataType::new(native, TypeCategory::Decimal),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" => {
            DataType::new(native, TypeCategory::Text)
        }
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
            DataType::new(native, TypeCategory::Binary)
        }
        "date" => DataType::new(native, TypeCategory::Date),
        "time" => DataType::new(native, TypeCategory::Time),
        "datetime" => DataType::new(native, TypeCategory::Timestamp),
        "timestamp" => DataType::new(native, TypeCategory::TimestampTz),
        "json" => DataType::new(native, TypeCategory::Json),
        "bit" => DataType::new(native, TypeCategory::Bit),
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" => DataType::new(native, TypeCategory::Geometry),
        "enum" => {
            let variants = rest.map(parse_quoted_list).unwrap_or_default();
            DataType::new(native, TypeCategory::Enum).with_variants(variants)
        }
        "set" => {
            let variants = rest.map(parse_quoted_list).unwrap_or_default();
            DataType::new(native, TypeCategory::Set).with_variants(variants)
        }
        _ => DataType::new(native, TypeCategory::Other),
    };
    if unsigned {
        data_type = data_type.with_params(fg_core::TypeParams {
            unsigned: true,
            ..Default::default()
        });
    }
    data_type
}

/// Parse `'a','b\'c','d'` into `["a", "b'c", "d"]`. MySQL escapes a quote
/// inside a member with a backslash (`\'`); doubled quotes are accepted
/// too. Commas inside quotes are not separators.
fn parse_quoted_list(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = rest.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            // A backslash inside quotes escapes the next character.
            '\\' if in_quotes => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            '\'' if in_quotes => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    current.push('\'');
                } else {
                    in_quotes = false;
                }
            }
            '\'' => in_quotes = true,
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut current));
            }
            ')' if !in_quotes => break,
            _ => current.push(c),
        }
    }
    if !current.is_empty() || !out.is_empty() {
        out.push(current);
    }
    out
}

/// Encode an `fg-core` value as a MySQL parameter. The server parses text
/// parameters with the target column's input function, so dates, decimals,
/// and JSON travel as their text forms. Arrays have no MySQL literal form
/// and are refused rather than silently rewritten.
pub fn to_param(value: &Value) -> fg_core::Result<MysqlValue> {
    Ok(match value {
        Value::Null => MysqlValue::NULL,
        Value::Bool(v) => MysqlValue::Int(i64::from(*v)),
        Value::Int(v) => MysqlValue::Int(*v),
        Value::UInt(v) => MysqlValue::UInt(*v),
        Value::Float(v) => MysqlValue::Double(*v),
        Value::Decimal(v) | Value::Json(v) => MysqlValue::Bytes(v.clone().into_bytes()),
        Value::Text(v) | Value::Uuid(v) => MysqlValue::Bytes(v.clone().into_bytes()),
        Value::Bytes(v) => MysqlValue::Bytes(v.clone()),
        Value::Date(v)
        | Value::Time(v)
        | Value::Timestamp(v)
        | Value::TimestampTz(v)
        | Value::Interval(v) => MysqlValue::Bytes(v.clone().into_bytes()),
        Value::Other { text, .. } => MysqlValue::Bytes(text.clone().into_bytes()),
        Value::Array(_) => {
            return Err(fg_core::Error::Unsupported(
                "array parameters have no MySQL literal form".into(),
            ));
        }
        Value::Truncated { .. } => {
            return Err(fg_core::Error::Unsupported(
                "a truncated value cannot be sent as a parameter".into(),
            ));
        }
        // `Value` is non-exhaustive: future variants must fail loudly
        // rather than silently encoding as something else.
        _ => {
            return Err(fg_core::Error::Unsupported(
                "this value kind has no MySQL parameter encoding".into(),
            ));
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn col(kind: ColumnType) -> Column {
        Column::new(kind)
    }

    #[test]
    fn tinyint_width_one_is_bool() {
        let c = Column::new(ColumnType::MYSQL_TYPE_TINY).with_column_length(1);
        assert_eq!(decode(&c, MysqlValue::Int(1)), Value::Bool(true));
        assert_eq!(decode(&c, MysqlValue::Int(0)), Value::Bool(false));
        assert_eq!(data_type(&c).category, TypeCategory::Bool);
    }

    #[test]
    fn tinyint_wider_is_int() {
        let c = Column::new(ColumnType::MYSQL_TYPE_TINY).with_column_length(4);
        assert_eq!(decode(&c, MysqlValue::Int(7)), Value::Int(7));
    }

    #[test]
    fn zero_date_survives() {
        let c = col(ColumnType::MYSQL_TYPE_DATE);
        assert_eq!(
            decode(&c, MysqlValue::Date(0, 0, 0, 0, 0, 0, 0)),
            Value::Date("0000-00-00".into())
        );
        let c = col(ColumnType::MYSQL_TYPE_DATETIME);
        assert_eq!(
            decode(&c, MysqlValue::Date(0, 0, 0, 0, 0, 0, 0)),
            Value::Timestamp("0000-00-00T00:00:00".into())
        );
    }

    #[test]
    fn time_folds_days_into_hours() {
        let c = col(ColumnType::MYSQL_TYPE_TIME);
        assert_eq!(
            decode(&c, MysqlValue::Time(false, 1, 2, 3, 4, 0)),
            Value::Time("26:03:04".into())
        );
        assert_eq!(
            decode(&c, MysqlValue::Time(true, 0, 1, 2, 3, 500000)),
            Value::Time("-01:02:03.500000".into())
        );
    }

    #[test]
    fn native_types_map() {
        assert_eq!(
            data_type_from_native("bigint unsigned").category,
            TypeCategory::Integer
        );
        assert_eq!(
            data_type_from_native("decimal(10,3)").category,
            TypeCategory::Decimal
        );
        let e = data_type_from_native("enum('a','b\\'c')");
        assert_eq!(e.category, TypeCategory::Enum);
        assert_eq!(e.variants, vec!["a".to_string(), "b'c".to_string()]);
        assert_eq!(
            data_type_from_native("tinyint(1)").category,
            TypeCategory::Bool
        );
        assert_eq!(
            data_type_from_native("tinyint(4)").category,
            TypeCategory::Integer
        );
        assert_eq!(
            data_type_from_native("mediumtext").category,
            TypeCategory::Text
        );
    }

    #[test]
    fn params_encode() {
        assert_eq!(to_param(&Value::Bool(true)).unwrap(), MysqlValue::Int(1));
        assert_eq!(
            to_param(&Value::Decimal("1.50".into())).unwrap(),
            MysqlValue::Bytes(b"1.50".to_vec())
        );
        assert!(to_param(&Value::Array(vec![])).is_err());
    }
}
