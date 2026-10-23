//! Staged edits to one table's rows, kept in memory until the user commits (roadmap M3).
//!
//! The loaded page is the immutable baseline; this overlay records what differs from it:
//! modified cells, new rows and rows marked for deletion. Nothing here touches the database.
//! [`Changeset::changes`] reduces the overlay to [`RowChange`]s for `fg-sql` to compile.
//!
//! Existing rows are identified by their primary-key values as loaded, so the overlay survives
//! paging and re-sorting. A table without a primary key is read-only: every edit is refused with
//! the reason, because there is no way to say which row was meant.
//!
//! Undo/redo is snapshot based. The state is a handful of small maps, so cloning it per edit is
//! cheaper than getting an inverse-operation log right, and it cannot drift out of sync.

use std::collections::BTreeMap;

use fg_core::Value;
use fg_sql::{CellChange, RowChange};

/// Undo depth. Older steps are dropped, never the current state.
const MAX_UNDO: usize = 500;

/// Which row an edit is about.
#[derive(Debug, Clone, PartialEq)]
pub enum RowId {
    /// A row from the loaded data, by its primary-key values (in primary-key order).
    Existing(Vec<Value>),
    /// A row added in this changeset.
    New(u64),
}

/// How a cell differs from the baseline.
#[derive(Debug, Clone, PartialEq)]
pub enum CellState {
    Unchanged,
    Modified(CellChange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Unchanged,
    Modified,
    Inserted,
    Deleted,
}

#[derive(Debug, Clone, PartialEq)]
struct Modified {
    key: Vec<Value>,
    cells: BTreeMap<usize, CellChange>,
}

#[derive(Debug, Clone, PartialEq)]
struct Inserted {
    id: u64,
    cells: BTreeMap<usize, CellChange>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct State {
    modified: Vec<Modified>,
    inserted: Vec<Inserted>,
    deleted: Vec<Vec<Value>>,
    next_id: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub modified_rows: usize,
    pub inserted_rows: usize,
    pub deleted_rows: usize,
}

impl Counts {
    pub fn total(self) -> usize {
        self.modified_rows + self.inserted_rows + self.deleted_rows
    }
}

#[derive(Debug, Clone)]
pub struct Changeset {
    columns: Vec<String>,
    /// Indices into `columns`, in primary-key order. Empty = read-only.
    key_columns: Vec<usize>,
    state: State,
    undo: Vec<State>,
    redo: Vec<State>,
}

impl Changeset {
    /// `key_columns` are the primary-key column names in key order (empty for none).
    pub fn new(columns: Vec<String>, key_columns: &[&str]) -> Self {
        // A key column missing from the loaded columns cannot be used to locate rows, so the
        // table is treated as having no usable key.
        let key_columns = key_columns
            .iter()
            .map(|k| columns.iter().position(|c| c == k))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        Self {
            columns,
            key_columns,
            state: State::default(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn key_column_indices(&self) -> &[usize] {
        &self.key_columns
    }

    /// New rows in the order they were added (which is the order they are shown and inserted).
    pub fn new_row_ids(&self) -> Vec<u64> {
        self.state.inserted.iter().map(|r| r.id).collect()
    }

    /// Why this table cannot be edited, if it cannot.
    pub fn read_only_reason(&self) -> Option<&'static str> {
        self.key_columns
            .is_empty()
            .then_some("This table has no primary key, so its rows cannot be edited.")
    }

    /// The id of a loaded row, from its values.
    pub fn row_id(&self, row: &[Value]) -> Option<RowId> {
        if self.key_columns.is_empty() {
            return None;
        }
        self.key_columns
            .iter()
            .map(|&i| row.get(i).cloned())
            .collect::<Option<Vec<_>>>()
            .map(RowId::Existing)
    }

    pub fn is_empty(&self) -> bool {
        self.counts().total() == 0
    }

    pub fn counts(&self) -> Counts {
        Counts {
            modified_rows: self
                .state
                .modified
                .iter()
                .filter(|m| !self.state.deleted.contains(&m.key))
                .count(),
            inserted_rows: self.state.inserted.len(),
            deleted_rows: self.state.deleted.len(),
        }
    }

    pub fn row_state(&self, id: &RowId) -> RowState {
        match id {
            RowId::New(n) => {
                if self.state.inserted.iter().any(|r| r.id == *n) {
                    RowState::Inserted
                } else {
                    RowState::Unchanged
                }
            }
            RowId::Existing(key) => {
                if self.state.deleted.contains(key) {
                    RowState::Deleted
                } else if self.state.modified.iter().any(|m| &m.key == key) {
                    RowState::Modified
                } else {
                    RowState::Unchanged
                }
            }
        }
    }

    pub fn cell_state(&self, id: &RowId, column: usize) -> CellState {
        let cells = match id {
            RowId::New(n) => self
                .state
                .inserted
                .iter()
                .find(|r| r.id == *n)
                .map(|r| &r.cells),
            RowId::Existing(key) => self
                .state
                .modified
                .iter()
                .find(|m| &m.key == key)
                .map(|m| &m.cells),
        };
        match cells.and_then(|c| c.get(&column)) {
            Some(edit) => CellState::Modified(edit.clone()),
            None => CellState::Unchanged,
        }
    }

    /// Edit one cell. `baseline` is the row as loaded (ignored for new rows). Putting the
    /// original value back removes the modification, and a row with none left stops being
    /// modified.
    pub fn edit_cell(
        &mut self,
        id: &RowId,
        baseline: &[Value],
        column: usize,
        edit: CellChange,
    ) -> Result<(), String> {
        if column >= self.columns.len() {
            return Err(format!("no column {column}"));
        }
        self.check_editable()?;
        if let RowId::Existing(key) = id
            && self.state.deleted.contains(key)
        {
            return Err("This row is marked for deletion; restore it first.".into());
        }
        match id {
            RowId::New(n) => {
                if !self.state.inserted.iter().any(|r| r.id == *n) {
                    return Err("no such new row".into());
                }
                self.checkpoint();
                if let Some(row) = self.state.inserted.iter_mut().find(|r| r.id == *n) {
                    row.cells.insert(column, edit);
                }
            }
            RowId::Existing(key) => {
                let same_as_baseline =
                    matches!(&edit, CellChange::Set(v) if baseline.get(column) == Some(v));
                let pos = self.state.modified.iter().position(|m| &m.key == key);
                if pos.is_none() && same_as_baseline {
                    return Ok(());
                }
                self.checkpoint();
                match (pos, same_as_baseline) {
                    (Some(p), true) => {
                        self.state.modified[p].cells.remove(&column);
                        if self.state.modified[p].cells.is_empty() {
                            self.state.modified.remove(p);
                        }
                    }
                    (None, true) => {}
                    (Some(p), false) => {
                        self.state.modified[p].cells.insert(column, edit);
                    }
                    (None, false) => self.state.modified.push(Modified {
                        key: key.clone(),
                        cells: BTreeMap::from([(column, edit)]),
                    }),
                }
            }
        }
        Ok(())
    }

    /// Put one cell back to its baseline value.
    pub fn revert_cell(&mut self, id: &RowId, column: usize) {
        if self.cell_state(id, column) == CellState::Unchanged {
            return;
        }
        self.checkpoint();
        match id {
            RowId::New(n) => {
                if let Some(row) = self.state.inserted.iter_mut().find(|r| r.id == *n) {
                    row.cells.remove(&column);
                }
            }
            RowId::Existing(key) => {
                if let Some(p) = self.state.modified.iter().position(|m| &m.key == key) {
                    self.state.modified[p].cells.remove(&column);
                    if self.state.modified[p].cells.is_empty() {
                        self.state.modified.remove(p);
                    }
                }
            }
        }
    }

    /// Add an empty row; every column takes its default unless edited.
    pub fn insert_row(&mut self) -> Result<RowId, String> {
        self.push_new_row(BTreeMap::new())
    }

    fn push_new_row(&mut self, cells: BTreeMap<usize, CellChange>) -> Result<RowId, String> {
        self.check_editable()?;
        self.checkpoint();
        let id = self.state.next_id;
        self.state.next_id += 1;
        self.state.inserted.push(Inserted { id, cells });
        Ok(RowId::New(id))
    }

    /// Add a row that starts as a copy of `row`. `clear` lists columns that must not be copied
    /// (auto-increment keys); they take their default.
    pub fn clone_row(&mut self, row: &[Value], clear: &[usize]) -> Result<RowId, String> {
        let cells = row
            .iter()
            .enumerate()
            .take(self.columns.len())
            .filter(|(i, _)| !clear.contains(i))
            .map(|(i, v)| (i, CellChange::Set(v.clone())))
            .collect();
        self.push_new_row(cells)
    }

    /// Mark a loaded row for deletion, or drop a new row outright.
    pub fn delete_row(&mut self, id: &RowId) -> Result<(), String> {
        self.check_editable()?;
        match id {
            RowId::New(n) => {
                if !self.state.inserted.iter().any(|r| r.id == *n) {
                    return Err("no such new row".into());
                }
                self.checkpoint();
                self.state.inserted.retain(|r| r.id != *n);
            }
            RowId::Existing(key) => {
                if self.state.deleted.contains(key) {
                    return Ok(());
                }
                self.checkpoint();
                self.state.deleted.push(key.clone());
            }
        }
        Ok(())
    }

    /// Undo a deletion mark. Edits made before the mark are still there.
    pub fn restore_row(&mut self, id: &RowId) {
        if let RowId::Existing(key) = id
            && self.state.deleted.contains(key)
        {
            self.checkpoint();
            self.state.deleted.retain(|k| k != key);
        }
    }

    /// Throw every staged change away. Undoable, like any other step.
    pub fn discard_all(&mut self) {
        if self.is_empty() {
            return;
        }
        self.checkpoint();
        let next_id = self.state.next_id;
        self.state = State {
            next_id,
            ..State::default()
        };
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
                self.redo.push(std::mem::replace(&mut self.state, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut self.state, next));
                true
            }
            None => false,
        }
    }

    /// The overlay as changes for `fg-sql`: deletes first (so a new row may reuse a deleted
    /// key), then updates, then inserts. A deleted row's pending edits are not sent.
    pub fn changes(&self) -> Vec<RowChange> {
        let key_names: Vec<String> = self
            .key_columns
            .iter()
            .map(|&i| self.columns[i].clone())
            .collect();
        let keyed = |key: &[Value]| -> Vec<(String, Value)> {
            key_names.iter().cloned().zip(key.iter().cloned()).collect()
        };
        let named = |cells: &BTreeMap<usize, CellChange>| -> Vec<(String, CellChange)> {
            cells
                .iter()
                .map(|(&i, c)| (self.columns[i].clone(), c.clone()))
                .collect()
        };
        let mut out = Vec::new();
        for key in &self.state.deleted {
            out.push(RowChange::Delete { key: keyed(key) });
        }
        for m in &self.state.modified {
            if !self.state.deleted.contains(&m.key) {
                out.push(RowChange::Update {
                    key: keyed(&m.key),
                    cells: named(&m.cells),
                });
            }
        }
        for row in &self.state.inserted {
            out.push(RowChange::Insert {
                cells: named(&row.cells),
            });
        }
        out
    }

    fn check_editable(&self) -> Result<(), String> {
        match self.read_only_reason() {
            Some(reason) => Err(reason.into()),
            None => Ok(()),
        }
    }

    fn checkpoint(&mut self) {
        self.undo.push(self.state.clone());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn cs() -> Changeset {
        Changeset::new(vec!["id".into(), "name".into(), "n".into()], &["id"])
    }

    fn row(n: i64) -> Vec<Value> {
        vec![int(n), text(&format!("r{n}")), int(n * 10)]
    }

    fn id(n: i64) -> RowId {
        cs().row_id(&row(n)).unwrap()
    }

    fn set(v: Value) -> CellChange {
        CellChange::Set(v)
    }

    #[test]
    fn an_edit_is_tracked_per_cell_and_per_row() {
        let mut c = cs();
        assert!(c.is_empty());
        c.edit_cell(&id(1), &row(1), 1, set(text("x"))).unwrap();
        assert_eq!(c.row_state(&id(1)), RowState::Modified);
        assert_eq!(c.row_state(&id(2)), RowState::Unchanged);
        assert_eq!(c.cell_state(&id(1), 1), CellState::Modified(set(text("x"))));
        assert_eq!(c.cell_state(&id(1), 2), CellState::Unchanged);
        assert_eq!(c.counts().modified_rows, 1);
    }

    #[test]
    fn putting_the_original_value_back_clears_the_edit() {
        let mut c = cs();
        c.edit_cell(&id(1), &row(1), 1, set(text("x"))).unwrap();
        c.edit_cell(&id(1), &row(1), 1, set(text("r1"))).unwrap();
        assert!(c.is_empty(), "no phantom modification is left");
        // Editing an untouched row to its own value stages nothing and leaves no undo step.
        let mut c = cs();
        c.edit_cell(&id(1), &row(1), 1, set(text("r1"))).unwrap();
        assert!(c.is_empty() && !c.can_undo());
    }

    #[test]
    fn changes_are_ordered_delete_update_insert() {
        let mut c = cs();
        let new = c.insert_row().unwrap();
        c.edit_cell(&new, &[], 0, set(int(2))).unwrap();
        c.edit_cell(&new, &[], 1, set(text("reuses key 2")))
            .unwrap();
        c.edit_cell(&id(1), &row(1), 2, set(int(99))).unwrap();
        c.delete_row(&id(2)).unwrap();

        let changes = c.changes();
        assert_eq!(changes.len(), 3);
        assert!(matches!(changes[0], RowChange::Delete { .. }));
        assert!(matches!(changes[1], RowChange::Update { .. }));
        assert!(matches!(changes[2], RowChange::Insert { .. }));
        assert_eq!(
            changes[1],
            RowChange::Update {
                key: vec![("id".into(), int(1))],
                cells: vec![("n".into(), set(int(99)))],
            }
        );
    }

    #[test]
    fn a_deleted_rows_edits_are_kept_but_not_sent() {
        let mut c = cs();
        c.edit_cell(&id(1), &row(1), 1, set(text("x"))).unwrap();
        c.delete_row(&id(1)).unwrap();
        assert_eq!(c.changes().len(), 1);
        assert!(matches!(c.changes()[0], RowChange::Delete { .. }));
        assert_eq!(c.counts().modified_rows, 0, "not counted twice");
        assert!(
            c.edit_cell(&id(1), &row(1), 1, set(text("y"))).is_err(),
            "no editing a row that is going away"
        );

        c.restore_row(&id(1));
        assert_eq!(c.row_state(&id(1)), RowState::Modified);
        assert!(matches!(c.changes()[0], RowChange::Update { .. }));
    }

    #[test]
    fn deleting_a_new_row_just_removes_it() {
        let mut c = cs();
        let new = c.insert_row().unwrap();
        assert_eq!(c.counts().inserted_rows, 1);
        c.delete_row(&new).unwrap();
        assert!(c.is_empty() && c.changes().is_empty());
    }

    #[test]
    fn a_clone_copies_everything_except_the_cleared_columns() {
        let mut c = cs();
        let copy = c.clone_row(&row(3), &[0]).unwrap();
        assert_eq!(c.cell_state(&copy, 0), CellState::Unchanged);
        assert_eq!(c.cell_state(&copy, 1), CellState::Modified(set(text("r3"))));
        assert_eq!(
            c.changes(),
            [RowChange::Insert {
                cells: vec![("name".into(), set(text("r3"))), ("n".into(), set(int(30)))]
            }]
        );
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut c = cs();
        c.edit_cell(&id(1), &row(1), 1, set(text("a"))).unwrap();
        c.delete_row(&id(2)).unwrap();
        c.insert_row().unwrap();
        assert_eq!(c.counts().total(), 3);

        assert!(c.undo());
        assert_eq!(c.counts().inserted_rows, 0);
        assert!(c.undo());
        assert_eq!(c.counts().deleted_rows, 0);
        assert!(c.redo());
        assert_eq!(c.counts().deleted_rows, 1);

        // A new edit forks the history: the old redo is gone.
        c.edit_cell(&id(3), &row(3), 1, set(text("z"))).unwrap();
        assert!(!c.can_redo());

        while c.undo() {}
        assert!(c.is_empty());
    }

    #[test]
    fn discard_all_is_one_undoable_step() {
        let mut c = cs();
        c.edit_cell(&id(1), &row(1), 1, set(text("a"))).unwrap();
        c.delete_row(&id(2)).unwrap();
        c.discard_all();
        assert!(c.is_empty());
        assert!(c.undo());
        assert_eq!(c.counts().total(), 2);
    }

    #[test]
    fn revert_cell_restores_the_baseline_for_that_cell_only() {
        let mut c = cs();
        c.edit_cell(&id(1), &row(1), 1, set(text("a"))).unwrap();
        c.edit_cell(&id(1), &row(1), 2, set(int(5))).unwrap();
        c.revert_cell(&id(1), 1);
        assert_eq!(c.cell_state(&id(1), 1), CellState::Unchanged);
        assert_eq!(c.cell_state(&id(1), 2), CellState::Modified(set(int(5))));
        c.revert_cell(&id(1), 2);
        assert!(c.is_empty());
    }

    #[test]
    fn a_table_without_a_usable_key_is_read_only() {
        for key in [&[][..], &["missing"][..]] {
            let mut c = Changeset::new(vec!["a".into(), "b".into()], key);
            assert!(c.read_only_reason().is_some());
            assert!(c.row_id(&[int(1), int(2)]).is_none());
            assert!(c.insert_row().is_err());
            assert!(
                c.edit_cell(
                    &RowId::Existing(vec![int(1)]),
                    &[int(1), int(2)],
                    1,
                    set(int(3))
                )
                .is_err()
            );
            assert!(c.delete_row(&RowId::Existing(vec![int(1)])).is_err());
            assert!(c.is_empty());
        }
    }

    #[test]
    fn composite_keys_identify_a_row_in_key_order() {
        let mut c = Changeset::new(vec!["a".into(), "b".into(), "v".into()], &["b", "a"]);
        let r = [int(1), int(2), text("x")];
        let rid = c.row_id(&r).unwrap();
        assert_eq!(rid, RowId::Existing(vec![int(2), int(1)]));
        c.delete_row(&rid).unwrap();
        assert_eq!(
            c.changes(),
            [RowChange::Delete {
                key: vec![("b".into(), int(2)), ("a".into(), int(1))]
            }]
        );
    }

    #[test]
    fn undo_depth_is_bounded() {
        let mut c = cs();
        for i in 0..(MAX_UNDO as i64 + 50) {
            c.edit_cell(&id(1), &row(1), 2, set(int(1000 + i))).unwrap();
        }
        let mut steps = 0;
        while c.undo() {
            steps += 1;
        }
        assert_eq!(steps, MAX_UNDO);
    }
}
