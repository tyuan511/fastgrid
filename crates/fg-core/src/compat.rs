//! Forward-compatibility helpers for persisted JSON.
//!
//! Evolution rules for every persisted struct/enum:
//! 1. Only **add** fields, always with `#[serde(default)]`. Never rename or remove; deprecate.
//! 2. Never use `deny_unknown_fields` (a newer version may have written extra fields).
//! 3. Unit enums carry an `Unknown` variant via `#[serde(other)]`, so a value written by a newer
//!    version degrades instead of failing the whole record.
//! 4. Enums with data cannot do (3); lists of them use [`lenient_vec`] so one unreadable element
//!    (from a newer version) is skipped instead of losing the list.
//! 5. A change that cannot follow 1-4 needs a store migration (see `docs/data-model.md`).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// Deserialize a list, silently dropping elements that fail to parse.
pub fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::lenient_vec;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    enum Kind {
        A { x: i32 },
        B,
    }

    #[derive(Debug, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "lenient_vec")]
        items: Vec<Kind>,
    }

    #[test]
    fn unknown_elements_are_skipped() {
        let h: Holder = serde_json::from_str(
            r#"{"items":[{"type":"A","x":1},{"type":"FromTheFuture","y":2},{"type":"B"}]}"#,
        )
        .unwrap_or(Holder { items: vec![] });
        assert_eq!(h.items, vec![Kind::A { x: 1 }, Kind::B]);
    }
}
