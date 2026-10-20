//! The staged-changes model behind inline editing (see roadmap M3). Lives in `fg-core` so the
//! UI, `fg-sql` (DML compiler) and `fg-service` (commit) share one definition.

use serde::{Deserialize, Serialize};

use crate::{ObjectPath, Value};

/// A value being written to a cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CellValue {
    Value(Value),
    /// Use the column's default (`Set Default`; omitted from INSERT or `DEFAULT` in UPDATE).
    Default,
}

/// Identity of an existing row: the primary-key columns and their values.
/// Tables without a primary key cannot produce a `RowKey` and are therefore read-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowKey(pub Vec<(String, Value)>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RowChange {
    Insert {
        values: Vec<(String, CellValue)>,
    },
    Update {
        key: RowKey,
        set: Vec<(String, CellValue)>,
        /// Values as loaded, for the changed columns. When present the commit also checks them in
        /// the `WHERE` clause (optimistic concurrency): if someone else changed the row first, the
        /// update affects 0 rows and the commit fails instead of overwriting.
        before: Vec<(String, Value)>,
    },
    Delete {
        key: RowKey,
    },
}

/// All staged changes for one table; committed atomically in a single transaction.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Changeset {
    pub target: Option<ObjectPath>,
    pub changes: Vec<RowChange>,
}

impl Changeset {
    pub fn new(target: ObjectPath) -> Self {
        Self {
            target: Some(target),
            changes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// (inserts, updates, deletes) for the title-bar capsule.
    pub fn counts(&self) -> (usize, usize, usize) {
        self.changes.iter().fold((0, 0, 0), |(i, u, d), c| match c {
            RowChange::Insert { .. } => (i + 1, u, d),
            RowChange::Update { .. } => (i, u + 1, d),
            RowChange::Delete { .. } => (i, u, d + 1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        let key = RowKey(vec![("id".into(), Value::Int(1))]);
        let mut cs = Changeset::new(ObjectPath::new(None, "t"));
        cs.changes.push(RowChange::Insert { values: vec![] });
        cs.changes.push(RowChange::Delete { key: key.clone() });
        cs.changes.push(RowChange::Delete { key });
        assert_eq!(cs.counts(), (1, 0, 2));
        assert!(!cs.is_empty());
    }
}
