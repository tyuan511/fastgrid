//! Following a foreign key: which columns of a table can be clicked through, and to where.
//!
//! Only single-column keys are offered for the forward jump. A composite key needs several
//! values to find the referenced row; that comes with the reverse-reference work in M6.

use fg_core::{ObjectPath, Relation, TableDetail};

/// A column whose value identifies a row of another table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpTarget {
    /// The column in this table holding the key.
    pub column: String,
    pub table: ObjectPath,
    /// The column in `table` it refers to.
    pub referenced_column: String,
}

/// The jumps available from `detail`. A key that names no target column refers to the target's
/// primary key; `primary_key_of` supplies it (and returns `None` when it is not a single column,
/// in which case the key is left out).
pub fn jump_targets(
    detail: &TableDetail,
    primary_key_of: impl Fn(&ObjectPath) -> Option<String>,
) -> Vec<JumpTarget> {
    detail
        .foreign_keys
        .iter()
        .filter_map(|fk| {
            let [column] = fk.columns.as_slice() else {
                return None;
            };
            let referenced_column = match fk.referenced_columns.as_slice() {
                [one] => one.clone(),
                [] => primary_key_of(&fk.referenced)?,
                _ => return None,
            };
            Some(JumpTarget {
                column: column.clone(),
                table: fk.referenced.clone(),
                referenced_column,
            })
        })
        .collect()
}

/// A table that references another: the reverse of [`JumpTarget`]. `referenced_columns`
/// is empty when the key names no column, i.e. it refers to the target's primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackRef {
    pub from: ObjectPath,
    pub from_columns: Vec<String>,
    pub referenced_columns: Vec<String>,
}

/// Every table in `relations` that references `table`: the "referenced by"
/// list for a primary-key cell. Composite keys stay whole — the caller
/// turns every column pair into a filter, so no single-column restriction
/// applies here.
pub fn back_references(relations: &[Relation], table: &ObjectPath) -> Vec<BackRef> {
    relations
        .iter()
        .filter(|r| r.key.referenced == *table)
        .map(|r| BackRef {
            from: r.from.clone(),
            from_columns: r.key.columns.clone(),
            referenced_columns: r.key.referenced_columns.clone(),
        })
        .collect()
}

/// Tables whose primary key is needed to resolve `detail`'s keys (so they can be looked up first).
pub fn tables_needing_primary_key(detail: &TableDetail) -> Vec<ObjectPath> {
    detail
        .foreign_keys
        .iter()
        .filter(|fk| fk.columns.len() == 1 && fk.referenced_columns.is_empty())
        .map(|fk| fk.referenced.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use fg_core::{ForeignKeyInfo, ObjectKind};

    use super::*;

    fn detail(fks: Vec<ForeignKeyInfo>) -> TableDetail {
        TableDetail {
            path: ObjectPath::new(Some("main"), "books"),
            kind: ObjectKind::Table,
            comment: None,
            columns: vec![],
            indexes: vec![],
            foreign_keys: fks,
            checks: vec![],
            triggers: vec![],
            row_estimate: None,
            size_bytes: None,
            options: BTreeMap::new(),
        }
    }

    fn fk(cols: &[&str], target: &str, target_cols: &[&str]) -> ForeignKeyInfo {
        ForeignKeyInfo {
            name: None,
            columns: cols.iter().map(|c| (*c).into()).collect(),
            referenced: ObjectPath::new(Some("main"), target),
            referenced_columns: target_cols.iter().map(|c| (*c).into()).collect(),
            on_update: None,
            on_delete: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn explicit_and_implicit_targets() {
        let d = detail(vec![
            fk(&["author_id"], "authors", &["id"]),
            fk(&["publisher_id"], "publishers", &[]),
        ]);
        let targets = jump_targets(&d, |p| (p.name == "publishers").then(|| "pid".to_string()));
        assert_eq!(
            targets,
            vec![
                JumpTarget {
                    column: "author_id".into(),
                    table: ObjectPath::new(Some("main"), "authors"),
                    referenced_column: "id".into()
                },
                JumpTarget {
                    column: "publisher_id".into(),
                    table: ObjectPath::new(Some("main"), "publishers"),
                    referenced_column: "pid".into()
                },
            ]
        );
    }

    #[test]
    fn composite_keys_and_unresolvable_targets_are_left_out() {
        let d = detail(vec![
            fk(&["a", "b"], "pairs", &["x", "y"]),
            fk(&["c"], "no_single_pk", &[]),
        ]);
        assert!(jump_targets(&d, |_| None).is_empty());
    }

    #[test]
    fn back_references_keep_composite_keys_whole() {
        use fg_core::Relation;
        let relations = vec![
            // References to other tables are not back-references of books.
            Relation {
                from: ObjectPath::new(Some("main"), "books"),
                key: fk(&["author_id"], "authors", &["id"]),
            },
            Relation {
                from: ObjectPath::new(Some("main"), "reviews"),
                key: fk(&["book_id"], "books", &[]),
            },
            Relation {
                from: ObjectPath::new(Some("main"), "copies"),
                key: fk(&["book_a", "book_b"], "books", &["id", "seq"]),
            },
        ];
        let refs = back_references(&relations, &ObjectPath::new(Some("main"), "books"));
        assert_eq!(
            refs,
            vec![
                BackRef {
                    from: ObjectPath::new(Some("main"), "reviews"),
                    from_columns: vec!["book_id".into()],
                    referenced_columns: vec![],
                },
                BackRef {
                    from: ObjectPath::new(Some("main"), "copies"),
                    from_columns: vec!["book_a".into(), "book_b".into()],
                    referenced_columns: vec!["id".into(), "seq".into()],
                },
            ]
        );
        // A table nobody references has no back-references.
        assert!(back_references(&relations, &ObjectPath::new(Some("main"), "nobody")).is_empty());
    }

    #[test]
    fn lists_the_tables_whose_primary_key_is_needed() {
        let d = detail(vec![
            fk(&["a"], "explicit", &["id"]),
            fk(&["b"], "implicit", &[]),
            fk(&["c", "d"], "composite", &[]),
        ]);
        let names: Vec<_> = tables_needing_primary_key(&d)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["implicit"]);
    }
}
