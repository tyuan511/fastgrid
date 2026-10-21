//! What to offer at a byte offset, ranked, using the scope analysis rather than string prefixes.
//!
//! The ranking is what makes this useful: a column of a table the query actually mentions beats a
//! table that happens to share a prefix, which beats a keyword. `t.` offers only `t`'s columns.
//! Names already in scope are not offered again, and an exact match is dropped (completing `items`
//! to `items` is noise).

use fg_core::{ColumnMeta, DbKind, ObjectPath};

use crate::scope::{Catalog, Scope, scope_at};

/// What a candidate is. The order is the ranking: earlier wins ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletionKind {
    /// A column of a relation in scope. Most specific.
    Column,
    /// A relation in scope, by its alias (or its name when it has none).
    Relation,
    /// A table or view that is not in scope yet but exists in the schema.
    Table,
    /// A SQL keyword.
    Keyword,
}

/// One candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub kind: CompletionKind,
    /// What gets inserted.
    pub text: String,
    /// The bytes the completion replaces: the word already typed.
    ///
    /// This is what separates a completion from an insertion. An editor that only gets `text`
    /// inserts it at the caret, so typing `se` and accepting `select` produces `seselect`. The
    /// range is where the replacement happens, and it is the analysis's job to say it because only
    /// it knows where the word starts (a qualifier makes `t.na` replace `na`, not `t.na`).
    pub replace: std::ops::Range<usize>,
    /// What the menu shows. For a column this is `alias.column` so two `id`s are distinguishable.
    pub label: String,
    /// A short type or origin hint.
    pub detail: Option<String>,
}

/// How many candidates to return. A long menu is slower to read, not more helpful.
pub const MAX_COMPLETIONS: usize = 50;

/// SQL keywords worth completing. Deliberately the vocabulary a query is built from, not the whole
/// grammar: a menu of 300 keywords is worse than none.
pub const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "ON",
    "AS",
    "DISTINCT",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "WITH",
    "UNION",
    "UNION ALL",
    "EXISTS",
    "IN",
    "BETWEEN",
    "LIKE",
    "IS NULL",
    "IS NOT NULL",
    "AND",
    "OR",
    "NOT",
    "ASC",
    "DESC",
];

/// The word being typed at `offset`: what to match against and what to replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    /// Byte range of the word in the buffer.
    pub range: std::ops::Range<usize>,
    pub text: String,
    /// The qualifier before a dot, when the user typed `alias.`.
    pub qualifier: Option<String>,
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// Read the word under the caret. `None` when the caret is not in a word and not right after a dot:
/// there is nothing to complete against.
pub fn prefix_at(text: &str, offset: usize) -> Option<Prefix> {
    let offset = offset.min(text.len());
    let mut start = offset;
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut word_start = start;
    for (ix, ch) in text[..start].char_indices().rev() {
        if is_ident(ch) {
            word_start = ix;
        } else {
            break;
        }
    }
    let qualifier = if word_start > 0 && text[..word_start].ends_with('.') {
        let before_dot = &text[..word_start - 1];
        let mut q_start = before_dot.len();
        for (ix, ch) in before_dot.char_indices().rev() {
            if is_ident(ch) {
                q_start = ix;
            } else {
                break;
            }
        }
        let q = &before_dot[q_start..];
        (!q.is_empty()).then(|| q.to_owned())
    } else {
        None
    };
    let word = text[word_start..start].to_owned();
    if word.is_empty() && qualifier.is_none() {
        return None;
    }
    Some(Prefix {
        range: word_start..start,
        text: word,
        qualifier,
    })
}

/// Rank the candidates for the caret at `offset`.
///
/// `objects` is the schema's object list; it is used for tables that are not in scope yet, so
/// `SELECT * FROM ord|` can still offer `orders`.
pub fn complete_at(
    sql: &str,
    offset: usize,
    kind: DbKind,
    catalog: &dyn Catalog,
    objects: &[ObjectPath],
) -> Vec<Completion> {
    let Some(prefix) = prefix_at(sql, offset) else {
        return Vec::new();
    };
    let scope = scope_at(sql, offset, kind, catalog);
    completions(&scope, &prefix, catalog, objects)
}

/// The same, from a scope the caller already has.
pub fn completions(
    scope: &Scope,
    prefix: &Prefix,
    catalog: &dyn Catalog,
    objects: &[ObjectPath],
) -> Vec<Completion> {
    let needle = prefix.text.to_lowercase();
    let mut out: Vec<Completion> = Vec::new();

    // A qualifier narrows everything to that relation: `t.` is a column list, full stop.
    if let Some(qualifier) = &prefix.qualifier {
        let columns = scope.columns_of(qualifier);
        for column in columns {
            if !offers(&column.name, &needle, &prefix.text) {
                continue;
            }
            out.push(column_completion(qualifier, column, prefix.range.clone()));
        }
        // A qualifier that resolves to nothing may be a table the user is about to add: offer its
        // columns anyway rather than an empty menu.
        if out.is_empty()
            && let Some(path) = catalog.resolve(qualifier)
            && let Some(columns) = catalog.columns(&path)
        {
            for column in columns {
                if offers(&column.name, &needle, &prefix.text) {
                    out.push(column_completion(qualifier, column, prefix.range.clone()));
                }
            }
        }
        return finish(out);
    }

    // Columns of everything in scope, qualified by the name the query uses. A column is matched on
    // its own name, so typing `r` does not hide `r.id`.
    for relation in &scope.relations {
        for column in &relation.columns {
            if !column.name.to_lowercase().starts_with(&needle) {
                continue;
            }
            out.push(column_completion(
                &relation.name,
                column,
                prefix.range.clone(),
            ));
        }
    }

    // Relations in scope, by the name the query uses.
    for relation in &scope.relations {
        if !offers(&relation.name, &needle, &prefix.text) {
            continue;
        }
        out.push(Completion {
            kind: CompletionKind::Relation,
            text: relation.name.clone(),
            replace: prefix.range.clone(),
            label: relation.name.clone(),
            detail: relation
                .object
                .as_ref()
                .map(|p| p.name.clone())
                .or_else(|| relation.derived.then(|| "subquery".to_owned())),
        });
    }

    // Tables that exist but are not in scope: `FROM ord|` before `orders` is written.
    for path in objects {
        if !offers(&path.name, &needle, &prefix.text) {
            continue;
        }
        out.push(Completion {
            kind: CompletionKind::Table,
            text: path.name.clone(),
            replace: prefix.range.clone(),
            label: path.name.clone(),
            detail: path.namespace.clone(),
        });
    }

    for keyword in KEYWORDS {
        if !offers(keyword, &needle, &prefix.text) {
            continue;
        }
        out.push(Completion {
            kind: CompletionKind::Keyword,
            text: (*keyword).to_owned(),
            replace: prefix.range.clone(),
            label: (*keyword).to_owned(),
            detail: None,
        });
    }

    finish(out)
}

fn column_completion(
    qualifier: &str,
    column: &ColumnMeta,
    replace: std::ops::Range<usize>,
) -> Completion {
    Completion {
        kind: CompletionKind::Column,
        text: column.name.clone(),
        replace,
        label: format!("{qualifier}.{}", column.name),
        detail: Some(short_type(column)),
    }
}

/// Should `candidate` be offered for a typed `needle`? Prefix match, case-insensitive, and never the
/// name the user has already typed in full.
fn offers(candidate: &str, needle_lower: &str, typed: &str) -> bool {
    if candidate.eq_ignore_ascii_case(typed) {
        return false;
    }
    candidate.to_lowercase().starts_with(needle_lower)
}

/// Sort by kind, then shortest name, then alphabetically; drop duplicates; cap the list.
fn finish(mut out: Vec<Completion>) -> Vec<Completion> {
    out.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.text.len().cmp(&b.text.len()))
            .then_with(|| a.text.cmp(&b.text))
            .then_with(|| a.label.cmp(&b.label))
    });
    out.dedup_by(|a, b| a.kind == b.kind && a.text == b.text && a.label == b.label);
    out.truncate(MAX_COMPLETIONS);
    out
}

/// The base name of a native type, for the hint next to a column.
fn short_type(column: &ColumnMeta) -> String {
    let native = column.data_type.native.to_lowercase();
    let base = native
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_owned();
    if base.is_empty() {
        category_name(column.data_type.category).to_owned()
    } else {
        base
    }
}

fn category_name(category: fg_core::TypeCategory) -> &'static str {
    use fg_core::TypeCategory::*;
    match category {
        Bool => "bool",
        Integer => "int",
        Float => "float",
        Decimal => "decimal",
        Text => "text",
        Binary => "binary",
        Date => "date",
        Time => "time",
        Timestamp | TimestampTz => "timestamp",
        Interval => "interval",
        Json => "json",
        Uuid => "uuid",
        Enum => "enum",
        Set => "set",
        Array => "array",
        Geometry => "geometry",
        Network => "network",
        Bit => "bit",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::MapCatalog;
    use fg_core::{DataType, TypeCategory};

    fn path(name: &str) -> ObjectPath {
        ObjectPath::new(Some("main"), name)
    }

    fn column(name: &str, native: &str, category: TypeCategory) -> ColumnMeta {
        ColumnMeta::new(name, DataType::new(native, category))
    }

    fn catalog() -> MapCatalog {
        MapCatalog::new(vec![
            (
                path("items"),
                vec![
                    column("id", "INTEGER", TypeCategory::Integer),
                    column("name", "TEXT", TypeCategory::Text),
                ],
            ),
            (
                path("orders"),
                vec![
                    column("id", "INTEGER", TypeCategory::Integer),
                    column("total", "DECIMAL", TypeCategory::Decimal),
                ],
            ),
        ])
    }

    fn objects() -> Vec<ObjectPath> {
        vec![path("items"), path("orders"), path("order_items")]
    }

    fn complete(sql: &str, offset: usize) -> Vec<Completion> {
        complete_at(sql, offset, DbKind::Sqlite, &catalog(), &objects())
    }

    #[test]
    fn an_alias_resolves_to_its_tables_columns() {
        // The whole point: `i.` is `items`'s columns, not a guess.
        let sql = "SELECT i. FROM items i";
        let out = complete(sql, 9);
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["id", "name"], "{out:?}");
        assert_eq!(out[0].label, "i.id");
        assert_eq!(out[0].detail.as_deref(), Some("integer"));
    }

    #[test]
    fn a_qualifier_with_a_prefix_filters_its_columns() {
        let sql = "SELECT i.na FROM items i";
        let out = complete(sql, 10);
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["name"]);
    }

    #[test]
    fn a_qualifier_never_offers_keywords() {
        let sql = "SELECT i. FROM items i";
        let out = complete(sql, 9);
        assert!(
            out.iter().all(|c| c.kind == CompletionKind::Column),
            "{out:?}"
        );
    }

    #[test]
    fn a_qualifier_naming_a_real_table_offers_its_columns_before_it_is_joined() {
        // `orders.` while `orders` is not in the FROM list yet: the user is about to add it, and the
        // catalog knows its columns, so the menu is not empty.
        let sql = "SELECT orders. FROM items i";
        let out = complete(sql, 14);
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["id", "total"], "resolved through the catalog");
    }

    #[test]
    fn a_qualifier_that_names_nothing_offers_nothing() {
        // An alias that is not in scope has no columns, and inventing some would be worse.
        let out = complete("SELECT o. FROM items i", 9);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn columns_beat_tables_beat_keywords() {
        let sql = "SELECT i FROM items";
        let out = complete(sql, 8);
        let kinds: Vec<CompletionKind> = out.iter().map(|c| c.kind).collect();
        assert!(
            kinds.windows(2).all(|w| w[0] <= w[1]),
            "ranking is monotonic: {kinds:?}"
        );
    }

    #[test]
    fn a_relation_is_offered_by_its_alias() {
        // The caret inside `it` in the SELECT list, so the alias is a candidate.
        let sql = "SELECT i FROM items it";
        let out = complete(sql, 8);
        assert!(
            out.iter()
                .any(|c| c.kind == CompletionKind::Relation && c.text == "it"),
            "the alias, not the table name: {out:?}"
        );
    }

    #[test]
    fn a_table_not_in_scope_is_still_offered() {
        let sql = "SELECT * FROM ord";
        let out = complete(sql, sql.len());
        assert!(
            out.iter()
                .any(|c| c.kind == CompletionKind::Table && c.text == "orders"),
            "{out:?}"
        );
    }

    #[test]
    fn an_exact_match_is_not_offered() {
        let sql = "SELECT  FROM items";
        let out = complete(sql, 7);
        assert!(
            !out.iter().any(|c| c.text.eq_ignore_ascii_case("items")),
            "completing `items` to `items` is noise: {out:?}"
        );
    }

    #[test]
    fn matching_ignores_case() {
        let sql = "SELECT ITEM";
        let out = complete(sql, sql.len());
        assert!(
            out.iter().any(|c| c.text == "items"),
            "typed uppercase, offered as stored: {out:?}"
        );
    }

    #[test]
    fn the_shorter_name_wins_a_tie_within_a_kind() {
        let sql = "SELECT or FROM order_items";
        let out = complete(sql, 9);
        let tables: Vec<&str> = out
            .iter()
            .filter(|c| c.kind == CompletionKind::Table)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(tables, vec!["orders", "order_items"], "`orders` is shorter");
    }

    #[test]
    fn a_ctes_columns_are_offered_from_its_body() {
        // No `with_cte` stub: the body `SELECT * FROM orders` is analysed, so `orders`' columns are
        // what `r.` offers.
        let sql = "WITH recent AS (SELECT * FROM orders) SELECT r. FROM recent r";
        let offset = sql.find("SELECT r.").expect("the marker") + "SELECT r.".len();
        let out = complete_at(sql, offset, DbKind::Sqlite, &catalog(), &objects());
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["id", "total"], "the CTE's columns: {out:?}");
    }

    #[test]
    fn a_derived_tables_columns_are_offered() {
        let sql = "SELECT d. FROM (SELECT id, name FROM items) d";
        let offset = sql.find("SELECT d.").expect("the marker") + "SELECT d.".len();
        let out = complete_at(sql, offset, DbKind::Sqlite, &catalog(), &objects());
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["id", "name"], "{out:?}");
    }

    #[test]
    fn a_cte_can_be_joined_with_a_table_and_both_resolve() {
        let sql = "WITH recent AS (SELECT id, total FROM orders) \
                   SELECT  FROM recent r JOIN items i ON 1 = 1";
        // `r.` in the SELECT list: the CTE's columns, resolved from its body.
        let r_offset =
            sql.find("SELECT  FROM recent").expect("the outer select") + "SELECT r.".len();
        let r_cols: Vec<String> = complete(&sql.replacen("SELECT  ", "SELECT r.", 1), r_offset)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(r_cols, vec!["r.id", "r.total"], "the CTE side");

        // `i.` likewise, for the real table in the same FROM list.
        let i_offset = r_offset;
        let i_cols: Vec<String> = complete(&sql.replacen("SELECT  ", "SELECT i.", 1), i_offset)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(i_cols, vec!["i.id", "i.name"], "the table side");
    }

    #[test]
    fn each_join_alias_resolves_to_its_own_table() {
        // The case that matters: `i.` and `o.` point at different tables, which a prefix matcher
        // could never tell apart.
        let sql = "SELECT i. FROM items i JOIN orders o ON 1 = 1";
        let i_cols: Vec<String> = complete(sql, 9).into_iter().map(|c| c.label).collect();
        assert_eq!(i_cols, vec!["i.id", "i.name"], "`i.` is items");

        let sql = "SELECT o. FROM items i JOIN orders o ON 1 = 1";
        let o_cols: Vec<String> = complete(sql, 9).into_iter().map(|c| c.label).collect();
        assert_eq!(o_cols, vec!["o.id", "o.total"], "`o.` is orders");
    }

    #[test]
    fn nothing_to_complete_outside_a_word() {
        assert!(complete("SELECT  ", 8).is_empty());
        assert!(complete("", 0).is_empty());
    }

    #[test]
    fn the_list_is_capped_and_stable() {
        let sql = "SELECT a";
        let first = complete(sql, 8);
        let second = complete(sql, 8);
        assert_eq!(first, second, "the order does not drift");
        assert!(first.len() <= MAX_COMPLETIONS);
    }

    #[test]
    fn a_caret_inside_a_multibyte_character_does_not_panic() {
        let sql = "SELECT '中文' FROM items";
        for offset in 0..=sql.len() {
            let _ = complete(sql, offset);
        }
    }

    #[test]
    fn the_prefix_is_read_from_the_word_under_the_caret() {
        let prefix = prefix_at("SELECT na FROM items", 9).expect("inside `na`");
        assert_eq!(prefix.text, "na");
        assert_eq!(prefix.range, 7..9);
        let prefix = prefix_at("SELECT t.na", 11).expect("inside `t.na`");
        assert_eq!(prefix.qualifier.as_deref(), Some("t"));
    }
}
