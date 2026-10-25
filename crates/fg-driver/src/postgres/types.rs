//! PostgreSQL type mapping and value conversion.
//!
//! Decoding goes through one [`Cell`] wrapper: tokio-postgres always asks the server for
//! binary format, and `Row::try_get` refuses a type unless its `FromSql` accepts it, so a
//! plain `String` read fails on enums, ranges, geometry and every other type without a
//! built-in decoder. `Cell` accepts everything and dispatches on the column's [`Type`]
//! inside `from_sql`, so exotic values still arrive instead of erroring the whole result.
//! Unknown simple types fall back to [`Value::Other`] with the type name and no text rather
//! than guessing at bytes whose format is unknown.
//!
//! Encoding is the mirror image: parameters are always sent in text format (see
//! [`PgTextParam`]), which the server parses with the column's own input function, so one
//! encoder covers every type including arrays and ranges.

use std::error::Error as StdError;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use fg_core::{DataType, Error, TypeCategory, Value};
use postgres_protocol::types::{self as wire, Range, RangeBound};
use postgres_types::{FromSql, IsNull, Kind, ToSql, Type};
use tokio_postgres::types::Format;

/// One decoded cell. Accepts every type; see the module docs.
pub struct Cell(pub Value);

impl<'a> FromSql<'a> for Cell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Cell, Box<dyn StdError + Sync + Send>> {
        decode(ty, raw).map(Cell)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn from_sql_null(_ty: &Type) -> Result<Cell, Box<dyn StdError + Sync + Send>> {
        Ok(Cell(Value::Null))
    }
}

/// Display type for a result column. `Type::name` for array types is the internal
/// `_int4` form, so arrays are rendered from their element type instead.
pub fn data_type(ty: &Type) -> DataType {
    match ty.kind() {
        Kind::Array(elem) => {
            DataType::new(format!("{}[]", canonical_name(elem)), TypeCategory::Array)
        }
        Kind::Enum(variants) => {
            DataType::new(ty.name(), TypeCategory::Enum).with_variants(variants.clone())
        }
        Kind::Range(_) | Kind::Multirange(_) => DataType::new(ty.name(), TypeCategory::Other),
        Kind::Domain(base) => data_type(base),
        Kind::Composite(_) => DataType::new(ty.name(), TypeCategory::Other),
        Kind::Simple | Kind::Pseudo => match *ty {
            Type::BOOL => DataType::new("boolean", TypeCategory::Bool),
            Type::INT2 => DataType::new("smallint", TypeCategory::Integer),
            Type::INT4 => DataType::new("integer", TypeCategory::Integer),
            Type::INT8 => DataType::new("bigint", TypeCategory::Integer),
            Type::FLOAT4 => DataType::new("real", TypeCategory::Float),
            Type::FLOAT8 => DataType::new("double precision", TypeCategory::Float),
            Type::NUMERIC => DataType::new("numeric", TypeCategory::Decimal),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
                DataType::new(ty.name(), TypeCategory::Text)
            }
            Type::BYTEA => DataType::new("bytea", TypeCategory::Binary),
            Type::DATE => DataType::new("date", TypeCategory::Date),
            Type::TIME => DataType::new("time", TypeCategory::Time),
            Type::TIMESTAMP => DataType::new("timestamp", TypeCategory::Timestamp),
            Type::TIMESTAMPTZ => DataType::new("timestamptz", TypeCategory::TimestampTz),
            Type::INTERVAL => DataType::new("interval", TypeCategory::Interval),
            Type::UUID => DataType::new("uuid", TypeCategory::Uuid),
            Type::JSON | Type::JSONB => DataType::new(ty.name(), TypeCategory::Json),
            Type::BIT | Type::VARBIT => DataType::new(ty.name(), TypeCategory::Bit),
            Type::INET | Type::CIDR => DataType::new(ty.name(), TypeCategory::Network),
            Type::POINT
            | Type::LSEG
            | Type::BOX
            | Type::PATH
            | Type::POLYGON
            | Type::CIRCLE
            | Type::LINE => DataType::new(ty.name(), TypeCategory::Geometry),
            _ => DataType::new(ty.name(), TypeCategory::Other),
        },
        // `Kind` is non_exhaustive: unknown future kinds degrade to Other.
        _ => DataType::new(ty.name(), TypeCategory::Other),
    }
}

/// Display name for a type: `format_type` spells `int4` as `integer`, `float8` as
/// `double precision`, and so on. Used for array elements, whose `Type::name` is the
/// internal form.
fn canonical_name(ty: &Type) -> &str {
    match *ty {
        Type::BOOL => "boolean",
        Type::INT2 => "smallint",
        Type::INT4 => "integer",
        Type::INT8 => "bigint",
        Type::FLOAT4 => "real",
        Type::FLOAT8 => "double precision",
        Type::VARCHAR => "character varying",
        Type::BPCHAR => "character",
        _ => ty.name(),
    }
}

fn other(ty: &Type) -> Value {
    Value::Other {
        type_name: ty.name().to_owned(),
        text: String::new(),
    }
}

/// Map a `format_type()` string (as `pg_attribute` reports it) to a [`DataType`].
/// Used for `describe`, where the type arrives as text rather than a [`Type`].
/// Array suffixes stack (`integer[][]`); parameters in parentheses feed
/// `TypeParams` for `varchar(n)` and `numeric(p, s)`.
pub fn data_type_from_native(native: &str) -> DataType {
    use fg_core::TypeParams;
    let mut rest = native.trim();
    let mut dims = 0;
    while let Some(stripped) = rest.strip_suffix("[]") {
        rest = stripped.trim_end();
        dims += 1;
    }
    let (base, params) = match rest.split_once('(') {
        Some((base, args)) => {
            let args = args.trim_end_matches(')');
            let nums: Vec<Option<u32>> = args
                .split(',')
                .map(|n| n.trim().parse::<u32>().ok())
                .collect();
            (base.trim(), nums)
        }
        None => (rest, Vec::new()),
    };
    let lower = base.to_ascii_lowercase();
    let mut category = match lower.as_str() {
        "boolean" | "bool" => TypeCategory::Bool,
        "smallint" | "int2" | "integer" | "int" | "int4" | "bigint" | "int8" => {
            TypeCategory::Integer
        }
        "real" | "float4" | "double precision" | "float8" => TypeCategory::Float,
        "numeric" | "decimal" => TypeCategory::Decimal,
        "character varying" | "varchar" | "character" | "char" | "text" | "name" | "citext" => {
            TypeCategory::Text
        }
        "bytea" => TypeCategory::Binary,
        "date" => TypeCategory::Date,
        "time without time zone" | "time" => TypeCategory::Time,
        "timestamp without time zone" | "timestamp" => TypeCategory::Timestamp,
        "timestamp with time zone" | "timestamptz" => TypeCategory::TimestampTz,
        "interval" => TypeCategory::Interval,
        "uuid" => TypeCategory::Uuid,
        "json" | "jsonb" => TypeCategory::Json,
        "bit" | "bit varying" | "varbit" => TypeCategory::Bit,
        "inet" | "cidr" => TypeCategory::Network,
        "point" | "lseg" | "box" | "path" | "polygon" | "circle" | "line" => TypeCategory::Geometry,
        _ => TypeCategory::Other,
    };
    if dims > 0 {
        category = TypeCategory::Array;
    }
    let mut type_params = TypeParams::default();
    match category {
        TypeCategory::Decimal => {
            if let Some(Some(p)) = params.first() {
                type_params.precision = Some(*p);
            }
            if let Some(Some(s)) = params.get(1) {
                type_params.scale = Some(*s);
            }
        }
        TypeCategory::Text => {
            if let Some(Some(length)) = params.first() {
                type_params.length = Some(*length);
            }
        }
        _ => {}
    }
    DataType::new(native, category).with_params(type_params)
}

/// Decode one non-null cell from its binary payload.
pub fn decode(ty: &Type, raw: &[u8]) -> Result<Value, Box<dyn StdError + Sync + Send>> {
    // Domains decode as their base type; the wrapper name is kept for display only.
    if let Kind::Domain(base) = ty.kind() {
        return decode(base, raw);
    }
    if let Kind::Enum(_) = ty.kind() {
        return Ok(Value::Text(wire::text_from_sql(raw)?.to_owned()));
    }
    if let Kind::Array(elem) = ty.kind() {
        return decode_array(elem, raw);
    }
    if let Kind::Range(elem) = ty.kind() {
        return Ok(Value::Other {
            type_name: ty.name().to_owned(),
            text: decode_range(elem, raw)?,
        });
    }
    if let Kind::Multirange(elem) = ty.kind() {
        return decode_multirange(elem, raw).map(|text| Value::Other {
            type_name: ty.name().to_owned(),
            text,
        });
    }
    if let Kind::Composite(fields) = ty.kind() {
        return Ok(Value::Other {
            type_name: ty.name().to_owned(),
            text: decode_composite(fields, raw)?,
        });
    }
    Ok(match *ty {
        Type::BOOL => Value::Bool(wire::bool_from_sql(raw)?),
        Type::INT2 => Value::Int(i64::from(wire::int2_from_sql(raw)?)),
        Type::INT4 => Value::Int(i64::from(wire::int4_from_sql(raw)?)),
        Type::INT8 => Value::Int(wire::int8_from_sql(raw)?),
        Type::FLOAT4 => Value::Float(f64::from(wire::float4_from_sql(raw)?)),
        Type::FLOAT8 => Value::Float(wire::float8_from_sql(raw)?),
        Type::NUMERIC => Value::Decimal(numeric_to_string(raw)?),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            Value::Text(String::from_utf8_lossy(raw).into_owned())
        }
        Type::BYTEA => Value::Bytes(wire::bytea_from_sql(raw).to_vec()),
        Type::DATE => Value::Date(pg_date(wire::date_from_sql(raw)?)),
        Type::TIME => Value::Time(pg_time(wire::time_from_sql(raw)?)),
        Type::TIMESTAMP => Value::Timestamp(pg_timestamp(wire::timestamp_from_sql(raw)?)),
        Type::TIMESTAMPTZ => Value::TimestampTz(pg_timestamptz(wire::timestamp_from_sql(raw)?)),
        Type::INTERVAL => Value::Interval(interval_to_string(raw)?),
        Type::UUID => Value::Uuid(uuid_to_string(wire::uuid_from_sql(raw)?)),
        Type::JSON => Value::Json(String::from_utf8_lossy(raw).into_owned()),
        Type::JSONB => Value::Json(jsonb_to_string(raw)),
        Type::BIT | Type::VARBIT => Value::Text(bit_to_string(raw)?),
        Type::INET | Type::CIDR => Value::Text(inet_to_string(raw)?),
        Type::MACADDR => Value::Text(macaddr_to_string(raw)?),
        Type::POINT => Value::Other {
            type_name: ty.name().to_owned(),
            text: point_to_string(raw)?,
        },
        Type::LSEG => Value::Other {
            type_name: ty.name().to_owned(),
            text: lseg_to_string(raw)?,
        },
        Type::BOX => Value::Other {
            type_name: ty.name().to_owned(),
            text: box_to_string(raw)?,
        },
        Type::PATH => Value::Other {
            type_name: ty.name().to_owned(),
            text: path_to_string(raw)?,
        },
        Type::POLYGON => Value::Other {
            type_name: ty.name().to_owned(),
            text: polygon_to_string(raw)?,
        },
        Type::CIRCLE => Value::Other {
            type_name: ty.name().to_owned(),
            text: circle_to_string(raw)?,
        },
        Type::LINE => Value::Other {
            type_name: ty.name().to_owned(),
            text: line_to_string(raw)?,
        },
        _ => other(ty),
    })
}

/// PostgreSQL's day zero is 2000-01-01.
fn pg_epoch() -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(2000, 1, 1)
}

fn pg_date(days: i32) -> String {
    pg_epoch()
        .and_then(|e| e.checked_add_signed(Duration::days(i64::from(days))))
        .map_or_else(
            || format!("date({days})"),
            |d| d.format("%Y-%m-%d").to_string(),
        )
}

fn pg_time(micros: i64) -> String {
    if micros == 24 * 60 * 60 * 1_000_000 {
        return "24:00:00".to_owned();
    }
    let secs = micros.div_euclid(1_000_000);
    let nanos = u32::try_from(micros.rem_euclid(1_000_000) * 1_000).unwrap_or(0);
    NaiveTime::from_num_seconds_from_midnight_opt(u32::try_from(secs).unwrap_or(0), nanos)
        .map_or_else(
            || format!("time({micros})"),
            |t| {
                let base = t.format("%H:%M:%S").to_string();
                if nanos == 0 {
                    base
                } else {
                    format!("{}.{}", base, format!("{nanos:09}").trim_end_matches('0'))
                }
            },
        )
}

fn pg_timestamp(micros: i64) -> String {
    pg_epoch()
        .and_then(|e| e.and_hms_opt(0, 0, 0))
        .and_then(|dt| dt.checked_add_signed(Duration::microseconds(micros)))
        .map_or_else(|| format!("timestamp({micros})"), format_timestamp)
}

fn format_timestamp(dt: NaiveDateTime) -> String {
    let base = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = dt.and_utc().timestamp_subsec_nanos();
    if nanos == 0 {
        base
    } else {
        format!("{}.{}", base, format!("{nanos:09}").trim_end_matches('0'))
    }
}

/// Timestamps with time zone render in UTC: the binary value is an absolute instant and
/// the session's time zone is not available to a stateless decoder. The instant is exact
/// and round-trips; only the displayed zone differs from psql's session-zone rendering.
fn pg_timestamptz(micros: i64) -> String {
    let Some(naive) = pg_epoch()
        .and_then(|e| e.and_hms_opt(0, 0, 0))
        .and_then(|dt| dt.checked_add_signed(Duration::microseconds(micros)))
    else {
        return format!("timestamptz({micros})");
    };
    DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

/// Numeric binary format: ndigits, weight, sign, dscale, then base-10000 digit groups.
/// The value is sum(digit[i] * 10000^(weight - i)); dscale is the displayed fraction digits.
fn numeric_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    use byteorder::{BigEndian, ReadBytesExt};
    let mut buf = raw;
    let ndigits = buf.read_i16::<BigEndian>()?;
    let weight = buf.read_i16::<BigEndian>()?;
    let sign = buf.read_u16::<BigEndian>()?;
    let dscale = buf.read_u16::<BigEndian>()?;
    if sign == 0xC000 {
        return Ok("NaN".to_owned());
    }
    let mut digits = Vec::with_capacity(ndigits.max(0) as usize);
    for _ in 0..ndigits.max(0) {
        digits.push(buf.read_i16::<BigEndian>()?);
    }
    let negative = sign == 0x4000;
    // Integer groups are the first weight + 1 groups; the rest are fractional.
    let mut int_part = String::new();
    let mut frac_digits = String::new();
    if weight >= 0 {
        let int_groups = (weight as usize + 1).min(digits.len());
        for (i, g) in digits.iter().take(int_groups).enumerate() {
            if i == 0 {
                int_part.push_str(&g.to_string());
            } else {
                int_part.push_str(&format!("{g:04}"));
            }
        }
        for _ in int_groups..weight as usize + 1 {
            int_part.push_str("0000");
        }
        for g in digits.iter().skip(int_groups) {
            frac_digits.push_str(&format!("{g:04}"));
        }
    } else {
        int_part.push('0');
        for _ in 0..(-weight as usize - 1) * 4 {
            frac_digits.push('0');
        }
        for g in &digits {
            frac_digits.push_str(&format!("{g:04}"));
        }
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(if int_part.is_empty() { "0" } else { &int_part });
    if dscale > 0 {
        let dscale = dscale as usize;
        while frac_digits.len() < dscale {
            frac_digits.push('0');
        }
        frac_digits.truncate(dscale);
        out.push('.');
        out.push_str(&frac_digits);
    }
    Ok(out)
}

/// Interval binary format: microseconds, days, months.
fn interval_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    use byteorder::{BigEndian, ReadBytesExt};
    let mut buf = raw;
    let micros = buf.read_i64::<BigEndian>()?;
    let days = buf.read_i32::<BigEndian>()?;
    let months = buf.read_i32::<BigEndian>()?;
    let years = months / 12;
    let mons = months % 12;
    let mut parts: Vec<String> = Vec::new();
    if years != 0 {
        parts.push(if years == 1 || years == -1 {
            format!("{years} year")
        } else {
            format!("{years} years")
        });
    }
    if mons != 0 {
        parts.push(if mons == 1 || mons == -1 {
            format!("{mons} mon")
        } else {
            format!("{mons} mons")
        });
    }
    if days != 0 {
        parts.push(if days == 1 || days == -1 {
            format!("{days} day")
        } else {
            format!("{days} days")
        });
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        let abs = micros.unsigned_abs();
        let secs = abs / 1_000_000;
        let frac = abs % 1_000_000;
        let time = if frac == 0 {
            format!(
                "{sign}{:02}:{:02}:{:02}",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60
            )
        } else {
            format!(
                "{sign}{:02}:{:02}:{:02}.{}",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60,
                format!("{frac:06}").trim_end_matches('0')
            )
        };
        parts.push(time);
    }
    Ok(parts.join(" "))
}

fn uuid_to_string(bytes: [u8; 16]) -> String {
    let h = |b: u8| format!("{b:02x}");
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h(bytes[0]),
        h(bytes[1]),
        h(bytes[2]),
        h(bytes[3]),
        h(bytes[4]),
        h(bytes[5]),
        h(bytes[6]),
        h(bytes[7]),
        h(bytes[8]),
        h(bytes[9]),
        h(bytes[10]),
        h(bytes[11]),
        h(bytes[12]),
        h(bytes[13]),
        h(bytes[14]),
        h(bytes[15]),
    )
}

/// JSONB binary is a version byte followed by the JSON text.
fn jsonb_to_string(raw: &[u8]) -> String {
    let text = if raw.first() == Some(&1) {
        &raw[1..]
    } else {
        raw
    };
    String::from_utf8_lossy(text).into_owned()
}

fn bit_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    let varbit = wire::varbit_from_sql(raw)?;
    let mut out = String::with_capacity(varbit.len());
    for i in 0..varbit.len() {
        let byte = varbit.bytes().get(i / 8).copied().unwrap_or(0);
        out.push(if byte & (0x80 >> (i % 8)) != 0 {
            '1'
        } else {
            '0'
        });
    }
    Ok(out)
}

fn inet_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    if raw.len() < 4 {
        return Err("short inet value".into());
    }
    let family = raw[0];
    let bits = raw[1];
    let is_cidr = raw[2] != 0;
    let addr_len = raw[3] as usize;
    let addr = raw.get(4..4 + addr_len).ok_or("short inet address")?;
    let ip = match (family, addr_len) {
        (2, 4) => format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3]),
        (3, 16) => {
            let groups: Vec<String> = addr
                .chunks(2)
                .map(|c| format!("{:x}", u16::from_be_bytes([c[0], c[1]])))
                .collect();
            // Full IPv6 expansion is unambiguous; compression is cosmetic.
            groups.join(":")
        }
        _ => return Err("unknown inet family".into()),
    };
    let max_bits = if family == 2 { 32 } else { 128 };
    if is_cidr || bits != max_bits {
        Ok(format!("{ip}/{bits}"))
    } else {
        Ok(ip)
    }
}

fn macaddr_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    let bytes = wire::macaddr_from_sql(raw)?;
    Ok(bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

fn f64_at(raw: &[u8], offset: usize) -> Result<f64, Box<dyn StdError + Sync + Send>> {
    use byteorder::{BigEndian, ReadBytesExt};
    let mut slice = raw.get(offset..offset + 8).ok_or("short geometry value")?;
    Ok(slice.read_f64::<BigEndian>()?)
}

fn point_text(x: f64, y: f64) -> String {
    format!("({x},{y})")
}

fn point_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    Ok(point_text(f64_at(raw, 0)?, f64_at(raw, 8)?))
}

fn lseg_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    Ok(format!(
        "[{},{}]",
        point_text(f64_at(raw, 0)?, f64_at(raw, 8)?),
        point_text(f64_at(raw, 16)?, f64_at(raw, 24)?)
    ))
}

fn box_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    Ok(format!(
        "{},{}",
        point_text(f64_at(raw, 0)?, f64_at(raw, 8)?),
        point_text(f64_at(raw, 16)?, f64_at(raw, 24)?)
    ))
}

fn points_to_string(
    raw: &[u8],
    offset: usize,
    count: usize,
) -> Result<Vec<String>, Box<dyn StdError + Sync + Send>> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = offset + i * 16;
        out.push(point_text(f64_at(raw, base)?, f64_at(raw, base + 8)?));
    }
    Ok(out)
}

fn path_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    use byteorder::{BigEndian, ReadBytesExt};
    let mut buf = raw;
    let closed = buf.read_i32::<BigEndian>()? != 0;
    let count = buf.read_i32::<BigEndian>()?;
    let points = points_to_string(raw, 8, count.max(0) as usize)?;
    if closed {
        Ok(format!("[{}]", points.join(",")))
    } else {
        Ok(format!("({})", points.join(",")))
    }
}

fn polygon_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    use byteorder::{BigEndian, ReadBytesExt};
    let mut buf = raw;
    let count = buf.read_i32::<BigEndian>()?;
    let points = points_to_string(raw, 4, count.max(0) as usize)?;
    Ok(format!("({})", points.join(",")))
}

fn circle_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    Ok(format!(
        "<{},{}>",
        point_text(f64_at(raw, 0)?, f64_at(raw, 8)?),
        f64_at(raw, 16)?
    ))
}

fn line_to_string(raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    Ok(format!(
        "{{{},{},{}}}",
        f64_at(raw, 0)?,
        f64_at(raw, 8)?,
        f64_at(raw, 16)?
    ))
}

/// Array payloads: dimensions first, then flat elements. Nested dimensions reshape the
/// flat values; a zero-dimension array is empty.
fn decode_array(elem: &Type, raw: &[u8]) -> Result<Value, Box<dyn StdError + Sync + Send>> {
    use fallible_iterator02::FallibleIterator;
    let array = wire::array_from_sql(raw)?;
    let dims: Vec<i32> = array
        .dimensions()
        .collect::<Vec<_>>()?
        .into_iter()
        .map(|d| d.len)
        .collect();
    let mut flat: Vec<Value> = Vec::new();
    let mut values = array.values();
    while let Some(element) = values.next()? {
        match element {
            None => flat.push(Value::Null),
            Some(bytes) => flat.push(decode(elem, bytes)?),
        }
    }
    Ok(reshape(&dims, &mut flat.into_iter()))
}

fn reshape(dims: &[i32], values: &mut std::vec::IntoIter<Value>) -> Value {
    if dims.is_empty() {
        return values.next().unwrap_or(Value::Null);
    }
    let (first, rest) = (dims[0].max(0) as usize, &dims[1..]);
    let mut out = Vec::with_capacity(first);
    for _ in 0..first {
        out.push(reshape(rest, values));
    }
    Value::Array(out)
}

fn bound_text(
    elem: &Type,
    bound: &RangeBound<Option<&[u8]>>,
) -> Result<String, Box<dyn StdError + Sync + Send>> {
    match bound {
        RangeBound::Unbounded => Ok(String::new()),
        RangeBound::Inclusive(None) | RangeBound::Exclusive(None) => Ok("NULL".to_owned()),
        RangeBound::Inclusive(Some(bytes)) | RangeBound::Exclusive(Some(bytes)) => {
            value_text(&decode(elem, bytes)?)
        }
    }
}

fn decode_range(elem: &Type, raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    match wire::range_from_sql(raw)? {
        Range::Empty => Ok("empty".to_owned()),
        Range::Nonempty(lower, upper) => {
            let (open, lower_text) = match &lower {
                RangeBound::Unbounded => ("(", String::new()),
                RangeBound::Inclusive(_) => ("[", bound_text(elem, &lower)?),
                RangeBound::Exclusive(_) => ("(", bound_text(elem, &lower)?),
            };
            let (close, upper_text) = match &upper {
                RangeBound::Unbounded => (")", String::new()),
                RangeBound::Inclusive(_) => ("]", bound_text(elem, &upper)?),
                RangeBound::Exclusive(_) => (")", bound_text(elem, &upper)?),
            };
            Ok(format!("{open}{lower_text},{upper_text}{close}"))
        }
    }
}

fn decode_multirange(elem: &Type, raw: &[u8]) -> Result<String, Box<dyn StdError + Sync + Send>> {
    use fallible_iterator02::FallibleIterator;
    // A multirange is an array of ranges; its element type is the range type.
    let array = wire::array_from_sql(raw)?;
    let mut parts = Vec::new();
    let mut values = array.values();
    while let Some(element) = values.next()? {
        match element {
            None => parts.push("NULL".to_owned()),
            Some(bytes) => {
                let text = decode_range(elem, bytes)?;
                if text.contains(',') || text.contains('"') {
                    parts.push(format!("\"{}\"", text.replace('"', "\\\"")));
                } else {
                    parts.push(text);
                }
            }
        }
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

fn decode_composite(
    fields: &[postgres_types::Field],
    raw: &[u8],
) -> Result<String, Box<dyn StdError + Sync + Send>> {
    use byteorder::{BigEndian, ReadBytesExt};
    let mut buf = raw;
    let count = buf.read_i32::<BigEndian>()?;
    let mut texts = Vec::new();
    for _ in 0..count.max(0) {
        let _oid = buf.read_i32::<BigEndian>()?;
        let len = buf.read_i32::<BigEndian>()?;
        if len < 0 {
            texts.push(String::new());
            continue;
        }
        let len = len as usize;
        let (bytes, tail) = buf.split_at_checked(len).ok_or("short composite value")?;
        buf = tail;
        // Field types come from the kind in order; a mismatch degrades to raw text.
        let field_ty = fields.get(texts.len()).map(|f| f.type_().clone());
        let text = match field_ty {
            Some(ty) => value_text(&decode(&ty, bytes)?)
                .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned()),
            None => String::from_utf8_lossy(bytes).into_owned(),
        };
        texts.push(quote_record_field(&text));
    }
    Ok(format!("({})", texts.join(",")))
}

/// Record output quoting: quote when the field contains a comma, parenthesis, quote,
/// backslash or whitespace, or is empty (which would otherwise read as NULL).
fn quote_record_field(text: &str) -> String {
    if text.is_empty()
        || text
            .chars()
            .any(|c| c == ',' || c == '(' || c == ')' || c == '"' || c == '\\' || c.is_whitespace())
    {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        text.to_owned()
    }
}

/// Plain-text rendering of a decoded value, for ranges and composite fields.
fn value_text(value: &Value) -> Result<String, Box<dyn StdError + Sync + Send>> {
    Ok(match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => s.clone(),
        Value::Bytes(b) => format!(
            "\\x{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(value_text).collect::<Result<_, _>>()?;
            format!("{{{}}}", parts.join(","))
        }
        Value::Other { text, .. } => text.clone(),
        Value::Truncated(_) => return Err("cannot render a truncated value".into()),
        // `Value` is non_exhaustive: refuse future variants rather than guessing.
        _ => return Err("cannot render this value".into()),
    })
}

/// A parameter encoded in text format: the server parses it with the target type's own
/// input function, so one encoder covers scalars, arrays, ranges and exotic types.
/// `None` is SQL NULL.
#[derive(Debug)]
pub struct PgTextParam(pub Option<String>);

impl ToSql for PgTextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        buf: &mut bytes::BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(text) => {
                buf.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        buf: &mut bytes::BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        if !Self::accepts(ty) {
            return Err("type mismatch".into());
        }
        self.to_sql(ty, buf)
    }
}

/// Render a [`Value`] as the PostgreSQL text literal its input function expects.
pub fn to_text_param(value: &Value) -> Result<PgTextParam, Error> {
    let text = match value {
        Value::Null => return Ok(PgTextParam(None)),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => {
            if f.is_nan() {
                "NaN".to_owned()
            } else if *f == f64::INFINITY {
                "Infinity".to_owned()
            } else if *f == f64::NEG_INFINITY {
                "-Infinity".to_owned()
            } else {
                f.to_string()
            }
        }
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => s.clone(),
        Value::Bytes(b) => format!(
            "\\x{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| {
                    let text = to_text_param(item)?.0;
                    Ok(match text {
                        None => "NULL".to_owned(),
                        Some(text) => {
                            // Array literals quote elements containing a comma, brace,
                            // quote, backslash or whitespace; everything else stays bare.
                            if text.is_empty()
                                || text.chars().any(|c| {
                                    c == ','
                                        || c == '{'
                                        || c == '}'
                                        || c == '"'
                                        || c == '\\'
                                        || c.is_whitespace()
                                })
                            {
                                format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
                            } else {
                                text
                            }
                        }
                    })
                })
                .collect::<Result<_, Error>>()?;
            format!("{{{}}}", parts.join(","))
        }
        Value::Other { text, .. } => text.clone(),
        Value::Truncated(_) => {
            return Err(Error::Query(fg_core::QueryError::new(
                "refusing to write a truncated value; load the full value first",
            )));
        }
        // `Value` is non_exhaustive: refuse future variants rather than guessing.
        _ => {
            return Err(Error::Query(fg_core::QueryError::new(
                "refusing to write an unknown value kind",
            )));
        }
    };
    Ok(PgTextParam(Some(text)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use byteorder::{BigEndian, WriteBytesExt};

    fn numeric_bytes(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.write_i16::<BigEndian>(ndigits).unwrap();
        buf.write_i16::<BigEndian>(weight).unwrap();
        buf.write_u16::<BigEndian>(sign).unwrap();
        buf.write_u16::<BigEndian>(dscale).unwrap();
        for d in digits {
            buf.write_i16::<BigEndian>(*d).unwrap();
        }
        buf
    }

    #[test]
    fn numeric_renders_with_precision() {
        // 123.45 -> digits [123, 4500], weight 0, dscale 2.
        assert_eq!(
            numeric_to_string(&numeric_bytes(2, 0, 0, 2, &[123, 4500])).unwrap(),
            "123.45"
        );
        // 12345.67 -> digits [1, 2345, 6700], weight 1, dscale 2.
        assert_eq!(
            numeric_to_string(&numeric_bytes(3, 1, 0, 2, &[1, 2345, 6700])).unwrap(),
            "12345.67"
        );
        // 0.0001 -> digits [1], weight -1, dscale 4.
        assert_eq!(
            numeric_to_string(&numeric_bytes(1, -1, 0, 4, &[1])).unwrap(),
            "0.0001"
        );
        // -10000 -> digits [1], weight 1, dscale 0.
        assert_eq!(
            numeric_to_string(&numeric_bytes(1, 1, 0x4000, 0, &[1])).unwrap(),
            "-10000"
        );
        // Trailing scale kept: 1.20 -> digits [1, 2000], dscale 2.
        assert_eq!(
            numeric_to_string(&numeric_bytes(2, 0, 0, 2, &[1, 2000])).unwrap(),
            "1.20"
        );
        assert_eq!(
            numeric_to_string(&numeric_bytes(0, 0, 0xC000, 0, &[])).unwrap(),
            "NaN"
        );
    }

    #[test]
    fn interval_renders_like_postgres() {
        // 1 year 2 mons 3 days 04:05:06.123456
        let mut buf = Vec::new();
        buf.write_i64::<BigEndian>(
            4 * 3600 * 1_000_000 + 5 * 60 * 1_000_000 + 6 * 1_000_000 + 123456,
        )
        .unwrap();
        buf.write_i32::<BigEndian>(3).unwrap();
        buf.write_i32::<BigEndian>(14).unwrap();
        assert_eq!(
            interval_to_string(&buf).unwrap(),
            "1 year 2 mons 3 days 04:05:06.123456"
        );
        // Zero interval.
        let mut buf = Vec::new();
        buf.write_i64::<BigEndian>(0).unwrap();
        buf.write_i32::<BigEndian>(0).unwrap();
        buf.write_i32::<BigEndian>(0).unwrap();
        assert_eq!(interval_to_string(&buf).unwrap(), "00:00:00");
        // Negative time only.
        let mut buf = Vec::new();
        buf.write_i64::<BigEndian>(-90 * 1_000_000).unwrap();
        buf.write_i32::<BigEndian>(0).unwrap();
        buf.write_i32::<BigEndian>(0).unwrap();
        assert_eq!(interval_to_string(&buf).unwrap(), "-00:01:30");
    }

    #[test]
    fn uuid_and_jsonb_and_bit_decode() {
        let bytes: [u8; 16] = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        assert_eq!(
            uuid_to_string(bytes),
            "12345678-9abc-def0-1234-56789abcdef0"
        );
        assert_eq!(jsonb_to_string(&[1, b'{', b'}']), "{}");
    }

    #[test]
    fn text_params_quote_arrays() {
        let param = to_text_param(&Value::Array(vec![
            Value::Text("a\"b".into()),
            Value::Null,
            Value::Int(3),
        ]))
        .unwrap();
        assert_eq!(param.0.as_deref(), Some("{\"a\\\"b\",NULL,3}"));
        assert_eq!(to_text_param(&Value::Null).unwrap().0, None);
        assert_eq!(
            to_text_param(&Value::Bytes(vec![0x00, 0xff]))
                .unwrap()
                .0
                .as_deref(),
            Some("\\x00ff")
        );
    }

    fn array_bytes(elem_oid: u32, dims: &[(i32, i32)], elements: &[Option<i32>]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.write_i32::<BigEndian>(dims.len() as i32).unwrap();
        buf.write_i32::<BigEndian>(if elements.iter().any(|e| e.is_none()) {
            1
        } else {
            0
        })
        .unwrap();
        buf.write_u32::<BigEndian>(elem_oid).unwrap();
        for (len, lb) in dims {
            buf.write_i32::<BigEndian>(*len).unwrap();
            buf.write_i32::<BigEndian>(*lb).unwrap();
        }
        for element in elements {
            match element {
                None => buf.write_i32::<BigEndian>(-1).unwrap(),
                Some(value) => {
                    buf.write_i32::<BigEndian>(4).unwrap();
                    buf.write_i32::<BigEndian>(*value).unwrap();
                }
            }
        }
        buf
    }

    #[test]
    fn arrays_reshape_and_keep_nulls() {
        // {1,2} as int4[]
        let payload = array_bytes(23, &[(2, 1)], &[Some(1), Some(2)]);
        assert_eq!(
            decode(&Type::INT4_ARRAY, &payload).unwrap(),
            Value::Array(vec![Value::Int(1), Value::Int(2)])
        );
        // {{1,2},{3,NULL}} as int4[][]
        let payload = array_bytes(23, &[(2, 1), (2, 1)], &[Some(1), Some(2), Some(3), None]);
        assert_eq!(
            decode(&Type::INT4_ARRAY, &payload).unwrap(),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Null]),
            ])
        );
    }

    #[test]
    fn ranges_render_in_postgres_notation() {
        // int4range '[1,10)': lower-inclusive flag, then two int4 bounds.
        let mut buf = vec![0x02u8];
        buf.write_i32::<BigEndian>(4).unwrap();
        buf.write_i32::<BigEndian>(1).unwrap();
        buf.write_i32::<BigEndian>(4).unwrap();
        buf.write_i32::<BigEndian>(10).unwrap();
        assert_eq!(
            decode(&Type::INT4_RANGE, &buf).unwrap(),
            Value::Other {
                type_name: "int4range".into(),
                text: "[1,10)".into(),
            }
        );
        assert_eq!(
            decode(&Type::INT4_RANGE, &[0x01]).unwrap(),
            Value::Other {
                type_name: "int4range".into(),
                text: "empty".into(),
            }
        );
    }

    #[test]
    fn data_type_names_arrays_and_enums() {
        let array = Type::INT4_ARRAY;
        assert_eq!(data_type(&array).native, "integer[]");
        assert_eq!(data_type(&array).category, TypeCategory::Array);
        assert_eq!(data_type(&Type::NUMERIC).category, TypeCategory::Decimal);
        assert_eq!(data_type(&Type::INET).category, TypeCategory::Network);
        assert_eq!(data_type(&Type::POINT).category, TypeCategory::Geometry);
    }
}
