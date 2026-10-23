//! Compile staged row changes into `INSERT` / `UPDATE` / `DELETE` with bound parameters.
//!
//! This is the last step before a commit (roadmap M3): the grid's change overlay is reduced to
//! [`RowChange`]s, and each becomes one statement. Values and identifiers never enter the SQL
//! text; rows are located by primary key only, and every `UPDATE` / `DELETE` states that it must
//! touch exactly one row ([`DmlStatement::expect_rows`]) so the caller rolls back when a key
//! matched nothing or, worse, several rows.

use fg_core::{Dialect, Error, ObjectPath, Result, Value};

/// What to put in one cell.
#[derive(Debug, Clone, PartialEq)]
pub enum CellChange {
    Set(Value),
    /// Let the engine pick the column default. On insert the column is simply omitted; on update
    /// it needs `SET c = DEFAULT`, which not every dialect has.
    Default,
}

/// One staged change to one row.
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Insert {
        cells: Vec<(String, CellChange)>,
    },
    Update {
        /// Primary-key columns and the values the row had when it was loaded.
        key: Vec<(String, Value)>,
        cells: Vec<(String, CellChange)>,
    },
    Delete {
        key: Vec<(String, Value)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DmlStatement {
    pub sql: String,
    pub params: Vec<Value>,
    /// The same statement with the values written in, for showing a person what will run. Never
    /// executed: the parameterised `sql` is what goes to the engine.
    pub preview: String,
    /// Rows this statement must affect; `None` when any count is acceptable (inserts).
    pub expect_rows: Option<u64>,
}

/// Compile in order. One bad change fails the whole batch: a partial script is worse than none.
pub fn compile_changes(
    path: &ObjectPath,
    changes: &[RowChange],
    dialect: &dyn Dialect,
) -> Result<Vec<DmlStatement>> {
    let table = dialect.qualified_name(path);
    changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            compile_one(&table, change, dialect).map_err(|e| match e {
                // Keep the variant: callers tell "unsupported here" from "bad input".
                Error::InvalidInput(m) => Error::InvalidInput(format!("change {}: {m}", i + 1)),
                Error::Unsupported(m) => Error::Unsupported(format!("change {}: {m}", i + 1)),
                other => other,
            })
        })
        .collect()
}

fn compile_one(table: &str, change: &RowChange, dialect: &dyn Dialect) -> Result<DmlStatement> {
    let mut params = Vec::new();
    let (sql, expect_rows) = build(table, change, dialect, &mut |v: &Value| {
        params.push(v.clone());
        dialect.placeholder(params.len())
    })?;
    let (preview, _) = build(table, change, dialect, &mut |v: &Value| literal(v))?;
    Ok(DmlStatement {
        sql,
        params,
        preview: format!("{preview};"),
        expect_rows,
    })
}

/// The statement text, with `bind` deciding what each value becomes (a placeholder, or for the
/// preview a literal). One builder keeps the two spellings from drifting apart.
fn build(
    table: &str,
    change: &RowChange,
    dialect: &dyn Dialect,
    bind: &mut dyn FnMut(&Value) -> String,
) -> Result<(String, Option<u64>)> {
    match change {
        RowChange::Insert { cells } => {
            check_unique(cells.iter().map(|(c, _)| c.as_str()))?;
            let set: Vec<_> = cells
                .iter()
                .filter_map(|(c, v)| match v {
                    CellChange::Set(v) => Some((c, v)),
                    CellChange::Default => None,
                })
                .collect();
            if set.is_empty() {
                return Ok((dialect.insert_defaults(table), None));
            }
            let names: Vec<_> = set.iter().map(|(c, _)| dialect.quote_ident(c)).collect();
            let marks: Vec<_> = set.iter().map(|(_, v)| bind(v)).collect();
            Ok((
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    names.join(", "),
                    marks.join(", ")
                ),
                None,
            ))
        }
        RowChange::Update { key, cells } => {
            if cells.is_empty() {
                return Err(Error::InvalidInput("an update sets no column".into()));
            }
            check_unique(cells.iter().map(|(c, _)| c.as_str()))?;
            let mut sets = Vec::new();
            for (column, change) in cells {
                let name = dialect.quote_ident(column);
                match change {
                    CellChange::Set(v) => sets.push(format!("{name} = {}", bind(v))),
                    CellChange::Default if dialect.supports_default_keyword() => {
                        sets.push(format!("{name} = DEFAULT"));
                    }
                    CellChange::Default => {
                        return Err(Error::Unsupported(format!(
                            "{:?} cannot reset `{column}` to its default",
                            dialect.kind()
                        )));
                    }
                }
            }
            let filter = key_filter(key, dialect, bind)?;
            Ok((
                format!("UPDATE {table} SET {} WHERE {filter}", sets.join(", ")),
                Some(1),
            ))
        }
        RowChange::Delete { key } => {
            let filter = key_filter(key, dialect, bind)?;
            Ok((format!("DELETE FROM {table} WHERE {filter}"), Some(1)))
        }
    }
}

/// `k1 = ? AND k2 = ?`. An empty key would match every row, and a NULL key value never matches
/// with `=`; both are refused instead of being turned into something that "works".
fn key_filter(
    key: &[(String, Value)],
    dialect: &dyn Dialect,
    bind: &mut dyn FnMut(&Value) -> String,
) -> Result<String> {
    if key.is_empty() {
        return Err(Error::InvalidInput(
            "no primary key: rows of this table cannot be changed".into(),
        ));
    }
    check_unique(key.iter().map(|(c, _)| c.as_str()))?;
    let mut parts = Vec::new();
    for (column, value) in key {
        if value.is_null() {
            return Err(Error::InvalidInput(format!(
                "key column `{column}` is NULL; the row cannot be located"
            )));
        }
        parts.push(format!("{} = {}", dialect.quote_ident(column), bind(value)));
    }
    Ok(parts.join(" AND "))
}

/// A value written as SQL text, for the preview only. Long text is cut so a preview of a big cell
/// stays readable.
fn literal(v: &Value) -> String {
    const MAX: usize = 200;
    let quote = |s: &str| {
        let shown: String = if s.chars().count() > MAX {
            s.chars().take(MAX).chain("…".chars()).collect()
        } else {
            s.to_owned()
        };
        format!("'{}'", shown.replace('\'', "''"))
    };
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Decimal(s) if s.parse::<f64>().is_ok() => s.clone(),
        Value::Bytes(b) => format!(
            "X'{}'",
            b.iter()
                .take(MAX)
                .map(|x| format!("{x:02X}"))
                .collect::<String>()
        ),
        Value::Float(f) => quote(&f.to_string()),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => quote(s),
        Value::Other { text, .. } => quote(text),
        _ => "'…'".into(),
    }
}

fn check_unique<'a>(columns: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for c in columns {
        if !seen.insert(c) {
            return Err(Error::InvalidInput(format!("column `{c}` appears twice")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use fg_core::{MySqlDialect, PostgresDialect, SqliteDialect};

    use super::*;

    fn users() -> ObjectPath {
        ObjectPath::new(None, "users")
    }

    fn set(c: &str, v: Value) -> (String, CellChange) {
        (c.into(), CellChange::Set(v))
    }

    fn id(n: i64) -> Vec<(String, Value)> {
        vec![("id".into(), Value::Int(n))]
    }

    #[test]
    fn insert_binds_values_and_skips_defaults() {
        let out = compile_changes(
            &users(),
            &[RowChange::Insert {
                cells: vec![
                    set("name", Value::Text("a'; DROP TABLE users;--".into())),
                    ("created".into(), CellChange::Default),
                    set("age", Value::Null),
                ],
            }],
            &PostgresDialect,
        )
        .unwrap();
        assert_eq!(
            out[0].sql,
            r#"INSERT INTO "users" ("name", "age") VALUES ($1, $2)"#
        );
        assert_eq!(
            out[0].params,
            [Value::Text("a'; DROP TABLE users;--".into()), Value::Null]
        );
        assert_eq!(out[0].expect_rows, None);
    }

    #[test]
    fn an_insert_with_nothing_set_uses_each_dialects_spelling() {
        let change = [RowChange::Insert { cells: vec![] }];
        let sql = |d: &dyn Dialect| {
            compile_changes(&users(), &change, d).unwrap()[0]
                .sql
                .clone()
        };
        assert_eq!(sql(&SqliteDialect), r#"INSERT INTO "users" DEFAULT VALUES"#);
        assert_eq!(
            sql(&PostgresDialect),
            r#"INSERT INTO "users" DEFAULT VALUES"#
        );
        assert_eq!(sql(&MySqlDialect), "INSERT INTO `users` () VALUES ()");
    }

    #[test]
    fn update_and_delete_locate_one_row_by_key() {
        let out = compile_changes(
            &ObjectPath::new(Some("app"), "users"),
            &[
                RowChange::Update {
                    key: vec![
                        ("a".into(), Value::Int(1)),
                        ("b".into(), Value::Text("x".into())),
                    ],
                    cells: vec![set("n", Value::Int(5)), ("m".into(), CellChange::Default)],
                },
                RowChange::Delete { key: id(9) },
            ],
            &PostgresDialect,
        )
        .unwrap();
        assert_eq!(
            out[0].sql,
            r#"UPDATE "app"."users" SET "n" = $1, "m" = DEFAULT WHERE "a" = $2 AND "b" = $3"#
        );
        assert_eq!(out[0].params.len(), 3);
        assert_eq!(out[0].expect_rows, Some(1));
        assert_eq!(out[1].sql, r#"DELETE FROM "app"."users" WHERE "id" = $1"#);
        assert_eq!(out[1].expect_rows, Some(1));
    }

    #[test]
    fn identifiers_are_quoted_not_spliced() {
        let out = compile_changes(
            &ObjectPath::new(None, "we\"ird"),
            &[RowChange::Delete {
                key: vec![("i\"d".into(), Value::Int(1))],
            }],
            &SqliteDialect,
        )
        .unwrap();
        assert_eq!(out[0].sql, r#"DELETE FROM "we""ird" WHERE "i""d" = ?"#);
    }

    #[test]
    fn unsafe_keys_are_refused() {
        let refuse = |change| compile_changes(&users(), &[change], &SqliteDialect).unwrap_err();
        // No key would hit every row.
        assert!(matches!(
            refuse(RowChange::Delete { key: vec![] }),
            Error::InvalidInput(_)
        ));
        // `id = NULL` matches nothing, `IS NULL` could match many: neither identifies a row.
        assert!(matches!(
            refuse(RowChange::Delete {
                key: vec![("id".into(), Value::Null)]
            }),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            refuse(RowChange::Update {
                key: id(1),
                cells: vec![]
            }),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            refuse(RowChange::Insert {
                cells: vec![set("a", Value::Int(1)), set("a", Value::Int(2))]
            }),
            Error::InvalidInput(_)
        ));
    }

    #[test]
    fn resetting_to_default_is_refused_where_it_cannot_be_written() {
        let change = RowChange::Update {
            key: id(1),
            cells: vec![("m".into(), CellChange::Default)],
        };
        assert!(matches!(
            compile_changes(&users(), std::slice::from_ref(&change), &SqliteDialect).unwrap_err(),
            Error::Unsupported(_)
        ));
        assert!(compile_changes(&users(), &[change], &MySqlDialect).is_ok());
    }

    #[test]
    fn the_preview_writes_the_values_in_without_changing_the_statement() {
        let out = compile_changes(
            &users(),
            &[
                RowChange::Update {
                    key: id(7),
                    cells: vec![
                        set("name", Value::Text("O'Brien ?".into())),
                        set("n", Value::Null),
                    ],
                },
                RowChange::Insert {
                    cells: vec![set("data", Value::Bytes(vec![0, 255]))],
                },
            ],
            &PostgresDialect,
        )
        .unwrap();
        assert_eq!(
            out[0].preview,
            r#"UPDATE "users" SET "name" = 'O''Brien ?', "n" = NULL WHERE "id" = 7;"#
        );
        assert_eq!(
            out[1].preview,
            r#"INSERT INTO "users" ("data") VALUES (X'00FF');"#
        );
        // What is executed is still the parameterised text.
        assert_eq!(
            out[0].sql,
            r#"UPDATE "users" SET "name" = $1, "n" = $2 WHERE "id" = $3"#
        );
        assert_eq!(out[0].params.len(), 3);
    }

    #[test]
    fn a_question_mark_in_an_identifier_does_not_confuse_the_preview() {
        let out = compile_changes(
            &users(),
            &[RowChange::Delete {
                key: vec![("why?".into(), Value::Int(1))],
            }],
            &SqliteDialect,
        )
        .unwrap();
        assert_eq!(out[0].preview, r#"DELETE FROM "users" WHERE "why?" = 1;"#);
    }

    #[test]
    fn the_error_names_the_failing_change() {
        let err = compile_changes(
            &users(),
            &[
                RowChange::Delete { key: id(1) },
                RowChange::Delete { key: vec![] },
            ],
            &SqliteDialect,
        )
        .unwrap_err();
        assert!(err.to_string().contains("change 2"), "{err}");
    }
}
