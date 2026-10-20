//! Typed identifiers and timestamps.
//!
//! Rules (they keep persisted data migration-free):
//! * IDs are opaque UUIDv7 strings generated client-side. They never encode meaning and are
//!   never auto-increment integers, so records can be exported, merged and synced later.
//! * Timestamps are Unix milliseconds in UTC.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// A fresh, time-ordered ID.
            pub fn new() -> Self {
                Self(Uuid::now_v7().to_string())
            }

            /// Wrap an existing ID (loaded from storage, or a fixed value in tests).
            pub fn from_string(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

define_id!(ConnectionId);
define_id!(GroupId);
define_id!(SnippetId);
define_id!(DocumentId);
define_id!(TabId);
define_id!(McpClientId);
define_id!(HistoryId);
define_id!(AuditId);

/// Unix time in milliseconds, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(pub i64);

impl UnixMillis {
    pub fn now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Self(ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_time_ordered() {
        let a = ConnectionId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = ConnectionId::new();
        assert_ne!(a, b);
        assert!(a < b, "UUIDv7 sorts by creation time");
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = GroupId::from_string("g1");
        assert_eq!(serde_json::to_string(&id).unwrap_or_default(), "\"g1\"");
    }
}
