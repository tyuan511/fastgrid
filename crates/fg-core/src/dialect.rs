use crate::{ColumnInfo, DbKind, ObjectPath, Value};

/// SQL syntax differences between engines. Pure and IO-free so `fg-sql` can use it for
/// building browse queries and DML without a connection.
pub trait Dialect: Send + Sync {
    fn kind(&self) -> DbKind;

    /// Quote an identifier, escaping the quote character.
    fn quote_ident(&self, ident: &str) -> String;

    /// 1-based positional parameter placeholder.
    fn placeholder(&self, index: usize) -> String;

    fn qualified_name(&self, path: &ObjectPath) -> String {
        match &path.namespace {
            Some(ns) => format!("{}.{}", self.quote_ident(ns), self.quote_ident(&path.name)),
            None => self.quote_ident(&path.name),
        }
    }

    fn limit_clause(&self, limit: u64, offset: u64) -> String {
        if offset == 0 {
            format!("LIMIT {limit}")
        } else {
            format!("LIMIT {limit} OFFSET {offset}")
        }
    }

    fn default_port(&self) -> Option<u16>;

    /// Convert an expression to text, so `LIKE` works on any column type.
    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS TEXT)")
    }

    /// `ORDER BY x NULLS FIRST/LAST`. MySQL lacks it and needs an `x IS NULL` sort key instead.
    fn supports_nulls_ordering(&self) -> bool {
        true
    }

    /// Whether `UPDATE t SET c = DEFAULT` is valid. SQLite has no `DEFAULT` keyword in
    /// expressions, so "reset to default" is not expressible there without reading the default
    /// back, which a pure compiler cannot do.
    fn supports_default_keyword(&self) -> bool {
        true
    }

    /// Whether `INSERT/UPDATE/DELETE ... RETURNING` is valid, so a commit can
    /// read back the row it just wrote (defaults, generated keys) instead of
    /// trusting the values it sent.
    fn supports_returning(&self) -> bool {
        false
    }

    /// A statement that yields the generated key of the last single-row
    /// insert on this session, for dialects without `RETURNING` (MySQL's
    /// `LAST_INSERT_ID()`). `None` means the dialect offers no such
    /// function, and a commit without `RETURNING` simply has no readback.
    fn last_insert_id_sql(&self) -> Option<&str> {
        None
    }

    /// An `INSERT` that sets no column: every column takes its default.
    fn insert_defaults(&self, table: &str) -> String {
        format!("INSERT INTO {table} DEFAULT VALUES")
    }

    /// A value as an exact SQL literal, for SQL export. Unlike a preview,
    /// nothing is truncated: what this renders must round-trip.
    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(_) => "NULL".into(),
            Value::Decimal(s) if s.parse::<f64>().is_ok() => s.clone(),
            Value::Bytes(b) => self.bytes_literal(b),
            Value::Array(items) => {
                let inner = items
                    .iter()
                    .map(|item| self.literal(item))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("ARRAY[{inner}]")
            }
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Json(s)
            | Value::Uuid(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::Timestamp(s)
            | Value::TimestampTz(s)
            | Value::Interval(s) => quote_literal(s),
            // Truncated cells must never reach an export (exports disable
            // cell truncation); NULL is the least surprising fallback.
            _ => "NULL".into(),
        }
    }

    /// A byte string as a SQL literal. PostgreSQL's hex format differs.
    fn bytes_literal(&self, bytes: &[u8]) -> String {
        format!(
            "X'{}'",
            bytes.iter().map(|b| format!("{b:02X}")).collect::<String>()
        )
    }

    /// One column definition for `CREATE TABLE` / `ADD COLUMN`: name, native
    /// type, nullability, default. Primary keys stay table-level constraints
    /// (except SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT`, which the
    /// table renderer handles), so this never emits one.
    fn column_def_sql(&self, col: &ColumnInfo) -> String {
        let mut def = format!("{} {}", self.quote_ident(&col.name), col.data_type.native);
        if !col.nullable {
            def.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default {
            def.push_str(&format!(" DEFAULT {default}"));
        }
        def
    }

    /// Whether this engine can rebuild a table (CREATE new + copy + DROP +
    /// RENAME) for changes its native ALTER cannot express. Only SQLite.
    fn supports_table_rebuild(&self) -> bool {
        false
    }

    /// Whether a single integer primary-key column with auto-increment is
    /// written inline (`INTEGER PRIMARY KEY AUTOINCREMENT`). Only SQLite
    /// needs this form; other engines keep the key as a table constraint.
    fn integer_primary_key_autoincrement(&self) -> bool {
        false
    }

    fn add_column_sql(&self, table: &str, col: &ColumnInfo) -> String {
        format!(
            "ALTER TABLE {table} ADD COLUMN {}",
            self.column_def_sql(col)
        )
    }

    fn drop_column_sql(&self, table: &str, col: &str) -> String {
        format!("ALTER TABLE {table} DROP COLUMN {}", self.quote_ident(col))
    }

    fn rename_column_sql(&self, table: &str, from: &str, to: &str) -> String {
        format!(
            "ALTER TABLE {table} RENAME COLUMN {} TO {}",
            self.quote_ident(from),
            self.quote_ident(to)
        )
    }

    fn rename_table_sql(&self, path: &ObjectPath, new_name: &str) -> String {
        format!(
            "ALTER TABLE {} RENAME TO {}",
            self.qualified_name(path),
            self.quote_ident(new_name)
        )
    }

    fn drop_table_sql(&self, table: &str) -> String {
        format!("DROP TABLE {table}")
    }

    fn create_index_sql(&self, table: &str, index: &crate::IndexInfo) -> String {
        let unique = if index.unique { "UNIQUE " } else { "" };
        let cols = index
            .columns
            .iter()
            .map(|c| {
                if c.expression {
                    c.name.clone()
                } else {
                    let mut s = self.quote_ident(&c.name);
                    if c.descending {
                        s.push_str(" DESC");
                    }
                    s
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            "CREATE {unique}INDEX {} ON {table} ({cols})",
            self.quote_ident(&index.name)
        );
        if let Some(method) = &index.method {
            sql.push_str(&format!(" USING {method}"));
        }
        if self.supports_partial_index()
            && let Some(predicate) = &index.predicate
        {
            sql.push_str(&format!(" WHERE {predicate}"));
        }
        sql
    }

    /// Whether `WHERE` predicates on indexes survive rendering. MySQL has no
    /// partial indexes.
    fn supports_partial_index(&self) -> bool {
        true
    }

    fn drop_index_sql(&self, _path: &ObjectPath, name: &str) -> String {
        format!("DROP INDEX {}", self.quote_ident(name))
    }

    fn add_fk_sql(&self, table: &str, fk: &crate::ForeignKeyInfo) -> String {
        format!("ALTER TABLE {table} ADD {}", self.fk_constraint_sql(fk))
    }

    /// A foreign-key constraint as it appears inside `CREATE TABLE` or after
    /// `ADD`: `CONSTRAINT name FOREIGN KEY (...) REFERENCES ...`.
    fn fk_constraint_sql(&self, fk: &crate::ForeignKeyInfo) -> String {
        let mut sql = String::new();
        if let Some(name) = &fk.name {
            sql.push_str(&format!("CONSTRAINT {} ", self.quote_ident(name)));
        }
        let cols = fk
            .columns
            .iter()
            .map(|c| self.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!("FOREIGN KEY ({cols}) REFERENCES "));
        let referenced =
            crate::ObjectPath::new(fk.referenced.namespace.as_deref(), &fk.referenced.name);
        sql.push_str(&self.qualified_name(&referenced));
        if !fk.referenced_columns.is_empty() {
            let refcols = fk
                .referenced_columns
                .iter()
                .map(|c| self.quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" ({refcols})"));
        }
        if let Some(action) = &fk.on_update {
            sql.push_str(&format!(" ON UPDATE {}", action.sql()));
        }
        if let Some(action) = &fk.on_delete {
            sql.push_str(&format!(" ON DELETE {}", action.sql()));
        }
        sql
    }

    fn drop_fk_sql(&self, table: &str, name: &str) -> String {
        format!(
            "ALTER TABLE {table} DROP CONSTRAINT {}",
            self.quote_ident(name)
        )
    }

    /// A native `ALTER COLUMN` for a type/nullability/default change, or
    /// `None` when this engine cannot express it (SQLite: the caller expands
    /// a rebuild instead). `current` is the column as it stands; only the
    /// `Some` fields change.
    fn alter_column_sql(
        &self,
        _table: &str,
        _current: &ColumnInfo,
        _new_type: Option<&str>,
        _nullable: Option<bool>,
        _default: Option<Option<&str>>,
    ) -> Option<String> {
        None
    }
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn quote_with(ident: &str, q: char) -> String {
    let escaped = ident.replace(q, &format!("{q}{q}"));
    format!("{q}{escaped}{q}")
}

pub struct SqliteDialect;
pub struct PostgresDialect;
pub struct MySqlDialect;

impl Dialect for SqliteDialect {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }
    fn quote_ident(&self, ident: &str) -> String {
        quote_with(ident, '"')
    }
    fn placeholder(&self, _index: usize) -> String {
        "?".into()
    }
    fn default_port(&self) -> Option<u16> {
        None
    }
    fn supports_default_keyword(&self) -> bool {
        false
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn supports_table_rebuild(&self) -> bool {
        true
    }

    fn integer_primary_key_autoincrement(&self) -> bool {
        true
    }
}

impl Dialect for PostgresDialect {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }
    fn quote_ident(&self, ident: &str) -> String {
        quote_with(ident, '"')
    }
    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }
    fn default_port(&self) -> Option<u16> {
        Some(5432)
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn bytes_literal(&self, bytes: &[u8]) -> String {
        format!(
            "'\\x{}'",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    }

    fn drop_index_sql(&self, path: &ObjectPath, name: &str) -> String {
        // An unqualified DROP INDEX only finds indexes on the search path;
        // qualify with the table's schema when there is one.
        match &path.namespace {
            Some(ns) => format!(
                "DROP INDEX {}.{}",
                self.quote_ident(ns),
                self.quote_ident(name)
            ),
            None => format!("DROP INDEX {}", self.quote_ident(name)),
        }
    }

    fn alter_column_sql(
        &self,
        table: &str,
        current: &ColumnInfo,
        new_type: Option<&str>,
        nullable: Option<bool>,
        default: Option<Option<&str>>,
    ) -> Option<String> {
        let col = self.quote_ident(&current.name);
        let mut actions = Vec::new();
        if let Some(t) = new_type {
            // No USING clause: a cast PostgreSQL cannot do implicitly fails
            // at apply time with the engine's own error, which is clearer
            // than guessing a cast here.
            actions.push(format!("ALTER COLUMN {col} TYPE {t}"));
        }
        match nullable {
            Some(true) => actions.push(format!("ALTER COLUMN {col} DROP NOT NULL")),
            Some(false) => actions.push(format!("ALTER COLUMN {col} SET NOT NULL")),
            None => {}
        }
        match default {
            Some(Some(d)) => actions.push(format!("ALTER COLUMN {col} SET DEFAULT {d}")),
            Some(None) => actions.push(format!("ALTER COLUMN {col} DROP DEFAULT")),
            None => {}
        }
        if actions.is_empty() {
            None
        } else {
            Some(format!("ALTER TABLE {table} {}", actions.join(", ")))
        }
    }
}

impl Dialect for MySqlDialect {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }
    fn quote_ident(&self, ident: &str) -> String {
        quote_with(ident, '`')
    }
    fn placeholder(&self, _index: usize) -> String {
        "?".into()
    }
    fn default_port(&self) -> Option<u16> {
        Some(3306)
    }
    fn supports_nulls_ordering(&self) -> bool {
        false
    }
    fn supports_partial_index(&self) -> bool {
        false
    }
    fn column_def_sql(&self, col: &ColumnInfo) -> String {
        let mut def = format!("{} {}", self.quote_ident(&col.name), col.data_type.native);
        if !col.nullable {
            def.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default {
            def.push_str(&format!(" DEFAULT {default}"));
        }
        if col.auto_increment {
            def.push_str(" AUTO_INCREMENT");
        }
        def
    }
    fn rename_table_sql(&self, path: &ObjectPath, new_name: &str) -> String {
        let new_path = ObjectPath::new(path.namespace.as_deref(), new_name);
        format!(
            "RENAME TABLE {} TO {}",
            self.qualified_name(path),
            self.qualified_name(&new_path)
        )
    }
    fn drop_index_sql(&self, path: &ObjectPath, name: &str) -> String {
        format!(
            "DROP INDEX {} ON {}",
            self.quote_ident(name),
            self.qualified_name(path)
        )
    }
    fn drop_fk_sql(&self, table: &str, name: &str) -> String {
        format!(
            "ALTER TABLE {table} DROP FOREIGN KEY {}",
            self.quote_ident(name)
        )
    }
    fn alter_column_sql(
        &self,
        table: &str,
        current: &ColumnInfo,
        new_type: Option<&str>,
        nullable: Option<bool>,
        default: Option<Option<&str>>,
    ) -> Option<String> {
        // MODIFY COLUMN redefines the column in full, so merge the change
        // into the current definition first.
        let mut merged = current.clone();
        if let Some(t) = new_type {
            merged.data_type.native = t.to_owned();
        }
        if let Some(nullable) = nullable {
            merged.nullable = nullable;
        }
        if let Some(default) = default {
            merged.default = default.map(str::to_owned);
        }
        Some(format!(
            "ALTER TABLE {table} MODIFY COLUMN {}",
            self.column_def_sql(&merged)
        ))
    }
    fn last_insert_id_sql(&self) -> Option<&str> {
        Some("SELECT LAST_INSERT_ID()")
    }
    fn insert_defaults(&self, table: &str) -> String {
        format!("INSERT INTO {table} () VALUES ()")
    }
    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS CHAR)")
    }
}

/// `None` for engines this build does not know (`DbKind::Unknown`).
pub fn dialect_for(kind: DbKind) -> Option<&'static dyn Dialect> {
    match kind {
        DbKind::Sqlite => Some(&SqliteDialect),
        DbKind::Postgres => Some(&PostgresDialect),
        DbKind::MySql => Some(&MySqlDialect),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_the_quote_char() {
        assert_eq!(SqliteDialect.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(MySqlDialect.quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn qualified_names_and_placeholders() {
        let p = ObjectPath::new(Some("public"), "users");
        assert_eq!(PostgresDialect.qualified_name(&p), "\"public\".\"users\"");
        assert_eq!(PostgresDialect.placeholder(2), "$2");
        assert_eq!(MySqlDialect.placeholder(2), "?");
        assert_eq!(
            SqliteDialect.qualified_name(&ObjectPath::new(None, "t")),
            "\"t\""
        );
    }

    #[test]
    fn limit_clause_omits_zero_offset() {
        assert_eq!(SqliteDialect.limit_clause(10, 0), "LIMIT 10");
        assert_eq!(SqliteDialect.limit_clause(10, 20), "LIMIT 10 OFFSET 20");
    }

    #[test]
    fn unknown_engines_have_no_dialect() {
        assert!(dialect_for(DbKind::Unknown).is_none());
        assert_eq!(
            dialect_for(DbKind::Postgres).map(|d| d.kind()),
            Some(DbKind::Postgres)
        );
    }
}
