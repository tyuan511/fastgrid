//! Compile staged schema changes into DDL, and render `CREATE TABLE`.
//!
//! This is the structure-editing counterpart of `dml::compile_changes`: the
//! grid's structure tab stages [`SchemaChange`]s, and this module turns them
//! into the statements the service executes, in order. One bad change fails
//! the whole batch: a partial schema script is worse than none.
//!
//! Engines differ in what their native `ALTER TABLE` can express, and the
//! differences live on the [`Dialect`] trait, not in a `match` on the engine
//! here: `alter_column_sql` returns `None` where the engine has no native
//! form, and `supports_table_rebuild` marks the engines where the compiler
//! expands such a change into a rebuild (CREATE new + copy + DROP +
//! RENAME + recreate indexes and triggers). A rebuild keeps the table's
//! final name, so trigger definitions — which name that table — are
//! recreated verbatim afterwards; a trigger without a stored definition
//! refuses the rebuild rather than being silently dropped.

use fg_core::{
    CheckConstraint, ColumnInfo, DataType, Dialect, Error, ForeignKeyInfo, IndexInfo, ObjectPath,
    Result, TableDetail, TypeCategory, Value,
};

/// One staged change to a table's structure, in the order the user made it.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaChange {
    AddColumn {
        column: ColumnInfo,
    },
    DropColumn {
        name: String,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    AlterColumn {
        name: String,
        /// New native type text, or `None` to leave the type alone.
        new_type: Option<String>,
        /// New nullability, or `None` to leave it alone.
        nullable: Option<bool>,
        /// `None` leaves the default alone, `Some(None)` drops it,
        /// `Some(Some(expr))` sets it.
        default: Option<Option<String>>,
    },
    AddIndex {
        index: IndexInfo,
    },
    DropIndex {
        name: String,
    },
    AddForeignKey {
        fk: ForeignKeyInfo,
    },
    DropForeignKey {
        name: String,
    },
    CreateTable {
        detail: TableDetail,
    },
    DropTable,
    RenameTable {
        new_name: String,
    },
}

/// One statement of a compiled schema script, with a short description for
/// the preview panel. The description is human text, never executed.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaStatement {
    pub sql: String,
    pub description: String,
}

fn stmt(sql: String, description: String) -> SchemaStatement {
    SchemaStatement { sql, description }
}

/// Compile in order, tracking a working copy of the table so later changes
/// in the same batch see earlier ones (a renamed column can be altered, a
/// dropped column cannot). `detail` is the table as it stands before the
/// batch; `path` is its current name, which `RenameTable` moves.
pub fn compile_schema_changes(
    path: &ObjectPath,
    detail: &TableDetail,
    changes: &[SchemaChange],
    dialect: &dyn Dialect,
) -> Result<Vec<SchemaStatement>> {
    let mut working = detail.clone();
    let mut current_path = path.clone();
    let mut dropped = false;
    let mut out = Vec::new();
    for (i, change) in changes.iter().enumerate() {
        if dropped && !matches!(change, SchemaChange::CreateTable { .. }) {
            return Err(Error::InvalidInput(format!(
                "change {}: the table was dropped earlier in this batch",
                i + 1
            )));
        }
        let steps =
            compile_one(&current_path, &mut working, change, dialect).map_err(|e| match e {
                Error::InvalidInput(m) => Error::InvalidInput(format!("change {}: {m}", i + 1)),
                Error::Unsupported(m) => Error::Unsupported(format!("change {}: {m}", i + 1)),
                other => other,
            })?;
        for step in steps {
            out.push(step);
        }
        match change {
            SchemaChange::CreateTable { detail: created } => {
                working = created.clone();
                current_path = created.path.clone();
                dropped = false;
            }
            SchemaChange::DropTable => dropped = true,
            SchemaChange::RenameTable { new_name } => current_path.name = new_name.clone(),
            _ => {}
        }
    }
    Ok(out)
}

fn compile_one(
    path: &ObjectPath,
    working: &mut TableDetail,
    change: &SchemaChange,
    dialect: &dyn Dialect,
) -> Result<Vec<SchemaStatement>> {
    let table = dialect.qualified_name(path);
    match change {
        SchemaChange::AddColumn { column } => {
            if working.column(&column.name).is_some() {
                return Err(Error::InvalidInput(format!(
                    "column `{}` already exists",
                    column.name
                )));
            }
            if column.primary_key_position.is_some() {
                return Err(Error::InvalidInput(
                    "a primary key column cannot be added to an existing table; \
                     create the table with the key instead"
                        .into(),
                ));
            }
            if column.auto_increment {
                return Err(Error::InvalidInput(
                    "an auto-increment column must be created with the table".into(),
                ));
            }
            if dialect.supports_table_rebuild() && !column.nullable && column.default.is_none() {
                // SQLite refuses ADD COLUMN ... NOT NULL without a non-null
                // default. Say so up front instead of failing at apply time.
                return Err(Error::InvalidInput(
                    "SQLite cannot add a NOT NULL column without a default".into(),
                ));
            }
            let mut column = column.clone();
            column.ordinal = working.columns.iter().map(|c| c.ordinal).max().unwrap_or(0) + 1;
            let sql = dialect.add_column_sql(&table, &column);
            working.columns.push(column.clone());
            Ok(vec![stmt(sql, format!("Add column `{}`", column.name))])
        }
        SchemaChange::DropColumn { name } => {
            let Some(pos) = working.columns.iter().position(|c| &c.name == name) else {
                return Err(Error::InvalidInput(format!(
                    "column `{name}` does not exist"
                )));
            };
            let column = working.columns[pos].clone();
            if dialect.supports_table_rebuild() && drop_needs_rebuild(&column, working) {
                let mut new_detail = working.clone();
                remove_column(&mut new_detail, name);
                return expand_rebuild(
                    path,
                    working,
                    &new_detail,
                    dialect,
                    format!("Drop column `{name}`"),
                );
            }
            let sql = dialect.drop_column_sql(&table, name);
            remove_column(working, name);
            Ok(vec![stmt(sql, format!("Drop column `{name}`"))])
        }
        SchemaChange::RenameColumn { from, to } => {
            let Some(pos) = working.columns.iter().position(|c| &c.name == from) else {
                return Err(Error::InvalidInput(format!(
                    "column `{from}` does not exist"
                )));
            };
            if working.column(to).is_some() {
                return Err(Error::InvalidInput(format!("column `{to}` already exists")));
            }
            working.columns[pos].name = to.clone();
            rename_in_indexes(working, from, to);
            rename_in_foreign_keys(working, from, to);
            let sql = dialect.rename_column_sql(&table, from, to);
            Ok(vec![stmt(sql, format!("Rename column `{from}` to `{to}`"))])
        }
        SchemaChange::AlterColumn {
            name,
            new_type,
            nullable,
            default,
        } => {
            let Some(current) = working.column(name).cloned() else {
                return Err(Error::InvalidInput(format!(
                    "column `{name}` does not exist"
                )));
            };
            let type_changed = new_type
                .as_ref()
                .is_some_and(|t| t != &current.data_type.native);
            let null_changed = nullable.is_some_and(|n| n != current.nullable);
            let default_changed = default
                .as_ref()
                .is_some_and(|d| d.as_deref() != current.default.as_deref());
            if !type_changed && !null_changed && !default_changed {
                return Ok(Vec::new());
            }
            let description = format!("Alter column `{name}`");
            if let Some(sql) = dialect.alter_column_sql(
                &table,
                &current,
                new_type.as_deref(),
                *nullable,
                default.as_ref().map(|d| d.as_deref()),
            ) {
                apply_alter(working, name, new_type, nullable, default);
                return Ok(vec![stmt(sql, description)]);
            }
            if dialect.supports_table_rebuild() {
                let mut new_detail = working.clone();
                apply_alter(&mut new_detail, name, new_type, nullable, default);
                return expand_rebuild(path, working, &new_detail, dialect, description);
            }
            Err(Error::Unsupported(
                "altering a column is not supported by this engine".to_string(),
            ))
        }
        SchemaChange::AddIndex { index } => {
            if index.primary {
                return Err(Error::InvalidInput(
                    "a primary key is a table constraint, not an index to add".into(),
                ));
            }
            if working.indexes.iter().any(|ix| ix.name == index.name) {
                return Err(Error::InvalidInput(format!(
                    "index `{}` already exists",
                    index.name
                )));
            }
            let sql = dialect.create_index_sql(&table, index);
            working.indexes.push(index.clone());
            Ok(vec![stmt(sql, format!("Add index `{}`", index.name))])
        }
        SchemaChange::DropIndex { name } => {
            let Some(pos) = working.indexes.iter().position(|ix| &ix.name == name) else {
                return Err(Error::InvalidInput(format!(
                    "index `{name}` does not exist"
                )));
            };
            if working.indexes[pos].primary {
                return Err(Error::InvalidInput(
                    "a primary key cannot be dropped as an index".into(),
                ));
            }
            let sql = dialect.drop_index_sql(path, name);
            working.indexes.remove(pos);
            Ok(vec![stmt(sql, format!("Drop index `{name}`"))])
        }
        SchemaChange::AddForeignKey { fk } => {
            if let Some(name) = &fk.name
                && working
                    .foreign_keys
                    .iter()
                    .any(|old| old.name.as_ref() == Some(name))
            {
                return Err(Error::InvalidInput(format!(
                    "foreign key `{name}` already exists"
                )));
            }
            if dialect.supports_table_rebuild() {
                let mut new_detail = working.clone();
                new_detail.foreign_keys.push(fk.clone());
                return expand_rebuild(
                    path,
                    working,
                    &new_detail,
                    dialect,
                    "Add foreign key".into(),
                );
            }
            let sql = dialect.add_fk_sql(&table, fk);
            working.foreign_keys.push(fk.clone());
            Ok(vec![stmt(sql, "Add foreign key".into())])
        }
        SchemaChange::DropForeignKey { name } => {
            let Some(pos) = working
                .foreign_keys
                .iter()
                .position(|fk| fk.name.as_deref() == Some(name.as_str()))
            else {
                return Err(Error::InvalidInput(format!(
                    "foreign key `{name}` does not exist"
                )));
            };
            if dialect.supports_table_rebuild() {
                let mut new_detail = working.clone();
                new_detail.foreign_keys.remove(pos);
                return expand_rebuild(
                    path,
                    working,
                    &new_detail,
                    dialect,
                    format!("Drop foreign key `{name}`"),
                );
            }
            let sql = dialect.drop_fk_sql(&table, name);
            working.foreign_keys.remove(pos);
            Ok(vec![stmt(sql, format!("Drop foreign key `{name}`"))])
        }
        SchemaChange::CreateTable { detail: created } => {
            let mut steps = vec![stmt(
                render_create_table(created, dialect),
                format!("Create table `{}`", created.path.name),
            )];
            for index in created.indexes.iter().filter(|ix| !ix.primary) {
                steps.push(stmt(
                    dialect.create_index_sql(&dialect.qualified_name(&created.path), index),
                    format!("Create index `{}`", index.name),
                ));
            }
            Ok(steps)
        }
        SchemaChange::DropTable => Ok(vec![stmt(
            dialect.drop_table_sql(&table),
            format!("Drop table `{}`", path.name),
        )]),
        SchemaChange::RenameTable { new_name } => Ok(vec![stmt(
            dialect.rename_table_sql(path, new_name),
            format!("Rename table `{}` to `{new_name}`", path.name),
        )]),
    }
}

/// Whether dropping this column on a rebuild engine needs the rebuild path:
/// native DROP COLUMN refuses columns in the primary key, in a unique
/// index, or in one of the table's own foreign keys.
fn drop_needs_rebuild(column: &ColumnInfo, detail: &TableDetail) -> bool {
    if column.primary_key_position.is_some() {
        return true;
    }
    if detail.indexes.iter().any(|ix| {
        ix.unique
            && ix
                .columns
                .iter()
                .any(|c| !c.expression && c.name == column.name)
    }) {
        return true;
    }
    detail
        .foreign_keys
        .iter()
        .any(|fk| fk.columns.iter().any(|c| c == &column.name))
}

/// Remove a column from a working detail, keeping dependent index and
/// foreign-key lists valid (columns drop out of them; emptied ones go).
fn remove_column(detail: &mut TableDetail, name: &str) {
    detail.columns.retain(|c| c.name != name);
    for index in detail.indexes.iter_mut() {
        index.columns.retain(|c| c.expression || c.name != name);
    }
    detail.indexes.retain(|ix| !ix.columns.is_empty());
    for fk in detail.foreign_keys.iter_mut() {
        fk.columns.retain(|c| c != name);
    }
    detail.foreign_keys.retain(|fk| !fk.columns.is_empty());
}

fn rename_in_indexes(detail: &mut TableDetail, from: &str, to: &str) {
    for index in detail.indexes.iter_mut() {
        for col in index.columns.iter_mut() {
            if !col.expression && col.name == from {
                col.name = to.to_owned();
            }
        }
    }
}

fn rename_in_foreign_keys(detail: &mut TableDetail, from: &str, to: &str) {
    for fk in detail.foreign_keys.iter_mut() {
        for col in fk.columns.iter_mut() {
            if col == from {
                *col = to.to_owned();
            }
        }
    }
}

fn apply_alter(
    detail: &mut TableDetail,
    name: &str,
    new_type: &Option<String>,
    nullable: &Option<bool>,
    default: &Option<Option<String>>,
) {
    if let Some(col) = detail.columns.iter_mut().find(|c| c.name == name) {
        if let Some(t) = new_type {
            col.data_type.native = t.clone();
        }
        if let Some(n) = nullable {
            col.nullable = *n;
        }
        if let Some(d) = default {
            col.default = d.clone();
        }
    }
}

/// Expand a change into a table rebuild: CREATE the new shape under a
/// temporary name, copy the shared columns, DROP the old table, RENAME the
/// new one back, recreate indexes, then recreate triggers verbatim. The
/// table ends up with its original name, so trigger definitions — which
/// name that table — need no rewriting. A trigger without a stored
/// definition refuses the rebuild: dropping it silently would be worse.
fn expand_rebuild(
    path: &ObjectPath,
    old_detail: &TableDetail,
    new_detail: &TableDetail,
    dialect: &dyn Dialect,
    description: String,
) -> Result<Vec<SchemaStatement>> {
    let temp_name = format!("{}__fg_rebuild", path.name);
    let temp_path = ObjectPath::new(path.namespace.as_deref(), &temp_name);
    let mut steps = vec![stmt(
        render_create_table_as(new_detail, &temp_path, dialect),
        format!("{description} via table rebuild: create new table"),
    )];
    let shared: Vec<&str> = new_detail
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| old_detail.column(n).is_some())
        .collect();
    if !shared.is_empty() {
        let cols = shared
            .iter()
            .map(|c| dialect.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        steps.push(stmt(
            format!(
                "INSERT INTO {} ({cols}) SELECT {cols} FROM {}",
                dialect.qualified_name(&temp_path),
                dialect.qualified_name(path),
            ),
            "Copy rows into the rebuilt table".into(),
        ));
    }
    steps.push(stmt(
        dialect.drop_table_sql(&dialect.qualified_name(path)),
        "Drop the old table".into(),
    ));
    steps.push(stmt(
        dialect.rename_table_sql(&temp_path, &path.name),
        "Rename the rebuilt table back".into(),
    ));
    for index in new_detail.indexes.iter().filter(|ix| !ix.primary) {
        steps.push(stmt(
            dialect.create_index_sql(&dialect.qualified_name(path), index),
            format!("Recreate index `{}`", index.name),
        ));
    }
    for trigger in &old_detail.triggers {
        let Some(definition) = &trigger.definition else {
            return Err(Error::InvalidInput(format!(
                "cannot rebuild: trigger `{}` has no stored definition to recreate",
                trigger.name
            )));
        };
        steps.push(stmt(
            definition.clone(),
            format!("Recreate trigger `{}`", trigger.name),
        ));
    }
    Ok(steps)
}

/// Render `CREATE TABLE` for a detail, with table-level primary key and
/// foreign-key constraints and CHECKs. Indexes are separate statements
/// (see [`SchemaChange::CreateTable`]).
pub fn render_create_table(detail: &TableDetail, dialect: &dyn Dialect) -> String {
    render_create_table_as(detail, &detail.path, dialect)
}

/// Same as [`render_create_table`] but under another name, for rebuilds.
pub fn render_create_table_as(
    detail: &TableDetail,
    path: &ObjectPath,
    dialect: &dyn Dialect,
) -> String {
    let pk = detail.primary_key();
    // SQLite's AUTOINCREMENT only exists as an inline INTEGER PRIMARY KEY.
    let inline_pk = dialect.integer_primary_key_autoincrement()
        && pk.len() == 1
        && detail.column(pk[0]).is_some_and(|c| {
            c.auto_increment && c.data_type.native.eq_ignore_ascii_case("integer")
        });
    let mut parts: Vec<String> = Vec::new();
    for col in &detail.columns {
        if inline_pk && Some(col.name.as_str()) == pk.first().copied() {
            parts.push(format!(
                "{} INTEGER PRIMARY KEY AUTOINCREMENT",
                dialect.quote_ident(&col.name)
            ));
        } else {
            parts.push(dialect.column_def_sql(col));
        }
    }
    if !inline_pk && !pk.is_empty() {
        let cols = pk
            .iter()
            .map(|c| dialect.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("PRIMARY KEY ({cols})"));
    }
    for fk in &detail.foreign_keys {
        parts.push(dialect.fk_constraint_sql(fk));
    }
    for check in &detail.checks {
        parts.push(check_sql(check, dialect));
    }
    format!(
        "CREATE TABLE {} (\n  {}\n)",
        dialect.qualified_name(path),
        parts.join(",\n  ")
    )
}

fn check_sql(check: &CheckConstraint, dialect: &dyn Dialect) -> String {
    match &check.name {
        Some(name) => format!(
            "CONSTRAINT {} CHECK ({})",
            dialect.quote_ident(name),
            check.expression
        ),
        None => format!("CHECK ({})", check.expression),
    }
}

/// Parse one CSV field into the value a column of this type holds. An empty
/// field is NULL, the CSV convention; anything unparseable is an error the
/// importer reports against the row. Date and time text is passed through
/// for the engine to validate at insert time.
pub fn coerce_text(text: &str, data_type: &DataType) -> Result<Value> {
    if text.is_empty() {
        return Ok(Value::Null);
    }
    match data_type.category {
        TypeCategory::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => Ok(Value::Bool(true)),
            "false" | "f" | "0" | "no" | "n" | "off" => Ok(Value::Bool(false)),
            _ => Err(Error::InvalidInput(format!("{text:?} is not a boolean"))),
        },
        TypeCategory::Integer => {
            if data_type.params.unsigned {
                text.parse::<u64>()
                    .map(Value::UInt)
                    .map_err(|_| Error::InvalidInput(format!("{text:?} is not an integer")))
            } else {
                text.parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| Error::InvalidInput(format!("{text:?} is not an integer")))
            }
        }
        TypeCategory::Float => text
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| Error::InvalidInput(format!("{text:?} is not a number"))),
        TypeCategory::Decimal => Ok(Value::Decimal(text.to_owned())),
        TypeCategory::Json => {
            serde_json::from_str::<serde_json::Value>(text)
                .map_err(|_| Error::InvalidInput(format!("{text:?} is not valid JSON")))?;
            Ok(Value::Json(text.to_owned()))
        }
        TypeCategory::Binary => Ok(Value::Bytes(text.as_bytes().to_vec())),
        TypeCategory::Uuid => Ok(Value::Uuid(text.to_owned())),
        _ => Ok(Value::Text(text.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fg_core::{ColumnInfo, DataType, IndexColumn, ObjectKind, TypeCategory, TypeParams};
    use fg_core::{MySqlDialect, PostgresDialect, SqliteDialect};

    fn col(name: &str, native: &str) -> ColumnInfo {
        let mut c = ColumnInfo::new(name, 1, DataType::new(native, TypeCategory::Text));
        c.nullable = true;
        c
    }

    fn int_col(name: &str) -> ColumnInfo {
        let mut c = ColumnInfo::new(name, 1, DataType::new("INTEGER", TypeCategory::Integer));
        c.nullable = false;
        c
    }

    fn detail() -> TableDetail {
        let mut id = int_col("id");
        id.primary_key_position = Some(1);
        TableDetail {
            path: ObjectPath::new(Some("public"), "users"),
            kind: ObjectKind::Table,
            comment: None,
            columns: vec![id, col("name", "TEXT")],
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            row_estimate: None,
            size_bytes: None,
            options: Default::default(),
        }
    }

    fn sqls(steps: &[SchemaStatement]) -> Vec<&str> {
        steps.iter().map(|s| s.sql.as_str()).collect()
    }

    #[test]
    fn add_drop_rename_column_per_engine() {
        let d = detail();
        let pg = &PostgresDialect;
        let my = &MySqlDialect;
        let lite = &SqliteDialect;
        let path = ObjectPath::new(Some("public"), "users");
        let mut age = col("age", "INTEGER");
        age.data_type.category = TypeCategory::Integer;

        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AddColumn {
                column: age.clone(),
            }],
            pg,
        )
        .unwrap();
        assert_eq!(
            sqls(&steps),
            ["ALTER TABLE \"public\".\"users\" ADD COLUMN \"age\" INTEGER"]
        );
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AddColumn {
                column: age.clone(),
            }],
            my,
        )
        .unwrap();
        // MySQL qualifies with the namespace as the database.
        assert_eq!(
            sqls(&steps),
            ["ALTER TABLE `public`.`users` ADD COLUMN `age` INTEGER"]
        );
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::DropColumn {
                name: "name".into(),
            }],
            lite,
        )
        .unwrap();
        assert_eq!(
            sqls(&steps),
            ["ALTER TABLE \"public\".\"users\" DROP COLUMN \"name\""]
        );
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::RenameColumn {
                from: "name".into(),
                to: "full_name".into(),
            }],
            pg,
        )
        .unwrap();
        assert_eq!(
            sqls(&steps),
            ["ALTER TABLE \"public\".\"users\" RENAME COLUMN \"name\" TO \"full_name\""]
        );
    }

    #[test]
    fn alter_column_uses_native_forms() {
        let d = detail();
        let path = ObjectPath::new(Some("public"), "users");
        let change = SchemaChange::AlterColumn {
            name: "name".into(),
            new_type: Some("VARCHAR(200)".into()),
            nullable: Some(false),
            default: Some(Some("'anon'".into())),
        };
        let steps =
            compile_schema_changes(&path, &d, std::slice::from_ref(&change), &PostgresDialect)
                .unwrap();
        assert_eq!(
            sqls(&steps),
            [
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"name\" TYPE VARCHAR(200), ALTER COLUMN \"name\" SET NOT NULL, ALTER COLUMN \"name\" SET DEFAULT 'anon'"
            ]
        );
        let steps = compile_schema_changes(&path, &d, &[change], &MySqlDialect).unwrap();
        assert_eq!(
            sqls(&steps),
            [
                "ALTER TABLE `public`.`users` MODIFY COLUMN `name` VARCHAR(200) NOT NULL DEFAULT 'anon'"
            ]
        );
        // A no-op alter emits nothing.
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AlterColumn {
                name: "name".into(),
                new_type: Some("TEXT".into()),
                nullable: None,
                default: None,
            }],
            &PostgresDialect,
        )
        .unwrap();
        assert!(steps.is_empty());
    }

    #[test]
    fn sqlite_alter_expands_to_a_rebuild() {
        let mut d = detail();
        d.indexes = vec![IndexInfo {
            name: "users_name".into(),
            columns: vec![IndexColumn::column("name")],
            unique: false,
            primary: false,
            method: None,
            predicate: None,
            comment: None,
            extra: Default::default(),
        }];
        d.triggers = vec![fg_core::TriggerInfo {
            name: "users_audit".into(),
            timing: "AFTER".into(),
            event: "INSERT".into(),
            definition: Some(
                "CREATE TRIGGER users_audit AFTER INSERT ON users BEGIN SELECT 1; END".into(),
            ),
            enabled: true,
        }];
        let path = ObjectPath::new(None, "users");
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AlterColumn {
                name: "name".into(),
                new_type: Some("VARCHAR(100)".into()),
                nullable: None,
                default: None,
            }],
            &SqliteDialect,
        )
        .unwrap();
        let s = sqls(&steps);
        assert_eq!(s.len(), 6, "{s:?}");
        assert!(s[0].starts_with("CREATE TABLE \"users__fg_rebuild\""));
        assert!(s[0].contains("VARCHAR(100)"));
        assert_eq!(
            s[1],
            "INSERT INTO \"users__fg_rebuild\" (\"id\", \"name\") SELECT \"id\", \"name\" FROM \"users\""
        );
        assert_eq!(s[2], "DROP TABLE \"users\"");
        assert_eq!(
            s[3],
            "ALTER TABLE \"users__fg_rebuild\" RENAME TO \"users\""
        );
        assert_eq!(s[4], "CREATE INDEX \"users_name\" ON \"users\" (\"name\")");
        assert_eq!(
            s[5],
            "CREATE TRIGGER users_audit AFTER INSERT ON users BEGIN SELECT 1; END"
        );
    }

    #[test]
    fn sqlite_guards_and_rebuild_triggers() {
        let d = detail();
        let path = ObjectPath::new(None, "users");
        // NOT NULL without a default cannot be added on SQLite.
        let mut c = col("age", "INTEGER");
        c.nullable = false;
        let err = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AddColumn { column: c }],
            &SqliteDialect,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        // Dropping the primary key column rebuilds instead of failing.
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::DropColumn { name: "id".into() }],
            &SqliteDialect,
        )
        .unwrap();
        assert!(sqls(&steps)[0].starts_with("CREATE TABLE \"users__fg_rebuild\""));
        // A primary key column cannot be added to an existing table.
        let mut c = col("id2", "INTEGER");
        c.primary_key_position = Some(1);
        let err = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AddColumn { column: c }],
            &PostgresDialect,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn indexes_and_foreign_keys() {
        let d = detail();
        let path = ObjectPath::new(Some("public"), "users");
        let index = IndexInfo {
            name: "users_name".into(),
            columns: vec![IndexColumn::column("name")],
            unique: true,
            primary: false,
            method: None,
            predicate: None,
            comment: None,
            extra: Default::default(),
        };
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AddIndex {
                index: index.clone(),
            }],
            &PostgresDialect,
        )
        .unwrap();
        assert_eq!(
            sqls(&steps),
            ["CREATE UNIQUE INDEX \"users_name\" ON \"public\".\"users\" (\"name\")"]
        );
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::DropIndex {
                name: "users_name".into(),
            }],
            &MySqlDialect,
        );
        // The index does not exist in the fixture detail.
        assert!(steps.is_err());
        let fk = ForeignKeyInfo {
            name: Some("users_group".into()),
            columns: vec!["id".into()],
            referenced: ObjectPath::new(Some("public"), "groups"),
            referenced_columns: vec!["id".into()],
            on_update: None,
            on_delete: Some(fg_core::ReferentialAction::Cascade),
            extra: Default::default(),
        };
        let steps = compile_schema_changes(
            &path,
            &d,
            &[SchemaChange::AddForeignKey { fk }],
            &PostgresDialect,
        )
        .unwrap();
        assert_eq!(
            sqls(&steps),
            [
                "ALTER TABLE \"public\".\"users\" ADD CONSTRAINT \"users_group\" FOREIGN KEY (\"id\") REFERENCES \"public\".\"groups\" (\"id\") ON DELETE CASCADE"
            ]
        );
    }

    #[test]
    fn render_create_table_with_constraints() {
        let mut d = detail();
        d.foreign_keys = vec![ForeignKeyInfo {
            name: None,
            columns: vec!["id".into()],
            referenced: ObjectPath::new(Some("public"), "groups"),
            referenced_columns: vec!["id".into()],
            on_update: None,
            on_delete: None,
            extra: Default::default(),
        }];
        d.checks = vec![CheckConstraint {
            name: Some("name_not_empty".into()),
            expression: "length(name) > 0".into(),
        }];
        let sql = render_create_table(&d, &PostgresDialect);
        assert_eq!(
            sql,
            "CREATE TABLE \"public\".\"users\" (\n  \"id\" INTEGER NOT NULL,\n  \"name\" TEXT,\n  PRIMARY KEY (\"id\"),\n  FOREIGN KEY (\"id\") REFERENCES \"public\".\"groups\" (\"id\"),\n  CONSTRAINT \"name_not_empty\" CHECK (length(name) > 0)\n)"
        );
    }

    #[test]
    fn coerce_text_parses_common_types() {
        let int = DataType::new("INTEGER", TypeCategory::Integer);
        assert_eq!(coerce_text("42", &int).unwrap(), Value::Int(42));
        assert_eq!(coerce_text("", &int).unwrap(), Value::Null);
        assert!(coerce_text("abc", &int).is_err());
        let params = TypeParams {
            unsigned: true,
            ..Default::default()
        };
        let ubig = DataType {
            native: "BIGINT UNSIGNED".into(),
            category: TypeCategory::Integer,
            params,
            variants: vec![],
        };
        assert_eq!(
            coerce_text("18446744073709551615", &ubig).unwrap(),
            Value::UInt(u64::MAX)
        );
        let bool_t = DataType::new("BOOLEAN", TypeCategory::Bool);
        assert_eq!(coerce_text("yes", &bool_t).unwrap(), Value::Bool(true));
        assert!(coerce_text("maybe", &bool_t).is_err());
        let json_t = DataType::new("JSON", TypeCategory::Json);
        assert_eq!(
            coerce_text("{\"a\": 1}", &json_t).unwrap(),
            Value::Json("{\"a\": 1}".into())
        );
        assert!(coerce_text("{nope", &json_t).is_err());
    }
}
