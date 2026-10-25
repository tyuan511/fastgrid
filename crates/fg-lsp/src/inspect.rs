//! What the caret points at: hover text for a table, column, alias or CTE, and where a name
//! is defined in the same script.
//!
//! This is the analysis behind the editor's hover popover and go-to-definition, and behind the
//! server's `textDocument/hover` and `textDocument/definition`. It answers from the same scope
//! analysis as completion, so the three agree about what a name means.
//!
//! Definitions are deliberately same-script only. A table's definition lives in the database,
//! not in the buffer, so there is nothing to jump to for one; what does have a definition in
//! the script is a CTE (its `WITH` clause) and an alias (its `FROM` / `JOIN` site). A column
//! has no definition in the script either, so hovering one explains it and jumping from one
//! does nothing.

use std::ops::Range;

use fg_core::{ColumnMeta, DbKind, ObjectPath};

use fg_sql::lex::{Flavor, Kind, lex};

use crate::scope::{Catalog, Relation, Scope, scope_at, unquote};

/// The identifier at the caret, with its qualifier when the caret sits on `qual.name`'s name
/// part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifierAt {
    /// Qualifier and its range, when the caret sits on the name part of `qual.name`.
    pub qualifier: Option<(String, Range<usize>)>,
    pub name: String,
    pub name_range: Range<usize>,
    /// The caret sits on the qualifier of `qual.name` (a `.` follows it).
    pub followed_by_dot: bool,
}

/// The word token at `offset`, or the one ending exactly there (a caret parked after a word
/// still points at it). Quoted identifiers unquote; keywords are words too, and resolve to
/// nothing later, which is what makes hovering one show nothing.
pub fn identifier_at(sql: &str, offset: usize, kind: DbKind) -> Option<IdentifierAt> {
    let offset = offset.min(sql.len());
    let tokens = lex(sql, Flavor::from(kind));
    let ix = tokens.iter().position(|t| {
        matches!(t.kind, Kind::Word | Kind::QuotedIdent) && t.start <= offset && offset <= t.end
    })?;
    let token = tokens[ix];
    let prev = (0..ix).rev().find(|&i| !tokens[i].is_trivia());
    let next = (ix + 1..tokens.len()).find(|&i| !tokens[i].is_trivia());
    let qualifier = match prev {
        Some(dot)
            if tokens[dot].kind == Kind::Punct
                && tokens[dot].text(sql) == "."
                && let Some(name_ix) = (0..dot).rev().find(|&i| !tokens[i].is_trivia())
                && matches!(tokens[name_ix].kind, Kind::Word | Kind::QuotedIdent) =>
        {
            let name_token = tokens[name_ix];
            Some((
                unquote(name_token.text(sql)),
                name_token.start..name_token.end,
            ))
        }
        _ => None,
    };
    let followed_by_dot = matches!(next, Some(dot) if tokens[dot].kind == Kind::Punct && tokens[dot].text(sql) == ".");
    Some(IdentifierAt {
        qualifier,
        name: unquote(token.text(sql)),
        name_range: token.start..token.end,
        followed_by_dot,
    })
}

/// Hover text and the range it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub range: Range<usize>,
    pub markdown: String,
}

/// Hover text for the identifier at `offset`, or `None` when the caret is not on a name this
/// analysis can explain (a keyword, an unknown word, a function).
pub fn hover_at(
    sql: &str,
    offset: usize,
    kind: DbKind,
    catalog: &dyn Catalog,
) -> Option<HoverInfo> {
    let ident = identifier_at(sql, offset, kind)?;
    let scope = scope_at(sql, offset, kind, catalog);
    let markdown = if let Some((qualifier, _)) = &ident.qualifier {
        // `qual.name`: a column of that relation, or of the table the qualifier names.
        let columns = relation_columns(&scope, qualifier, catalog);
        let column = columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&ident.name))?;
        let source = scope
            .relation(qualifier)
            .map(relation_source)
            .unwrap_or_else(|| {
                catalog
                    .resolve(qualifier)
                    .map(|p| format!("table `{}`", display_path(&p)))
                    .unwrap_or_else(|| format!("relation `{qualifier}`"))
            });
        column_markdown(column, &source)
    } else if ident.followed_by_dot {
        // The caret sits on the qualifier itself: describe the relation.
        relation_hover(&scope, &ident.name, catalog)?
    } else if let Some(relation) = scope
        .relations
        .iter()
        .find(|r| r.name_span.as_ref().is_some_and(|s| s.contains(&offset)))
    {
        // A declaration site always describes its relation, whatever the word is.
        relation_markdown(relation)
    } else {
        // Elsewhere a bare word in a SELECT list is a column, so columns win over relations;
        // a relation is the fallback, then a table that is not in scope yet.
        let matches: Vec<(&Relation, &ColumnMeta)> = scope
            .relations
            .iter()
            .flat_map(|r| r.columns.iter().map(move |c| (r, c)))
            .filter(|(_, c)| c.name.eq_ignore_ascii_case(&ident.name))
            .collect();
        match matches.len() {
            1 => {
                let (relation, column) = matches[0];
                column_markdown(column, &relation_source(relation))
            }
            n if n > 1 => {
                let sources = matches
                    .iter()
                    .map(|(r, _)| relation_name(r))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "`{}` — a column of more than one relation: {sources}",
                    ident.name
                )
            }
            _ => relation_hover(&scope, &ident.name, catalog)?,
        }
    };
    Some(HoverInfo {
        range: ident.name_range,
        markdown,
    })
}

/// Where a name is defined in the same script: the range the caret is on, and the range to
/// jump to. Empty when the name has no definition in the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub origin_range: Range<usize>,
    pub target_range: Range<usize>,
}

pub fn definitions_at(
    sql: &str,
    offset: usize,
    kind: DbKind,
    catalog: &dyn Catalog,
) -> Vec<Definition> {
    let Some(ident) = identifier_at(sql, offset, kind) else {
        return Vec::new();
    };
    let scope = scope_at(sql, offset, kind, catalog);
    let Some(relation) = scope.relation(&ident.name) else {
        return Vec::new();
    };
    // A column has no definition in the script; only its qualifier does, and only when the
    // caret is on the qualifier.
    if ident.qualifier.is_some() {
        return Vec::new();
    }
    let target = if ident.followed_by_dot {
        // `qual.name` with the caret on `qual`: the qualifier's declaration, whatever it is.
        declaration_target(&scope, relation)
    } else if relation.derived || is_alias(relation) {
        // A bare CTE name jumps to its `WITH` clause; a bare alias to its `FROM` / `JOIN`
        // site. A bare table name has no jump: its declaration is the `FROM` site itself,
        // and anything else is not a reference to it.
        declaration_target(&scope, relation)
    } else {
        None
    };
    let Some(target) = target else {
        return Vec::new();
    };
    if target.contains(&offset) {
        return Vec::new();
    }
    vec![Definition {
        origin_range: ident.name_range,
        target_range: target,
    }]
}

/// Columns of a relation in scope, or of the table a qualifier names when it is not in scope.
fn relation_columns<'a>(
    scope: &'a Scope,
    qualifier: &str,
    catalog: &'a dyn Catalog,
) -> Vec<ColumnMeta> {
    if let Some(relation) = scope.relation(qualifier) {
        return relation.columns.clone();
    }
    catalog
        .resolve(qualifier)
        .and_then(|p| catalog.columns(&p))
        .map(<[_]>::to_vec)
        .unwrap_or_default()
}

/// Where a relation comes from, for a column's "from …" line.
fn relation_source(relation: &Relation) -> String {
    if relation.derived {
        return format!("CTE `{}`", relation.name);
    }
    match &relation.object {
        Some(path) if !path.name.eq_ignore_ascii_case(&relation.name) => {
            format!(
                "alias `{}` for table `{}`",
                relation.name,
                display_path(path)
            )
        }
        Some(path) => format!("table `{}`", display_path(path)),
        None => format!("relation `{}`", relation.name),
    }
}

/// The name a relation is listed under in an ambiguous-column message: its alias when it has
/// one, else its table.
fn relation_name(relation: &Relation) -> String {
    match &relation.object {
        Some(path) if !path.name.eq_ignore_ascii_case(&relation.name) => relation.name.clone(),
        Some(path) => display_path(path),
        None => relation.name.clone(),
    }
}

/// Hover text for a relation (or a table that is not in scope yet): what it is, and its
/// columns when they are known.
fn relation_hover(scope: &Scope, name: &str, catalog: &dyn Catalog) -> Option<String> {
    if let Some(relation) = scope.relation(name) {
        return Some(relation_markdown(relation));
    }
    let path = catalog.resolve(name)?;
    let columns = catalog
        .columns(&path)
        .map(<[_]>::to_vec)
        .unwrap_or_default();
    Some(table_markdown(&path, &columns))
}

fn relation_markdown(relation: &Relation) -> String {
    if relation.derived {
        let mut out = format!("CTE `{}`", relation.name);
        push_columns(&mut out, &relation.columns);
        out
    } else {
        match &relation.object {
            Some(path) if !path.name.eq_ignore_ascii_case(&relation.name) => {
                let mut out = format!(
                    "alias `{}` for table `{}`",
                    relation.name,
                    display_path(path)
                );
                push_columns(&mut out, &relation.columns);
                out
            }
            Some(path) => table_markdown(path, &relation.columns),
            None => format!("relation `{}`", relation.name),
        }
    }
}

fn table_markdown(path: &ObjectPath, columns: &[ColumnMeta]) -> String {
    let mut out = format!("table `{}`", display_path(path));
    push_columns(&mut out, columns);
    out
}

fn column_markdown(column: &ColumnMeta, source: &str) -> String {
    let mut out = format!("`{}` `{}`", column.name, column.data_type.native);
    if column.nullable == Some(false) {
        out.push_str(" NOT NULL");
    }
    out.push_str(&format!("\nfrom {source}"));
    out
}

fn push_columns(out: &mut String, columns: &[ColumnMeta]) {
    if columns.is_empty() {
        return;
    }
    for column in columns.iter().take(10) {
        out.push_str(&format!(
            "\n- `{}` `{}`",
            column.name, column.data_type.native
        ));
    }
    if columns.len() > 10 {
        out.push_str(&format!("\n- … and {} more", columns.len() - 10));
    }
}

fn display_path(path: &ObjectPath) -> String {
    match &path.namespace {
        Some(namespace) => format!("{namespace}.{}", path.name),
        None => path.name.clone(),
    }
}

/// A relation is an alias when it names an object under a different name.
fn is_alias(relation: &Relation) -> bool {
    relation
        .object
        .as_ref()
        .is_some_and(|p| !p.name.eq_ignore_ascii_case(&relation.name))
}

/// Where a relation is defined in the script: a CTE's `WITH` clause when it has one, else
/// its `FROM` / `JOIN` site.
fn declaration_target(scope: &Scope, relation: &Relation) -> Option<Range<usize>> {
    if relation.derived
        && let Some((_, span)) = scope
            .cte_spans
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&relation.name))
    {
        return Some(span.clone());
    }
    relation.name_span.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::MapCatalog;
    use fg_core::{DataType, TypeCategory};

    fn path(name: &str) -> ObjectPath {
        ObjectPath::new(Some("main"), name)
    }

    fn column(name: &str, native: &str) -> ColumnMeta {
        ColumnMeta::new(name, DataType::new(native, TypeCategory::Text))
    }

    fn catalog() -> MapCatalog {
        MapCatalog::new(vec![
            (
                path("items"),
                vec![
                    ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
                    column("name", "TEXT"),
                ],
            ),
            (
                path("orders"),
                vec![
                    ColumnMeta::new("id", DataType::new("INTEGER", TypeCategory::Integer)),
                    column("total", "REAL"),
                ],
            ),
        ])
    }

    #[test]
    fn a_qualified_column_names_its_type_and_table() {
        let sql = "SELECT i.id FROM items i";
        let at = sql.find("i.id").unwrap() + 2;
        let hover = hover_at(sql, at, DbKind::Sqlite, &catalog()).expect("a column hovers");
        assert!(
            hover.markdown.contains("`id` `INTEGER`"),
            "{}",
            hover.markdown
        );
        assert!(hover.markdown.contains("main.items"), "{}", hover.markdown);
        assert_eq!(hover.range, 9..11);
    }

    #[test]
    fn a_bare_column_resolves_through_scope() {
        let sql = "SELECT name FROM items";
        let at = sql.find("name").unwrap();
        let hover = hover_at(sql, at, DbKind::Sqlite, &catalog()).expect("a column hovers");
        assert!(
            hover.markdown.contains("`name` `TEXT`"),
            "{}",
            hover.markdown
        );
        assert!(hover.markdown.contains("main.items"), "{}", hover.markdown);
    }

    #[test]
    fn a_table_hovers_with_its_columns() {
        let sql = "SELECT * FROM items";
        let at = sql.find("items").unwrap();
        let hover = hover_at(sql, at, DbKind::Sqlite, &catalog()).expect("a table hovers");
        assert!(
            hover.markdown.contains("table `main.items`"),
            "{}",
            hover.markdown
        );
        assert!(
            hover.markdown.contains("`id` `INTEGER`"),
            "{}",
            hover.markdown
        );
        assert!(
            hover.markdown.contains("`name` `TEXT`"),
            "{}",
            hover.markdown
        );
    }

    #[test]
    fn a_cte_hovers_as_a_cte() {
        let sql = "WITH r AS (SELECT 1 AS a) SELECT a FROM r";
        let at = sql.rfind('r').unwrap();
        let hover = hover_at(sql, at, DbKind::Sqlite, &catalog()).expect("a CTE hovers");
        assert!(hover.markdown.contains("CTE `r`"), "{}", hover.markdown);
        assert!(hover.markdown.contains("`a`"), "{}", hover.markdown);
    }

    #[test]
    fn an_alias_qualifier_hovers_as_an_alias() {
        let sql = "SELECT i.id FROM items i";
        let at = sql.find('i').unwrap();
        let hover = hover_at(sql, at, DbKind::Sqlite, &catalog()).expect("an alias hovers");
        assert!(hover.markdown.contains("alias `i`"), "{}", hover.markdown);
        assert!(hover.markdown.contains("main.items"), "{}", hover.markdown);
    }

    #[test]
    fn keywords_and_unknown_words_hover_nothing() {
        let sql = "SELECT zzz FROM items";
        assert_eq!(hover_at(sql, 0, DbKind::Sqlite, &catalog()), None);
        assert_eq!(
            hover_at(sql, sql.find("zzz").unwrap(), DbKind::Sqlite, &catalog()),
            None
        );
    }

    #[test]
    fn a_cte_reference_jumps_to_its_with_clause() {
        let sql = "WITH r AS (SELECT 1 AS a) SELECT a FROM r";
        let at = sql.rfind('r').unwrap();
        let defs = definitions_at(sql, at, DbKind::Sqlite, &catalog());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].target_range, 5..6);
        assert_eq!(defs[0].origin_range, at..at + 1);
    }

    #[test]
    fn an_alias_qualifier_jumps_to_its_from_site() {
        let sql = "SELECT i.id FROM items i";
        let at = sql.find('i').unwrap();
        let defs = definitions_at(sql, at, DbKind::Sqlite, &catalog());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].target_range, 23..24);
    }

    #[test]
    fn columns_and_declaration_sites_and_plain_tables_jump_nowhere() {
        let sql = "SELECT i.id FROM items i";
        // On the column: columns are not defined in the script.
        assert!(definitions_at(sql, 10, DbKind::Sqlite, &catalog()).is_empty());
        // On the alias declaration itself: already there.
        assert!(definitions_at(sql, 23, DbKind::Sqlite, &catalog()).is_empty());
        // A plain table name has no definition in the script.
        let plain = "SELECT * FROM items";
        assert!(
            definitions_at(
                plain,
                plain.find("items").unwrap(),
                DbKind::Sqlite,
                &catalog()
            )
            .is_empty()
        );
    }
}
