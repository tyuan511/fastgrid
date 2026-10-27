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

    /// The sort key that separates NULLs from values on engines without
    /// `NULLS FIRST/LAST`: `1` for NULL, `0` otherwise, so the caller appends
    /// `DESC` to sort NULLs first and `ASC` to sort them last. SQL Server has
    /// no boolean type in expressions, so `x IS NULL` cannot stand alone
    /// there and it overrides this with a `CASE`.
    fn null_sort_key(&self, expr: &str) -> String {
        format!("({expr} IS NULL)")
    }

    /// Whether `OFFSET n ROWS FETCH NEXT m ROWS ONLY` needs an `ORDER BY` to
    /// be legal. SQL Server rejects the clause without one, so a query with
    /// no sort keys must fall back to a constant ordering.
    fn limit_requires_order_by(&self) -> bool {
        false
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

    /// The statement that opens a transaction. Bare `BEGIN` is not valid
    /// T-SQL, and engines differ on the spelling, so every engine that
    /// supports transactions answers here instead of the callers hardcoding
    /// one spelling for all of them.
    fn begin_transaction_sql(&self) -> &str {
        "BEGIN"
    }

    /// The statement that commits the open transaction.
    fn commit_sql(&self) -> &str {
        "COMMIT"
    }

    /// The statement that rolls the open transaction back.
    fn rollback_sql(&self) -> &str {
        "ROLLBACK"
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
        if self.supports_index_method()
            && let Some(method) = &index.method
        {
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

    /// Whether `CREATE INDEX ... USING method` is valid. SQL Server has no
    /// access-method clause; its index kind is a separate keyword.
    fn supports_index_method(&self) -> bool {
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

    /// The same change as one or more statements. Engines that keep a column
    /// default as a separate named constraint need two (drop the old one,
    /// add the new one), so the schema compiler asks for a list.
    fn alter_column_sqls(
        &self,
        table: &str,
        current: &ColumnInfo,
        new_type: Option<&str>,
        nullable: Option<bool>,
        default: Option<Option<&str>>,
    ) -> Option<Vec<String>> {
        self.alter_column_sql(table, current, new_type, nullable, default)
            .map(|sql| vec![sql])
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
pub struct MsSqlDialect;

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
    fn begin_transaction_sql(&self) -> &str {
        "START TRANSACTION"
    }
    fn insert_defaults(&self, table: &str) -> String {
        format!("INSERT INTO {table} () VALUES ()")
    }
    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS CHAR)")
    }
}

/// T-SQL: SQL Server. Bracketed identifiers, `@Pn` placeholders, `OFFSET/FETCH`
/// paging that requires an `ORDER BY`, and schema changes expressed through
/// `ALTER COLUMN`, `EXEC sp_rename` and named default constraints.
impl Dialect for MsSqlDialect {
    fn kind(&self) -> DbKind {
        DbKind::MsSql
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("[{}]", ident.replace(']', "]]"))
    }

    fn placeholder(&self, index: usize) -> String {
        format!("@P{index}")
    }

    fn default_port(&self) -> Option<u16> {
        Some(1433)
    }

    fn limit_clause(&self, limit: u64, offset: u64) -> String {
        format!("OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY")
    }

    fn limit_requires_order_by(&self) -> bool {
        true
    }

    /// T-SQL has no `NULLS FIRST/LAST` clause.
    fn supports_nulls_ordering(&self) -> bool {
        false
    }

    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS NVARCHAR(MAX))")
    }

    fn null_sort_key(&self, expr: &str) -> String {
        // `x IS NULL` is not a value in T-SQL, so it cannot be a sort key.
        format!("CASE WHEN {expr} IS NULL THEN 1 ELSE 0 END")
    }

    /// `OUTPUT INSERTED.*` exists but sits before `VALUES`, which the DML
    /// compiler cannot express by appending a clause.
    fn supports_returning(&self) -> bool {
        false
    }

    fn last_insert_id_sql(&self) -> Option<&str> {
        // `@@IDENTITY` is session-scoped, so it survives into the separate
        // statement the commit path runs to read the key back. SCOPE_IDENTITY
        // would be NULL there (the insert's scope has ended), and it would be
        // NULL too if the insert were wrapped in sp_executesql. Triggers that
        // insert elsewhere can skew this, which is why an explicitly supplied
        // key always takes precedence over it.
        Some("SELECT CAST(@@IDENTITY AS BIGINT)")
    }

    fn begin_transaction_sql(&self) -> &str {
        // Bare `BEGIN` opens a statement block in T-SQL; the transaction needs
        // the keyword spelled out (measured: `Msg 102 ... near 'BEGIN'`).
        "BEGIN TRANSACTION"
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            // T-SQL has no boolean literals.
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            Value::Bytes(b) => self.bytes_literal(b),
            // SQL Server has no array type; the default rendering would emit
            // PostgreSQL's `ARRAY[...]`, which is not valid here.
            Value::Array(_) => "NULL".into(),
            other => self.literal_text(other),
        }
    }

    fn bytes_literal(&self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return "0x".into();
        }
        format!(
            "0x{}",
            bytes.iter().map(|b| format!("{b:02X}")).collect::<String>()
        )
    }

    fn column_def_sql(&self, col: &ColumnInfo) -> String {
        let mut def = format!("{} {}", self.quote_ident(&col.name), col.data_type.native);
        if col.auto_increment && !col.data_type.native.contains("IDENTITY") {
            def.push_str(" IDENTITY(1,1)");
        }
        if !col.nullable {
            def.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default {
            def.push_str(&format!(" DEFAULT {default}"));
        }
        def
    }

    fn supports_index_method(&self) -> bool {
        false
    }

    /// `DROP INDEX name ON table`, qualified with the schema.
    fn drop_index_sql(&self, path: &ObjectPath, name: &str) -> String {
        format!(
            "DROP INDEX {} ON {}",
            self.quote_ident(name),
            self.qualified_name(path)
        )
    }

    fn rename_table_sql(&self, path: &ObjectPath, new_name: &str) -> String {
        // sp_rename takes the current name as a string literal, so the
        // bracketed form has to be escaped into it.
        format!(
            "EXEC sp_rename {}, {}",
            sp_arg(&self.qualified_name(path)),
            sp_arg(new_name)
        )
    }

    fn rename_column_sql(&self, table: &str, from: &str, to: &str) -> String {
        format!(
            "EXEC sp_rename {}, {}, 'COLUMN'",
            sp_arg(&format!("{table}.{}", self.quote_ident(from))),
            sp_arg(to)
        )
    }

    /// `ALTER COLUMN` cannot carry a default in T-SQL, so the type and
    /// nullability go in one statement and the default is a named constraint
    /// that has to be dropped and re-added.
    fn alter_column_sqls(
        &self,
        table: &str,
        current: &ColumnInfo,
        new_type: Option<&str>,
        nullable: Option<bool>,
        default: Option<Option<&str>>,
    ) -> Option<Vec<String>> {
        let col = self.quote_ident(&current.name);
        let mut out = Vec::new();
        let type_changed = new_type.is_some_and(|t| t != current.data_type.native);
        let null_changed = nullable.is_some_and(|n| n != current.nullable);
        if type_changed || null_changed {
            // ALTER COLUMN restates the whole type, so keep the current one
            // when only nullability changes.
            let ty = new_type.unwrap_or(&current.data_type.native);
            let nullable = nullable.unwrap_or(current.nullable);
            let nullability = if nullable { " NULL" } else { " NOT NULL" };
            out.push(format!(
                "ALTER TABLE {table} ALTER COLUMN {col} {ty}{nullability}"
            ));
        }
        if let Some(default) = default {
            // A default is a separately named object here, so the old one has
            // to go first. Its name comes from the catalogue; without it the
            // change cannot be expressed, and the caller reports it rather
            // than emitting a statement that would fail.
            if current.default.is_some() {
                let name = current.extra.get(DEFAULT_CONSTRAINT_KEY)?;
                out.push(format!(
                    "ALTER TABLE {table} DROP CONSTRAINT {}",
                    self.quote_ident(name)
                ));
            }
            if let Some(expr) = default {
                // Unnamed, so the engine picks the constraint name; the next
                // read of the catalogue reports it.
                out.push(format!("ALTER TABLE {table} ADD DEFAULT {expr} FOR {col}"));
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// Key in [`ColumnInfo::extra`] holding the name of the column's default
/// constraint. SQL Server keeps defaults as separately named objects, so
/// changing one means dropping the constraint the catalogue reported.
pub const DEFAULT_CONSTRAINT_KEY: &str = "default_constraint";

/// A string literal for a stored-procedure argument.
fn sp_arg(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

impl MsSqlDialect {
    /// Everything but booleans, bytes and arrays, which [`Self::literal`]
    /// handles because T-SQL spells them differently.
    fn literal_text(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Float(_) => "NULL".into(),
            Value::Decimal(s) if s.parse::<f64>().is_ok() => s.clone(),
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Json(s)
            | Value::Uuid(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::Timestamp(s)
            | Value::TimestampTz(s)
            | Value::Interval(s) => quote_literal(s),
            _ => "NULL".into(),
        }
    }
}

/// `None` for engines this build does not know (`DbKind::Unknown`).
pub fn dialect_for(kind: DbKind) -> Option<&'static dyn Dialect> {
    match kind {
        DbKind::Sqlite => Some(&SqliteDialect),
        DbKind::Postgres => Some(&PostgresDialect),
        DbKind::MySql => Some(&MySqlDialect),
        DbKind::MsSql => Some(&MsSqlDialect),
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

    #[test]
    fn sql_server_brackets_identifiers_and_numbers_parameters() {
        assert_eq!(MsSqlDialect.quote_ident("a]b"), "[a]]b]");
        assert_eq!(MsSqlDialect.placeholder(3), "@P3");
        assert_eq!(MsSqlDialect.default_port(), Some(1433));
        assert_eq!(
            MsSqlDialect.qualified_name(&ObjectPath::new(Some("dbo"), "users")),
            "[dbo].[users]"
        );
        assert_eq!(
            MsSqlDialect.limit_clause(10, 20),
            "OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        assert!(MsSqlDialect.limit_requires_order_by());
        assert!(!MsSqlDialect.supports_nulls_ordering());
    }

    #[test]
    fn sql_server_literals_avoid_boolean_and_array_syntax() {
        assert_eq!(MsSqlDialect.literal(&Value::Bool(true)), "1");
        assert_eq!(MsSqlDialect.literal(&Value::Bool(false)), "0");
        assert_eq!(
            MsSqlDialect.literal(&Value::Bytes(vec![0xde, 0xad])),
            "0xDEAD"
        );
        assert_eq!(MsSqlDialect.literal(&Value::Bytes(Vec::new())), "0x");
        assert_eq!(
            MsSqlDialect.literal(&Value::Text("o'brien".into())),
            "'o''brien'"
        );
        assert_eq!(MsSqlDialect.literal(&Value::Null), "NULL");
        // An array cannot round-trip into T-SQL; NULL is the honest fallback.
        assert_eq!(
            MsSqlDialect.literal(&Value::Array(vec![Value::Int(1)])),
            "NULL"
        );
    }

    #[test]
    fn sql_server_renames_go_through_sp_rename() {
        let path = ObjectPath::new(Some("dbo"), "t");
        assert_eq!(
            MsSqlDialect.rename_table_sql(&path, "t2"),
            "EXEC sp_rename N'[dbo].[t]', N't2'"
        );
        assert_eq!(
            MsSqlDialect.rename_column_sql("[dbo].[t]", "a", "b"),
            "EXEC sp_rename N'[dbo].[t].[a]', N'b', 'COLUMN'"
        );
        // A quote in a name must not break out of the literal.
        assert_eq!(
            MsSqlDialect.rename_table_sql(&path, "o'brien"),
            "EXEC sp_rename N'[dbo].[t]', N'o''brien'"
        );
    }

    #[test]
    fn sql_server_index_drops_name_the_table_and_skip_the_method_clause() {
        assert!(!MsSqlDialect.supports_index_method());
        let path = ObjectPath::new(Some("dbo"), "t");
        assert_eq!(
            MsSqlDialect.drop_index_sql(&path, "ix"),
            "DROP INDEX [ix] ON [dbo].[t]"
        );
        let index = crate::IndexInfo {
            name: "ix".into(),
            columns: vec![crate::IndexColumn {
                name: "a".into(),
                expression: false,
                descending: true,
            }],
            unique: false,
            primary: false,
            method: Some("btree".into()),
            predicate: Some("a > 0".into()),
            comment: None,
            extra: Default::default(),
        };
        assert_eq!(
            MsSqlDialect.create_index_sql("[dbo].[t]", &index),
            "CREATE INDEX [ix] ON [dbo].[t] ([a] DESC) WHERE a > 0"
        );
    }

    #[test]
    fn sql_server_alter_column_splits_type_from_default() {
        let mut col = crate::ColumnInfo {
            name: "a".into(),
            ordinal: 1,
            data_type: crate::DataType::new("int", crate::TypeCategory::Integer),
            nullable: true,
            default: Some("0".into()),
            primary_key_position: None,
            auto_increment: false,
            generated: None,
            collation: None,
            comment: None,
            extra: Default::default(),
        };
        // A type change alone: one statement, restating nullability.
        assert_eq!(
            MsSqlDialect.alter_column_sqls("[dbo].[t]", &col, Some("bigint"), None, None),
            Some(vec![
                "ALTER TABLE [dbo].[t] ALTER COLUMN [a] bigint NULL".into()
            ])
        );
        // A default change needs the old constraint dropped first, and its
        // name is only known from the catalogue.
        col.extra.insert(
            crate::dialect::DEFAULT_CONSTRAINT_KEY.into(),
            "DF_t_a".into(),
        );
        assert_eq!(
            MsSqlDialect.alter_column_sqls("[dbo].[t]", &col, None, None, Some(Some("7"))),
            Some(vec![
                "ALTER TABLE [dbo].[t] DROP CONSTRAINT [DF_t_a]".to_string(),
                "ALTER TABLE [dbo].[t] ADD DEFAULT 7 FOR [a]".to_string()
            ])
        );
        // Dropping the default without replacing it is one statement.
        assert_eq!(
            MsSqlDialect.alter_column_sqls("[dbo].[t]", &col, None, None, Some(None)),
            Some(vec![
                "ALTER TABLE [dbo].[t] DROP CONSTRAINT [DF_t_a]".to_string()
            ])
        );
        // A default exists but the catalogue did not report its name: refuse
        // rather than emit a statement that cannot run.
        let mut unnamed = col.clone();
        unnamed.extra.clear();
        assert_eq!(
            MsSqlDialect.alter_column_sqls("[dbo].[t]", &unnamed, None, None, Some(Some("7"))),
            None
        );
        // No change at all means no statement.
        assert_eq!(
            MsSqlDialect.alter_column_sqls("[dbo].[t]", &col, Some("int"), Some(true), None),
            None
        );
    }

    #[test]
    fn sql_server_declares_identity_inline() {
        let col = crate::ColumnInfo {
            name: "id".into(),
            ordinal: 1,
            data_type: crate::DataType::new("int", crate::TypeCategory::Integer),
            nullable: false,
            default: None,
            primary_key_position: Some(1),
            auto_increment: true,
            generated: None,
            collation: None,
            comment: None,
            extra: Default::default(),
        };
        assert_eq!(
            MsSqlDialect.column_def_sql(&col),
            "[id] int IDENTITY(1,1) NOT NULL"
        );
        assert!(!MsSqlDialect.integer_primary_key_autoincrement());
    }
}
