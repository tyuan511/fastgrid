use crate::{DbKind, ObjectPath};

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

    /// An `INSERT` that sets no column: every column takes its default.
    fn insert_defaults(&self, table: &str) -> String {
        format!("INSERT INTO {table} DEFAULT VALUES")
    }
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
