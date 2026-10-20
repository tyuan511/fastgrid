//! Compile a declarative [`TableQuery`] (what the grid, saved filters and MCP produce) into
//! dialect-specific SQL with bound parameters.
//!
//! Values and identifiers never get spliced into the SQL text: values are parameters, identifiers
//! are quoted by the [`Dialect`]. The one exception is the user-typed raw `WHERE` (the filter
//! drawer's Raw SQL mode), which is validated to be a single expression and can be refused
//! entirely with [`BuildOptions::allow_raw_where`] (agents never get it).

use fg_core::{Dialect, Error, Filter, FilterOp, NullsOrder, Result, TableQuery, Value};

use crate::lex::{Flavor, Kind, lex};

/// LIKE escape character. A letter-free symbol that is not special to any of the three engines'
/// string literals (unlike backslash).
const LIKE_ESCAPE: char = '!';

#[derive(Debug, Clone, PartialEq)]
pub struct BuiltQuery {
    pub sql: String,
    pub params: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Columns appended to `ORDER BY` (normally the primary key) so offset paging is stable.
    pub tie_breaker: Vec<String>,
    /// Accept `TableQuery::raw_where`. Off for anything driven by an agent.
    pub allow_raw_where: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            tie_breaker: Vec::new(),
            allow_raw_where: true,
        }
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

struct Binder<'a> {
    dialect: &'a dyn Dialect,
    params: Vec<Value>,
}

impl Binder<'_> {
    fn bind(&mut self, v: Value) -> String {
        self.params.push(v);
        self.dialect.placeholder(self.params.len())
    }
}

pub fn build_select(
    q: &TableQuery,
    dialect: &dyn Dialect,
    options: &BuildOptions,
) -> Result<BuiltQuery> {
    if q.limit == 0 {
        return Err(invalid("limit must be at least 1"));
    }
    let mut binder = Binder {
        dialect,
        params: Vec::new(),
    };
    let columns = match &q.columns {
        None => "*".to_owned(),
        Some(cols) if cols.is_empty() => return Err(invalid("column list is empty")),
        Some(cols) => cols
            .iter()
            .map(|c| dialect.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let mut sql = format!("SELECT {columns} FROM {}", dialect.qualified_name(&q.path));
    if let Some(w) = where_clause(q, options, &mut binder)? {
        sql.push_str(" WHERE ");
        sql.push_str(&w);
    }
    if let Some(order) = order_by(q, dialect, &options.tie_breaker) {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order);
    }
    sql.push(' ');
    sql.push_str(&dialect.limit_clause(q.limit, q.offset));
    Ok(BuiltQuery {
        sql,
        params: binder.params,
    })
}

/// `SELECT COUNT(*)` for the same filters (paging, sorting and column list are ignored).
pub fn build_count(
    q: &TableQuery,
    dialect: &dyn Dialect,
    options: &BuildOptions,
) -> Result<BuiltQuery> {
    let mut binder = Binder {
        dialect,
        params: Vec::new(),
    };
    let mut sql = format!("SELECT COUNT(*) FROM {}", dialect.qualified_name(&q.path));
    if let Some(w) = where_clause(q, options, &mut binder)? {
        sql.push_str(" WHERE ");
        sql.push_str(&w);
    }
    Ok(BuiltQuery {
        sql,
        params: binder.params,
    })
}

fn where_clause(
    q: &TableQuery,
    options: &BuildOptions,
    b: &mut Binder<'_>,
) -> Result<Option<String>> {
    let mut parts = Vec::new();
    for f in q.filters.iter().filter(|f| f.enabled) {
        parts.push(filter_sql(f, b)?);
    }
    if let Some(raw) = q
        .raw_where
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !options.allow_raw_where {
            return Err(Error::PolicyDenied(
                "raw WHERE clauses are not allowed here".into(),
            ));
        }
        validate_raw_where(raw, b.dialect)?;
        parts.push(format!("({raw})"));
    }
    Ok((!parts.is_empty()).then(|| parts.join(" AND ")))
}

fn filter_sql(f: &Filter, b: &mut Binder<'_>) -> Result<String> {
    let col = b.dialect.quote_ident(&f.column);
    let n = f.values.len();
    match f.op.arity() {
        Some(want) if n != want => {
            return Err(invalid(format!(
                "filter on {:?} ({:?}) needs {want} value(s), got {n}",
                f.column, f.op
            )));
        }
        None if n == 0 => {
            return Err(invalid(format!(
                "filter on {:?} ({:?}) needs a value",
                f.column, f.op
            )));
        }
        _ => {}
    }
    let compare = |b: &mut Binder<'_>, op: &str| {
        let ph = b.bind(f.values[0].clone());
        format!("{col} {op} {ph}")
    };
    Ok(match f.op {
        // `= NULL` is never true: mean what the user meant.
        FilterOp::Eq if f.values[0].is_null() => format!("{col} IS NULL"),
        FilterOp::Ne if f.values[0].is_null() => format!("{col} IS NOT NULL"),
        FilterOp::Eq => compare(b, "="),
        FilterOp::Ne => compare(b, "<>"),
        FilterOp::Lt => compare(b, "<"),
        FilterOp::Le => compare(b, "<="),
        FilterOp::Gt => compare(b, ">"),
        FilterOp::Ge => compare(b, ">="),
        FilterOp::Like => like(b, &col, false, f.values[0].clone(), false)?,
        FilterOp::NotLike => like(b, &col, true, f.values[0].clone(), false)?,
        FilterOp::Contains => like(b, &col, false, wildcard(&f.values[0], true, true)?, true)?,
        FilterOp::NotContains => like(b, &col, true, wildcard(&f.values[0], true, true)?, true)?,
        FilterOp::StartsWith => like(b, &col, false, wildcard(&f.values[0], false, true)?, true)?,
        FilterOp::EndsWith => like(b, &col, false, wildcard(&f.values[0], true, false)?, true)?,
        FilterOp::In | FilterOp::NotIn => {
            let list = f
                .values
                .iter()
                .map(|v| b.bind(v.clone()))
                .collect::<Vec<_>>()
                .join(", ");
            let not = if f.op == FilterOp::NotIn { "NOT " } else { "" };
            format!("{col} {not}IN ({list})")
        }
        FilterOp::Between => {
            let (lo, hi) = (b.bind(f.values[0].clone()), b.bind(f.values[1].clone()));
            format!("{col} BETWEEN {lo} AND {hi}")
        }
        FilterOp::IsNull => format!("{col} IS NULL"),
        FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
        _ => {
            return Err(invalid(format!(
                "unsupported filter operator on {:?}",
                f.column
            )));
        }
    })
}

/// `col LIKE ?`, casting the column to text first so it works on numbers and dates too.
fn like(
    b: &mut Binder<'_>,
    col: &str,
    negate: bool,
    pattern: Value,
    escaped: bool,
) -> Result<String> {
    let ph = b.bind(pattern);
    let target = b.dialect.text_cast(col);
    let not = if negate { "NOT " } else { "" };
    let escape = if escaped {
        format!(" ESCAPE '{LIKE_ESCAPE}'")
    } else {
        String::new()
    };
    Ok(format!("{target} {not}LIKE {ph}{escape}"))
}

/// `%` + escaped text + `%`, so `50%` searches for the literal text.
fn wildcard(v: &Value, leading: bool, trailing: bool) -> Result<Value> {
    let Value::Text(text) = v else {
        return Err(invalid("text filters need a text value"));
    };
    let mut out = String::new();
    if leading {
        out.push('%');
    }
    for c in text.chars() {
        if matches!(c, '%' | '_' | LIKE_ESCAPE) {
            out.push(LIKE_ESCAPE);
        }
        out.push(c);
    }
    if trailing {
        out.push('%');
    }
    Ok(Value::Text(out))
}

fn order_by(q: &TableQuery, d: &dyn Dialect, tie_breaker: &[String]) -> Option<String> {
    let mut keys = Vec::new();
    for k in &q.sort {
        let col = d.quote_ident(&k.column);
        let dir = if k.descending { "DESC" } else { "ASC" };
        match (k.nulls, d.supports_nulls_ordering()) {
            (Some(NullsOrder::First), true) => keys.push(format!("{col} {dir} NULLS FIRST")),
            (Some(NullsOrder::Last), true) => keys.push(format!("{col} {dir} NULLS LAST")),
            // MySQL: sort on "is null" first (1 for NULL), then on the value.
            (Some(NullsOrder::First), false) => {
                keys.push(format!("({col} IS NULL) DESC"));
                keys.push(format!("{col} {dir}"));
            }
            (Some(NullsOrder::Last), false) => {
                keys.push(format!("({col} IS NULL) ASC"));
                keys.push(format!("{col} {dir}"));
            }
            _ => keys.push(format!("{col} {dir}")),
        }
    }
    for pk in tie_breaker {
        if !q.sort.iter().any(|k| &k.column == pk) {
            keys.push(format!("{} ASC", d.quote_ident(pk)));
        }
    }
    (!keys.is_empty()).then(|| keys.join(", "))
}

/// Words that cannot appear at the top level of a `WHERE` expression: they would start another
/// clause or statement.
const FORBIDDEN_TOP_LEVEL: &[&str] = &[
    "SELECT",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "ORDER",
    "GROUP",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "FOR",
    "INTO",
    "WINDOW",
    "INSERT",
    "UPDATE",
    "DELETE",
    "DROP",
    "CREATE",
    "ALTER",
    "TRUNCATE",
    "REPLACE",
    "MERGE",
    "CALL",
    "EXECUTE",
    "PRAGMA",
    "ATTACH",
    "SET",
];

/// A raw filter must be one boolean expression: no `;`, no comments (a comment could swallow the
/// closing parenthesis it is wrapped in), balanced parentheses, nothing that starts a new clause.
pub fn validate_raw_where(expr: &str, dialect: &dyn Dialect) -> Result<()> {
    let flavor = Flavor::from(dialect.kind());
    let mut depth = 0usize;
    for t in lex(expr, flavor) {
        if t.unterminated {
            return Err(invalid(
                "unterminated string, identifier or comment in the filter",
            ));
        }
        match t.kind {
            Kind::LineComment | Kind::BlockComment | Kind::ExecCommentMark => {
                return Err(invalid("comments are not allowed in a filter"));
            }
            Kind::Punct if t.is_punct(expr, ';') => {
                return Err(invalid("a filter must be a single expression (found ';')"));
            }
            Kind::Punct if t.is_punct(expr, '(') => depth += 1,
            Kind::Punct if t.is_punct(expr, ')') => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| invalid("unbalanced ')' in the filter"))?;
            }
            Kind::Word if depth == 0 => {
                let w = t.text(expr).to_ascii_uppercase();
                if FORBIDDEN_TOP_LEVEL.contains(&w.as_str()) {
                    return Err(invalid(format!(
                        "{w} is not allowed at the top level of a filter"
                    )));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(invalid("unbalanced '(' in the filter"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use fg_core::{MySqlDialect, ObjectPath, PostgresDialect, SortKey, SqliteDialect};

    use super::*;

    fn filter(col: &str, op: FilterOp, values: Vec<Value>) -> Filter {
        Filter {
            column: col.into(),
            op,
            values,
            enabled: true,
        }
    }

    fn q() -> TableQuery {
        TableQuery::new(ObjectPath::new(Some("public"), "users"), 50)
    }

    fn build(q: &TableQuery, d: &dyn Dialect) -> BuiltQuery {
        build_select(q, d, &BuildOptions::default()).unwrap()
    }

    #[test]
    fn plain_table_scan_per_dialect() {
        assert_eq!(
            build(&q(), &SqliteDialect).sql,
            r#"SELECT * FROM "public"."users" LIMIT 50"#
        );
        assert_eq!(
            build(&q(), &MySqlDialect).sql,
            "SELECT * FROM `public`.`users` LIMIT 50"
        );
        let mut paged = q();
        paged.offset = 100;
        assert_eq!(
            build(&paged, &PostgresDialect).sql,
            r#"SELECT * FROM "public"."users" LIMIT 50 OFFSET 100"#
        );
    }

    #[test]
    fn filters_bind_values_with_the_dialects_placeholders() {
        let mut query = q();
        query.filters = vec![
            filter("age", FilterOp::Ge, vec![Value::Int(18)]),
            filter(
                "role",
                FilterOp::In,
                vec![Value::Text("a".into()), Value::Text("b".into())],
            ),
            filter(
                "score",
                FilterOp::Between,
                vec![Value::Int(1), Value::Int(9)],
            ),
        ];
        let pg = build(&query, &PostgresDialect);
        assert_eq!(
            pg.sql,
            r#"SELECT * FROM "public"."users" WHERE "age" >= $1 AND "role" IN ($2, $3) AND "score" BETWEEN $4 AND $5 LIMIT 50"#
        );
        assert_eq!(pg.params.len(), 5);
        let lite = build(&query, &SqliteDialect);
        assert!(lite.sql.contains(r#""age" >= ? AND "role" IN (?, ?)"#));
    }

    #[test]
    fn disabled_filters_are_skipped_and_values_never_reach_the_sql_text() {
        let mut query = q();
        let mut off = filter("a", FilterOp::Eq, vec![Value::Int(1)]);
        off.enabled = false;
        query.filters = vec![
            off,
            filter(
                "name",
                FilterOp::Eq,
                vec![Value::Text("x'; DROP TABLE users; --".into())],
            ),
        ];
        let b = build(&query, &SqliteDialect);
        assert!(!b.sql.contains("DROP"));
        assert_eq!(
            b.params,
            vec![Value::Text("x'; DROP TABLE users; --".into())]
        );
    }

    #[test]
    fn null_comparisons_mean_is_null() {
        let mut query = q();
        query.filters = vec![
            filter("a", FilterOp::Eq, vec![Value::Null]),
            filter("b", FilterOp::Ne, vec![Value::Null]),
            filter("c", FilterOp::IsNull, vec![]),
            filter("d", FilterOp::IsNotNull, vec![]),
        ];
        let b = build(&query, &SqliteDialect);
        assert!(
            b.sql
                .contains(r#""a" IS NULL AND "b" IS NOT NULL AND "c" IS NULL AND "d" IS NOT NULL"#)
        );
        assert!(b.params.is_empty());
    }

    #[test]
    fn text_filters_escape_wildcards_and_cast_to_text() {
        let mut query = q();
        query.filters = vec![
            filter(
                "note",
                FilterOp::Contains,
                vec![Value::Text("50%_off!".into())],
            ),
            filter("name", FilterOp::StartsWith, vec![Value::Text("ab".into())]),
        ];
        let pg = build(&query, &PostgresDialect);
        assert_eq!(
            pg.sql,
            r#"SELECT * FROM "public"."users" WHERE CAST("note" AS TEXT) LIKE $1 ESCAPE '!' AND CAST("name" AS TEXT) LIKE $2 ESCAPE '!' LIMIT 50"#
        );
        assert_eq!(pg.params[0], Value::Text("%50!%!_off!!%".into()));
        assert_eq!(pg.params[1], Value::Text("ab%".into()));
        assert!(
            build(&query, &MySqlDialect)
                .sql
                .contains("CAST(`note` AS CHAR) LIKE ?")
        );
    }

    #[test]
    fn identifiers_are_quoted_and_escaped() {
        let mut query = TableQuery::new(ObjectPath::new(None, "we\"ird"), 1);
        query.columns = Some(vec!["a\"b".into()]);
        query.sort = vec![SortKey {
            column: "x\"y".into(),
            descending: true,
            nulls: None,
        }];
        assert_eq!(
            build(&query, &SqliteDialect).sql,
            r#"SELECT "a""b" FROM "we""ird" ORDER BY "x""y" DESC LIMIT 1"#
        );
    }

    #[test]
    fn ordering_with_tie_breaker_and_null_placement() {
        let mut query = q();
        query.sort = vec![SortKey {
            column: "score".into(),
            descending: true,
            nulls: Some(NullsOrder::Last),
        }];
        let opts = BuildOptions {
            tie_breaker: vec!["id".into(), "score".into()],
            ..BuildOptions::default()
        };
        let pg = build_select(&query, &PostgresDialect, &opts).unwrap();
        assert!(
            pg.sql
                .ends_with(r#"ORDER BY "score" DESC NULLS LAST, "id" ASC LIMIT 50"#),
            "{}",
            pg.sql
        );
        let my = build_select(&query, &MySqlDialect, &opts).unwrap();
        assert!(
            my.sql
                .contains("ORDER BY (`score` IS NULL) ASC, `score` DESC, `id` ASC"),
            "{}",
            my.sql
        );
        // No explicit sort: the tie-breaker alone gives stable paging.
        let stable = build_select(
            &q(),
            &SqliteDialect,
            &BuildOptions {
                tie_breaker: vec!["id".into()],
                ..BuildOptions::default()
            },
        )
        .unwrap();
        assert!(stable.sql.contains(r#"ORDER BY "id" ASC"#));
    }

    #[test]
    fn count_ignores_paging_and_sorting() {
        let mut query = q();
        query.offset = 500;
        query.sort = vec![SortKey {
            column: "a".into(),
            descending: false,
            nulls: None,
        }];
        query.filters = vec![filter("a", FilterOp::Gt, vec![Value::Int(1)])];
        let c = build_count(&query, &PostgresDialect, &BuildOptions::default()).unwrap();
        assert_eq!(
            c.sql,
            r#"SELECT COUNT(*) FROM "public"."users" WHERE "a" > $1"#
        );
    }

    #[test]
    fn malformed_filters_are_rejected() {
        let bad = |f: Filter| {
            let mut query = q();
            query.filters = vec![f];
            build_select(&query, &SqliteDialect, &BuildOptions::default()).unwrap_err()
        };
        assert!(matches!(
            bad(filter("a", FilterOp::Between, vec![Value::Int(1)])),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            bad(filter("a", FilterOp::In, vec![])),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            bad(filter("a", FilterOp::Eq, vec![])),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            bad(filter("a", FilterOp::IsNull, vec![Value::Int(1)])),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            bad(filter("a", FilterOp::Contains, vec![Value::Int(1)])),
            Error::InvalidInput(_)
        ));
        assert!(matches!(
            bad(filter("a", FilterOp::Unknown, vec![Value::Int(1)])),
            Error::InvalidInput(_)
        ));
        assert!(
            build_select(
                &TableQuery::new(ObjectPath::new(None, "t"), 0),
                &SqliteDialect,
                &BuildOptions::default()
            )
            .is_err()
        );
    }

    #[test]
    fn raw_where_is_wrapped_and_combined() {
        let mut query = q();
        query.filters = vec![filter("a", FilterOp::Eq, vec![Value::Int(1)])];
        query.raw_where = Some("status = 'active' AND (score > 90 OR is_vip = true)".into());
        let b = build(&query, &SqliteDialect);
        assert!(
            b.sql.contains(
                r#"WHERE "a" = ? AND (status = 'active' AND (score > 90 OR is_vip = true))"#
            ),
            "{}",
            b.sql
        );
    }

    #[test]
    fn raw_where_can_be_refused_entirely() {
        let mut query = q();
        query.raw_where = Some("1 = 1".into());
        let opts = BuildOptions {
            allow_raw_where: false,
            ..BuildOptions::default()
        };
        assert!(matches!(
            build_select(&query, &SqliteDialect, &opts),
            Err(Error::PolicyDenied(_))
        ));
    }

    #[test]
    fn raw_where_must_be_one_plain_expression() {
        let d = &SqliteDialect;
        for ok in [
            "a > 1",
            "a IN (SELECT id FROM u WHERE x = 1)",
            "name = 'semi;colon' AND b = '--not a comment'",
            "EXISTS (SELECT 1 FROM t2 WHERE t2.id = t.id)",
        ] {
            assert!(validate_raw_where(ok, d).is_ok(), "{ok}");
        }
        for bad in [
            "1; DROP TABLE t",
            "1) OR (1=1",
            "(1 = 1",
            "a = 1 -- trailing",
            "a = 1 /* x */",
            "a = 'unterminated",
            "a = 1 UNION SELECT password FROM users",
            "a = 1 ORDER BY b",
            "a = 1 LIMIT 5",
            "1=1 INTO OUTFILE '/x'",
        ] {
            assert!(validate_raw_where(bad, d).is_err(), "{bad}");
        }
        assert!(validate_raw_where("a = 1 # c", &MySqlDialect).is_err());
    }
}
