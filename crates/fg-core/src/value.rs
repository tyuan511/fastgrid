use serde::{Deserialize, Serialize};

/// A typed cell value, the common currency between drivers, the grid, edits, export and MCP.
///
/// Design rules:
/// * Lossless: numbers that may not fit `f64`/`i64` travel as text (`Decimal`); exotic engine
///   types that have no variant travel as [`Value::Other`] and stay read-only in the UI.
/// * Temporal values are ISO-8601 text tagged by kind, so writing them back needs no guessing.
/// * Oversized cells arrive as [`Value::Truncated`] (see `ExecOptions::max_cell_bytes`) and are
///   loaded in full on demand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Arbitrary-precision number as text.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    /// JSON document as text (formatting and key order preserved).
    Json(String),
    Uuid(String),
    /// `2026-10-20`
    Date(String),
    /// `12:30:00` or `12:30:00.123456`
    Time(String),
    /// `2026-10-20T12:30:00` (no zone)
    Timestamp(String),
    /// `2026-10-20T12:30:00+08:00`
    TimestampTz(String),
    Interval(String),
    Array(Vec<Value>),
    /// A large cell cut short to keep rows light.
    Truncated(Truncated),
    /// Anything else, as the engine's textual form plus its native type name.
    Other {
        type_name: String,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LargeKind {
    Text,
    Bytes,
    Json,
}

/// Head of a large value plus its full size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Truncated {
    pub kind: LargeKind,
    /// Full size in bytes.
    pub total_len: u64,
    /// First bytes of the value; for text/JSON cut on a UTF-8 boundary.
    pub preview: Vec<u8>,
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Replace a text/bytes/JSON value larger than `max_bytes` by a [`Value::Truncated`].
    pub fn truncated_if_larger(self, max_bytes: usize) -> Value {
        fn cut(bytes: &[u8], text: bool, max: usize) -> Vec<u8> {
            let mut end = max.min(bytes.len());
            if text {
                while end > 0 && end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
                    end -= 1;
                }
            }
            bytes[..end].to_vec()
        }
        match self {
            Value::Text(s) if s.len() > max_bytes => Value::Truncated(Truncated {
                kind: LargeKind::Text,
                total_len: s.len() as u64,
                preview: cut(s.as_bytes(), true, max_bytes),
            }),
            Value::Json(s) if s.len() > max_bytes => Value::Truncated(Truncated {
                kind: LargeKind::Json,
                total_len: s.len() as u64,
                preview: cut(s.as_bytes(), true, max_bytes),
            }),
            Value::Bytes(b) if b.len() > max_bytes => Value::Truncated(Truncated {
                kind: LargeKind::Bytes,
                total_len: b.len() as u64,
                preview: cut(&b, false, max_bytes),
            }),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_pass_through() {
        assert_eq!(
            Value::Text("hi".into()).truncated_if_larger(10),
            Value::Text("hi".into())
        );
        assert_eq!(Value::Int(5).truncated_if_larger(0), Value::Int(5));
    }

    #[test]
    fn text_is_cut_on_a_char_boundary() {
        // "é" is two bytes; cutting at 3 would split the second "é".
        let v = Value::Text("ééé".into()).truncated_if_larger(3);
        match v {
            Value::Truncated(t) => {
                assert_eq!(t.total_len, 6);
                assert_eq!(String::from_utf8(t.preview).unwrap(), "é");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bytes_keep_exact_prefix() {
        match Value::Bytes(vec![1, 2, 3, 4]).truncated_if_larger(2) {
            Value::Truncated(t) => assert_eq!(t.preview, vec![1, 2]),
            other => panic!("{other:?}"),
        }
    }
}
