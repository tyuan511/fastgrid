//! SQL Server value decoding, parameter encoding, and type mapping.
//!
//! `tiberius` returns a [`ColumnData`] per cell and a [`ColumnType`] per result
//! column, and its own `type_name()` is crate-private, so a result column's
//! display type has to be derived from `ColumnType` here. Catalogue metadata,
//! which does know the declared type, goes through [`data_type_from_native`]
//! instead — the same split the MySQL driver uses.
//!
//! Two conventions:
//!
//! * Temporal values render as ISO-8601 text, like every other driver. The
//!   wire forms are day counts and 10^-scale increments from engine-specific
//!   epochs (`date` counts from year 1, `datetime` from 1900, with 1/300 s
//!   fragments), which the UI cannot compare or display directly.
//! * `decimal`/`numeric` render as an exact decimal string built from the
//!   `i128` mantissa, never through `f64`.

use std::borrow::Cow;

use chrono::{Duration as ChronoDuration, NaiveDate};
use fg_core::{DataType, Error, Result, TypeCategory, TypeParams, Value};
use tiberius::numeric::Numeric;
use tiberius::{Column, ColumnData, ColumnType, ToSql, Uuid};

/// `decimal`/`numeric` scale limit on the wire. `Numeric::new_with_scale`
/// panics above it, so the encoder checks first.
const MAX_SCALE: u8 = 38;

/// Decode one wire value. The column is not needed: every `ColumnData` variant
/// carries the decoded value already, and result metadata has no extra
/// information (no length, precision or nullability) to apply.
pub fn decode(data: &ColumnData<'_>) -> Value {
    match data {
        ColumnData::U8(v) => int(v.map(i64::from)),
        ColumnData::I16(v) => int(v.map(i64::from)),
        ColumnData::I32(v) => int(v.map(i64::from)),
        ColumnData::I64(v) => int(*v),
        ColumnData::F32(v) => v.map_or(Value::Null, |v| Value::Float(f64::from(v))),
        ColumnData::F64(v) => v.map_or(Value::Null, Value::Float),
        ColumnData::Bit(v) => v.map_or(Value::Null, Value::Bool),
        ColumnData::String(v) => v
            .as_deref()
            .map_or(Value::Null, |s| Value::Text(s.to_owned())),
        ColumnData::Guid(v) => v.map_or(Value::Null, |u| Value::Uuid(u.to_string())),
        ColumnData::Binary(v) => v
            .as_deref()
            .map_or(Value::Null, |b| Value::Bytes(b.to_vec())),
        ColumnData::Numeric(v) => v.map_or(Value::Null, |n| Value::Decimal(numeric_string(n))),
        ColumnData::Xml(v) => v
            .as_deref()
            .map_or(Value::Null, |x| Value::Text(x.to_string())),
        ColumnData::DateTime(v) => v.map_or(Value::Null, |d| {
            Value::Timestamp(timestamp_from_1900(d.days(), d.seconds_fragments()))
        }),
        ColumnData::SmallDateTime(v) => v.map_or(Value::Null, |d| {
            Value::Timestamp(timestamp_from_1900(
                i32::from(d.days()),
                u32::from(d.seconds_fragments()),
            ))
        }),
        ColumnData::Date(v) => v.map_or(Value::Null, |d| Value::Date(date_from_year_one(d.days()))),
        ColumnData::Time(v) => v.map_or(Value::Null, |t| {
            Value::Time(time_of_day(t.increments(), t.scale()))
        }),
        ColumnData::DateTime2(v) => v.map_or(Value::Null, |d| {
            let date = date_from_year_one(d.date().days());
            let time = time_of_day(d.time().increments(), d.time().scale());
            Value::Timestamp(format!("{date}T{time}"))
        }),
        ColumnData::DateTimeOffset(v) => v.map_or(Value::Null, |d| {
            // `tiberius` hands back the UTC instant in the `datetime2` field plus
            // the offset, not the local wall clock: `'2024-03-02T00:00:01+08:00'`
            // arrives as 2024-03-01 16:00:01 with offset 480. Shift back to local
            // before rendering, or the text would name the wrong hour.
            let inner = d.datetime2();
            let date = date_from_year_one(inner.date().days());
            let time = time_of_day(inner.time().increments(), inner.time().scale());
            let local = local_datetime(&date, &time, d.offset());
            Value::TimestampTz(format!("{}{}", local, offset_text(d.offset())))
        }),
    }
}

fn int(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Int)
}

/// Display type for a result column. The wire type alone is all there is:
/// result metadata carries no length, precision or nullability.
pub fn data_type(col: &Column) -> DataType {
    let native = native_name(col.column_type());
    let category = match col.column_type() {
        ColumnType::Bit | ColumnType::Bitn => TypeCategory::Bool,
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => TypeCategory::Integer,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => TypeCategory::Float,
        ColumnType::Money | ColumnType::Money4 | ColumnType::Decimaln | ColumnType::Numericn => {
            TypeCategory::Decimal
        }
        ColumnType::Guid => TypeCategory::Uuid,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => TypeCategory::Binary,
        ColumnType::Daten => TypeCategory::Date,
        ColumnType::Timen => TypeCategory::Time,
        ColumnType::DatetimeOffsetn => TypeCategory::TimestampTz,
        ColumnType::Datetime4
        | ColumnType::Datetime
        | ColumnType::Datetimen
        | ColumnType::Datetime2 => TypeCategory::Timestamp,
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::Text
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::NText
        | ColumnType::Xml => TypeCategory::Text,
        _ => TypeCategory::Other,
    };
    DataType::new(native, category)
}

/// The T-SQL name of a wire type, for display next to a result column.
fn native_name(ty: ColumnType) -> String {
    let name = match ty {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln | ColumnType::Numericn => "decimal",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin | ColumnType::BigBinary => "varbinary",
        ColumnType::Image => "image",
        ColumnType::BigVarChar | ColumnType::BigChar => "varchar",
        ColumnType::Text => "text",
        ColumnType::NVarchar | ColumnType::NChar => "nvarchar",
        ColumnType::NText => "ntext",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::SSVariant => "sql_variant",
    };
    name.to_owned()
}

/// Display type for a declared type string from the catalogue, e.g.
/// `nvarchar(50)` or `decimal(18,4)`. The catalogue knows the real
/// declaration, so lengths and precision are preserved here.
pub fn data_type_from_native(native: &str) -> DataType {
    let trimmed = native.trim();
    let lower = trimmed.to_ascii_lowercase();
    let base = lower
        .split_once('(')
        .map_or(lower.as_str(), |(head, _)| head)
        .trim();
    let category = match base {
        "bit" => TypeCategory::Bool,
        "tinyint" | "smallint" | "int" | "bigint" => TypeCategory::Integer,
        "real" | "float" => TypeCategory::Float,
        "decimal" | "numeric" | "money" | "smallmoney" => TypeCategory::Decimal,
        "binary" | "varbinary" | "image" | "rowversion" | "timestamp" => TypeCategory::Binary,
        "date" => TypeCategory::Date,
        "time" => TypeCategory::Time,
        "datetime" | "datetime2" | "smalldatetime" => TypeCategory::Timestamp,
        "datetimeoffset" => TypeCategory::TimestampTz,
        "uniqueidentifier" => TypeCategory::Uuid,
        "char" | "varchar" | "text" | "nchar" | "nvarchar" | "ntext" | "xml" | "sysname" => {
            TypeCategory::Text
        }
        _ => TypeCategory::Other,
    };
    let mut data = DataType::new(trimmed, category);
    if let Some((_, rest)) = lower.split_once('(')
        && let Some((args, _)) = rest.split_once(')')
    {
        let mut parts = args.split(',').map(|p| p.trim().to_owned());
        let first = parts.next().unwrap_or_default();
        let second = parts.next();
        let mut params = TypeParams::default();
        match (second, first.parse::<u32>()) {
            (Some(scale), Ok(precision)) => {
                params.precision = Some(precision);
                params.scale = scale.parse::<u32>().ok();
            }
            (None, Ok(length)) => params.length = Some(length),
            _ => {}
        }
        data = data.with_params(params);
    }
    data
}

/// Exact decimal text for a wire `numeric`/`decimal` value.
fn numeric_string(value: Numeric) -> String {
    let scale = usize::from(value.scale());
    let mantissa = value.value();
    let sign = if mantissa < 0 { "-" } else { "" };
    let digits = mantissa.unsigned_abs().to_string();
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = if digits.len() <= scale {
        format!("{}{}", "0".repeat(scale - digits.len() + 1), digits)
    } else {
        digits
    };
    let split = padded.len() - scale;
    format!("{sign}{}.{}", &padded[..split], &padded[split..])
}

/// `date` counts days from 0001-01-01.
fn date_from_year_one(days: u32) -> String {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|epoch| epoch.checked_add_signed(ChronoDuration::days(i64::from(days))))
        .map_or_else(
            || format!("date({days})"),
            |d| d.format("%Y-%m-%d").to_string(),
        )
}

/// `datetime`/`smalldatetime` count days from 1900-01-01 and store the time of
/// day as 1/300 s fragments.
fn timestamp_from_1900(days: i32, fragments: u32) -> String {
    let Some(epoch) = NaiveDate::from_ymd_opt(1900, 1, 1).and_then(|d| d.and_hms_opt(0, 0, 0))
    else {
        return format!("datetime({days},{fragments})");
    };
    let nanos = u64::from(fragments) * 1_000_000_000 / 300;
    let seconds = i64::try_from(nanos / 1_000_000_000).unwrap_or(0);
    let sub = u32::try_from(nanos % 1_000_000_000).unwrap_or(0);
    epoch
        .checked_add_signed(ChronoDuration::days(i64::from(days)))
        .and_then(|dt| dt.checked_add_signed(ChronoDuration::seconds(seconds)))
        .and_then(|dt| dt.checked_add_signed(ChronoDuration::nanoseconds(i64::from(sub))))
        .map_or_else(|| format!("datetime({days},{fragments})"), format_timestamp)
}

/// `time`/`datetime2`/`datetimeoffset` store 10^-scale seconds since midnight.
fn time_of_day(increments: u64, scale: u8) -> String {
    let nanos = if scale <= 9 {
        increments.saturating_mul(10u64.pow(u32::from(9 - scale)))
    } else {
        increments / 10u64.pow(u32::from(scale - 9))
    };
    let seconds = nanos / 1_000_000_000;
    let sub = u32::try_from(nanos % 1_000_000_000).unwrap_or(0);
    let hh = seconds / 3600;
    let mm = (seconds % 3600) / 60;
    let ss = seconds % 60;
    let base = format!("{hh:02}:{mm:02}:{ss:02}");
    if sub == 0 {
        base
    } else {
        format!("{base}.{}", format!("{sub:09}").trim_end_matches('0'))
    }
}

fn format_timestamp(dt: chrono::NaiveDateTime) -> String {
    let base = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = dt.and_utc().timestamp_subsec_nanos();
    if nanos == 0 {
        base
    } else {
        format!("{base}.{}", format!("{nanos:09}").trim_end_matches('0'))
    }
}

/// Shift a rendered UTC date and time by `minutes` to get the local wall clock.
fn local_datetime(date: &str, time: &str, minutes: i16) -> String {
    let parsed =
        chrono::NaiveDateTime::parse_from_str(&format!("{date}T{time}"), "%Y-%m-%dT%H:%M:%S%.f");
    let Ok(dt) = parsed else {
        return format!("{date}T{time}");
    };
    let shifted = dt + ChronoDuration::minutes(i64::from(minutes));
    format_timestamp(shifted)
}

/// `datetimeoffset` stores its offset in minutes east of UTC.
fn offset_text(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let total = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", total / 60, total % 60)
}

/// One bound parameter.
///
/// `tiberius` implements `ToSql` for the primitive Rust types but not for
/// `u64` or `i128`, and it needs one concrete type behind `&dyn ToSql`, so
/// parameters go through this enum instead of a `Vec<Box<dyn ToSql>>`.
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Decimal(String),
    Uuid(Uuid),
}

impl ToSql for Param {
    fn to_sql(&self) -> ColumnData<'_> {
        match self {
            // An untyped NULL: the server coerces it to the parameter's type.
            Param::Null => ColumnData::I32(None),
            Param::Bool(v) => ColumnData::Bit(Some(*v)),
            Param::Int(v) => ColumnData::I64(Some(*v)),
            Param::UInt(v) => ColumnData::I64(Some(i64::try_from(*v).unwrap_or(i64::MAX))),
            Param::Float(v) => ColumnData::F64(Some(*v)),
            Param::Text(v) => ColumnData::String(Some(Cow::Borrowed(v.as_str()))),
            Param::Bytes(v) => ColumnData::Binary(Some(Cow::Borrowed(v.as_slice()))),
            Param::Decimal(v) => match decimal_param(v) {
                Some(numeric) => ColumnData::Numeric(Some(numeric)),
                // Unreachable: `to_param` validated the text already. Sending
                // the text keeps the statement runnable instead of panicking.
                None => ColumnData::String(Some(Cow::Borrowed(v.as_str()))),
            },
            Param::Uuid(v) => ColumnData::Guid(Some(*v)),
        }
    }
}

/// Encode one `fg-core` value as a bound parameter.
pub fn to_param(value: &Value) -> Result<Param> {
    Ok(match value {
        Value::Null => Param::Null,
        Value::Bool(v) => Param::Bool(*v),
        Value::Int(v) => Param::Int(*v),
        Value::UInt(v) => Param::UInt(*v),
        Value::Float(v) => Param::Float(*v),
        Value::Decimal(v) => {
            if decimal_param(v).is_none() {
                return Err(Error::Unsupported(format!(
                    "`{v}` is not a decimal SQL Server can bind"
                )));
            }
            Param::Decimal(v.clone())
        }
        // Temporal and textual values travel as text; the server converts them
        // with the target column's own parser, so one encoder covers `date`,
        // `time`, `datetime2`, `datetimeoffset`, `nvarchar` and `xml`.
        Value::Text(v)
        | Value::Json(v)
        | Value::Date(v)
        | Value::Time(v)
        | Value::Timestamp(v)
        | Value::TimestampTz(v)
        | Value::Interval(v)
        | Value::Other { text: v, .. } => Param::Text(v.clone()),
        Value::Uuid(v) => match Uuid::parse_str(v) {
            Ok(uuid) => Param::Uuid(uuid),
            Err(_) => Param::Text(v.clone()),
        },
        Value::Bytes(v) => Param::Bytes(v.clone()),
        Value::Array(_) => {
            return Err(Error::Unsupported(
                "array parameters have no SQL Server form".into(),
            ));
        }
        Value::Truncated(_) => {
            return Err(Error::Unsupported(
                "a truncated value cannot be sent as a parameter".into(),
            ));
        }
        // `Value` is `#[non_exhaustive]`: a value kind from a newer build has
        // no encoding this driver knows.
        _ => {
            return Err(Error::Unsupported(
                "this value kind has no SQL Server parameter encoding".into(),
            ));
        }
    })
}

/// Parse exact decimal text into the wire mantissa/scale pair.
fn decimal_param(text: &str) -> Option<Numeric> {
    let trimmed = text.trim();
    let (negative, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let (int_part, frac_part) = rest.split_once('.').unwrap_or((rest, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let scale = u8::try_from(frac_part.len())
        .ok()
        .filter(|s| *s <= MAX_SCALE)?;
    let mut mantissa: i128 = 0;
    for byte in int_part.bytes().chain(frac_part.bytes()) {
        mantissa = mantissa.checked_mul(10)?;
        mantissa = mantissa.checked_add(i128::from(byte - b'0'))?;
    }
    if negative {
        mantissa = -mantissa;
    }
    Some(Numeric::new_with_scale(mantissa, scale))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tiberius::time::{Date, DateTime2, Time};

    fn column(ty: ColumnType) -> Column {
        Column::new("c".to_owned(), ty)
    }

    #[test]
    fn integer_and_float_wire_types_keep_their_categories() {
        assert_eq!(
            data_type(&column(ColumnType::Int4)).category,
            TypeCategory::Integer
        );
        assert_eq!(
            data_type(&column(ColumnType::Float8)).category,
            TypeCategory::Float
        );
        assert_eq!(
            data_type(&column(ColumnType::Bitn)).category,
            TypeCategory::Bool
        );
        assert_eq!(
            data_type(&column(ColumnType::NVarchar)).native,
            "nvarchar".to_owned()
        );
    }

    #[test]
    fn decimals_render_without_floating_point() {
        let n = Numeric::new_with_scale(123456, 4);
        assert_eq!(numeric_string(n), "12.3456");
        let n = Numeric::new_with_scale(-5, 2);
        assert_eq!(numeric_string(n), "-0.05");
        let n = Numeric::new_with_scale(7, 0);
        assert_eq!(numeric_string(n), "7");
    }

    #[test]
    fn declared_types_keep_length_and_precision() {
        let t = data_type_from_native("nvarchar(50)");
        assert_eq!(t.category, TypeCategory::Text);
        assert_eq!(t.params.length, Some(50));
        let t = data_type_from_native("decimal(18,4)");
        assert_eq!(t.category, TypeCategory::Decimal);
        assert_eq!(t.params.precision, Some(18));
        assert_eq!(t.params.scale, Some(4));
        let t = data_type_from_native("varbinary(max)");
        assert_eq!(t.category, TypeCategory::Binary);
        assert_eq!(t.params.length, None);
    }

    #[test]
    fn temporal_wire_values_become_iso_text() {
        // 2024-03-02 is 738946 days after 0001-01-01.
        let date = Date::new(738_946);
        assert_eq!(date_from_year_one(date.days()), "2024-03-02");
        let time = Time::new(4_500_000_000, 7);
        assert_eq!(time_of_day(time.increments(), time.scale()), "00:07:30");
        let dt = DateTime2::new(Date::new(738_946), Time::new(1_234_567, 6));
        let Value::Timestamp(text) = decode(&ColumnData::DateTime2(Some(dt))) else {
            panic!("expected a timestamp");
        };
        assert_eq!(text, "2024-03-02T00:00:01.234567");
        // 1900-01-01 plus one day and half a second.
        assert_eq!(
            timestamp_from_1900(1, 150),
            "1900-01-02T00:00:00.5".to_owned()
        );
    }

    #[test]
    fn offsets_render_as_signed_hours_and_minutes() {
        assert_eq!(offset_text(480), "+08:00");
        assert_eq!(offset_text(-330), "-05:30");
        assert_eq!(offset_text(0), "+00:00");
    }

    #[test]
    fn decimal_parameters_reject_what_the_wire_cannot_carry() {
        assert!(decimal_param("12.3456").is_some());
        assert!(decimal_param("-0.5").is_some());
        assert!(decimal_param(".5").is_some());
        assert!(decimal_param("1e9").is_none());
        assert!(decimal_param("NaN").is_none());
        assert!(decimal_param("1.2.3").is_none());
        assert!(decimal_param(&format!("0.{}", "1".repeat(39))).is_none());
    }

    #[test]
    fn truncated_and_array_values_are_refused_as_parameters() {
        let truncated = Value::Truncated(fg_core::Truncated {
            kind: fg_core::LargeKind::Text,
            total_len: 10,
            preview: b"abc".to_vec(),
        });
        assert!(to_param(&truncated).is_err());
        assert!(to_param(&Value::Array(vec![Value::Int(1)])).is_err());
    }
}
