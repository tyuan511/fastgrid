//! Staged schema changes (roadmap M6): the structure tab's counterpart of
//! [`Changeset`]. Where the row overlay is keyed by primary key, a schema
//! batch is an ordered op list: operations compose in order (a renamed
//! column can be altered later in the same batch), so the state *is* the
//! op list, snapshotted for undo exactly like the row overlay.

use fg_sql::SchemaChange;

const MAX_UNDO: usize = 500;

/// The structure tab's staged changes, in the order the user made them.
#[derive(Debug, Clone, Default)]
pub struct StructureChangeset {
    changes: Vec<SchemaChange>,
    undo: Vec<Vec<SchemaChange>>,
    redo: Vec<Vec<SchemaChange>>,
}

impl StructureChangeset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn changes(&self) -> &[SchemaChange] {
        &self.changes
    }

    pub fn push(&mut self, change: SchemaChange) {
        self.checkpoint();
        self.changes.push(change);
    }

    pub fn clear(&mut self) {
        if !self.changes.is_empty() {
            self.checkpoint();
            self.changes.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut self.changes, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut self.changes, next));
                true
            }
            None => false,
        }
    }

    fn checkpoint(&mut self) {
        self.undo.push(self.changes.clone());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fg_core::{ColumnInfo, DataType, TypeCategory};

    fn add_col(name: &str) -> SchemaChange {
        SchemaChange::AddColumn {
            column: ColumnInfo::new(name, 1, DataType::new("TEXT", TypeCategory::Text)),
        }
    }

    #[test]
    fn push_undo_redo_walk_the_history() {
        let mut staged = StructureChangeset::new();
        assert!(staged.is_empty());
        staged.push(add_col("a"));
        staged.push(add_col("b"));
        assert_eq!(staged.len(), 2);
        assert!(staged.undo());
        assert_eq!(staged.len(), 1);
        assert!(staged.undo());
        assert!(staged.is_empty());
        assert!(!staged.undo());
        assert!(staged.redo());
        assert_eq!(staged.len(), 1);
        assert!(staged.redo());
        assert_eq!(staged.len(), 2);
        // A new push after undo drops the redo branch.
        assert!(staged.undo());
        staged.push(add_col("c"));
        assert!(!staged.redo());
        assert_eq!(staged.len(), 2);
    }

    #[test]
    fn undo_depth_is_bounded() {
        let mut staged = StructureChangeset::new();
        for i in 0..(MAX_UNDO as i64 + 50) {
            staged.push(add_col(&format!("c{i}")));
        }
        let mut steps = 0;
        while staged.undo() {
            steps += 1;
        }
        assert_eq!(steps, MAX_UNDO);
    }
}
