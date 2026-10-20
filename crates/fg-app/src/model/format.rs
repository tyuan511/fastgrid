//! How a cell value is shown in the grid, and how it is copied.
//!
//! Rules from the interaction spec: `NULL` must never be confusable with the text "NULL"; an
//! empty string is shown blank; numbers, dates and ids are monospaced; big binary/JSON values are
//! summarised as pills instead of being laid out.

use fg_core::{LargeKind, TypeCategory, Value};

/// How the grid should style a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// Rendered as a muted italic pill, never as the string "NULL".
    Null,
    /// Proportional font.
    Text,
    /// Monospace: ids, dates, UUIDs, JSON.
    Mono,
    /// Monospace, right-aligned.
    Number,
    Bool,
    /// A summary chip such as `[BLOB 45 KB]`.
    Pill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDisplay {
    pub kind: CellKind,
    pub text: String,
}

impl CellDisplay {
    fn new(kind: CellKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// A cell never lays out more than this many characters.
const MAX_CHARS: usize = 300;
/// JSON longer than this becomes a `[JSON n keys]` pill.
const INLINE_JSON_CHARS: usize = 120;

pub fn display(value: &Value) -> CellDisplay {
    match value {
        Value::Null => CellDisplay::new(CellKind::Null, "NULL"),
        Value::Bool(b) => CellDisplay::new(CellKind::Bool, if *b { "true" } else { "false" }),
        Value::Int(i) => CellDisplay::new(CellKind::Number, i.to_string()),
        Value::UInt(u) => CellDisplay::new(CellKind::Number, u.to_string()),
        Value::Float(f) => CellDisplay::new(CellKind::Number, float_text(*f)),
        Value::Decimal(s) => CellDisplay::new(CellKind::Number, s.clone()),
        Value::Text(s) => CellDisplay::new(CellKind::Text, one_line(s)),
        Value::Bytes(b) => CellDisplay::new(
            CellKind::Pill,
            format!("[BLOB {}]", human_bytes(b.len() as u64)),
        ),
        Value::Json(s) => json_display(s),
        Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => CellDisplay::new(CellKind::Mono, s.clone()),
        Value::Array(items) => CellDisplay::new(CellKind::Mono, array_text(items)),
        Value::Truncated(t) => match t.kind {
            LargeKind::Text => {
                let preview = String::from_utf8_lossy(&t.preview);
                CellDisplay::new(
                    CellKind::Text,
                    format!("{}… ({})", one_line(&preview), human_bytes(t.total_len)),
                )
            }
            LargeKind::Json => CellDisplay::new(
                CellKind::Pill,
                format!("[JSON {}]", human_bytes(t.total_len)),
            ),
            _ => CellDisplay::new(
                CellKind::Pill,
                format!("[BLOB {}]", human_bytes(t.total_len)),
            ),
        },
        Value::Other { text, .. } => CellDisplay::new(CellKind::Text, one_line(text)),
        // A value type added after this build: show something rather than nothing.
        _ => CellDisplay::new(CellKind::Text, "…"),
    }
}

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        f.to_string()
    }
}

/// Collapse line breaks so a cell stays on one line, and cap its length.
fn one_line(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(match c {
            '\n' | '\r' => '↵',
            '\t' => ' ',
            c if c.is_control() => '·',
            c => c,
        });
    }
    out
}

fn json_display(s: &str) -> CellDisplay {
    if s.chars().count() <= INLINE_JSON_CHARS {
        return CellDisplay::new(CellKind::Mono, one_line(s));
    }
    let summary = match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(m)) => format!("{} keys", m.len()),
        Ok(serde_json::Value::Array(a)) => format!("{} items", a.len()),
        _ => human_bytes(s.len() as u64),
    };
    CellDisplay::new(CellKind::Pill, format!("[JSON {summary}]"))
}

fn array_text(items: &[Value]) -> String {
    let inner: Vec<String> = items.iter().map(copy_text).collect();
    one_line(&format!("{{{}}}", inner.join(", ")))
}

/// Which alignment a column of this type gets.
pub fn align_for(category: TypeCategory) -> Align {
    match category {
        TypeCategory::Integer | TypeCategory::Float | TypeCategory::Decimal => Align::Right,
        _ => Align::Left,
    }
}

/// What ends up on the clipboard for one cell: the full value, not the shortened display.
/// `NULL` is the word NULL; binary is `0x` hex. A truncated value copies the part we have.
pub fn copy_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => float_text(*f),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => s.clone(),
        Value::Bytes(b) => hex(b),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(copy_text).collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Truncated(t) => match t.kind {
            LargeKind::Bytes => hex(&t.preview),
            _ => String::from_utf8_lossy(&t.preview).into_owned(),
        },
        Value::Other { text, .. } => text.clone(),
        _ => String::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Tab-separated text that pastes into a spreadsheet as a grid. Cells containing a tab, newline
/// or quote are quoted (quotes doubled), as Excel and Google Sheets expect.
pub fn to_tsv(rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|cell| {
                    if cell.contains(['\t', '\n', '\r', '"']) {
                        format!("\"{}\"", cell.replace('"', "\"\""))
                    } else {
                        cell.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `45 KB`, `1.2 MB`: one decimal below 10, none above.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `14200` -> `14,200`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use fg_core::Truncated;

    use super::*;

    fn d(v: Value) -> (CellKind, String) {
        let c = display(&v);
        (c.kind, c.text)
    }

    #[test]
    fn null_is_never_the_string_null() {
        assert_eq!(d(Value::Null), (CellKind::Null, "NULL".into()));
        // The text "NULL" is an ordinary string and must stay distinguishable.
        assert_eq!(
            d(Value::Text("NULL".into())),
            (CellKind::Text, "NULL".into())
        );
        assert_ne!(
            display(&Value::Null).kind,
            display(&Value::Text("NULL".into())).kind
        );
    }

    #[test]
    fn empty_string_is_blank_not_null() {
        assert_eq!(
            d(Value::Text(String::new())),
            (CellKind::Text, String::new())
        );
    }

    #[test]
    fn numbers_ids_and_dates_are_monospace() {
        assert_eq!(d(Value::Int(-5)).0, CellKind::Number);
        assert_eq!(
            d(Value::Decimal("12.50".into())),
            (CellKind::Number, "12.50".into())
        );
        assert_eq!(d(Value::Uuid("u".into())).0, CellKind::Mono);
        assert_eq!(
            d(Value::Timestamp("2026-01-01T00:00:00".into())).0,
            CellKind::Mono
        );
        assert_eq!(d(Value::Float(f64::NAN)).1, "NaN");
        assert_eq!(d(Value::Float(f64::NEG_INFINITY)).1, "-Infinity");
        assert_eq!(d(Value::Float(1.5)).1, "1.5");
    }

    #[test]
    fn text_stays_on_one_line_and_is_capped() {
        assert_eq!(d(Value::Text("a\nb\r\nc\td".into())).1, "a↵b↵↵c d");
        let long = "x".repeat(1000);
        let shown = d(Value::Text(long)).1;
        assert_eq!(shown.chars().count(), 301);
        assert!(shown.ends_with('…'));
        assert_eq!(d(Value::Text("a\u{7}b".into())).1, "a·b");
    }

    #[test]
    fn binary_and_big_json_become_pills() {
        assert_eq!(
            d(Value::Bytes(vec![0; 45 * 1024])),
            (CellKind::Pill, "[BLOB 45 KB]".into())
        );
        assert_eq!(d(Value::Bytes(vec![])).1, "[BLOB 0 B]");
        let big = format!(
            "{{{}}}",
            (0..40)
                .map(|i| format!("\"key{i}\": {i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(
            d(Value::Json(big)),
            (CellKind::Pill, "[JSON 40 keys]".into())
        );
        assert_eq!(
            d(Value::Json("{\"a\": 1}".into())),
            (CellKind::Mono, "{\"a\": 1}".into())
        );
        let arr = format!("[{}]", vec!["12345"; 40].join(","));
        assert_eq!(d(Value::Json(arr)).1, "[JSON 40 items]");
    }

    #[test]
    fn truncated_values_show_their_size() {
        let text = Value::Truncated(Truncated {
            kind: LargeKind::Text,
            total_len: 120 * 1024,
            preview: b"hello\nworld".to_vec(),
        });
        assert_eq!(d(text), (CellKind::Text, "hello↵world… (120 KB)".into()));
        let bytes = Value::Truncated(Truncated {
            kind: LargeKind::Bytes,
            total_len: 3 * 1024 * 1024,
            preview: vec![1],
        });
        assert_eq!(d(bytes), (CellKind::Pill, "[BLOB 3.0 MB]".into()));
    }

    #[test]
    fn arrays_and_unknown_types() {
        assert_eq!(
            d(Value::Array(vec![
                Value::Int(1),
                Value::Text("a".into()),
                Value::Null
            ]))
            .1,
            "{1, a, NULL}"
        );
        assert_eq!(
            d(Value::Other {
                type_name: "point".into(),
                text: "(1,2)".into()
            })
            .1,
            "(1,2)"
        );
    }

    #[test]
    fn copying_gives_the_full_value() {
        assert_eq!(copy_text(&Value::Null), "NULL");
        assert_eq!(copy_text(&Value::Text("a\nb".into())), "a\nb");
        assert_eq!(copy_text(&Value::Bytes(vec![0xde, 0xad])), "0xdead");
        assert_eq!(copy_text(&Value::Bool(true)), "true");
    }

    #[test]
    fn tsv_quotes_what_spreadsheets_would_split() {
        let rows = vec![
            vec!["id".to_string(), "note".to_string()],
            vec!["1".to_string(), "has\ttab".to_string()],
            vec!["2".to_string(), "line1\nline2".to_string()],
            vec!["3".to_string(), "say \"hi\"".to_string()],
        ];
        assert_eq!(
            to_tsv(&rows),
            "id\tnote\n1\t\"has\ttab\"\n2\t\"line1\nline2\"\n3\t\"say \"\"hi\"\"\""
        );
        assert_eq!(to_tsv(&[]), "");
    }

    #[test]
    fn size_and_number_formatting() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(45 * 1024), "45 KB");
        assert_eq!(human_bytes(1_500_000), "1.4 MB");
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(14_200), "14,200");
        assert_eq!(group_digits(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn numeric_columns_align_right() {
        assert_eq!(align_for(TypeCategory::Integer), Align::Right);
        assert_eq!(align_for(TypeCategory::Decimal), Align::Right);
        assert_eq!(align_for(TypeCategory::Text), Align::Left);
        assert_eq!(align_for(TypeCategory::Other), Align::Left);
    }
}
