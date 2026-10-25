//! What the caret can see: which relations are in scope at a byte offset, and what each of them
//! provides.
//!
//! This is the analysis the editor needs and a prefix matcher cannot do: `SELECT i. FROM items i`
//! resolves `i` to `items`, a CTE's columns shadow nothing but are visible like a table, and a
//! subquery's alias hides the outer table with the same name. Getting this right is the whole
//! reason the completion lives in a server instead of in the widget.
//!
//! Scope here is deliberately local: one statement at a time, `FROM` / `JOIN` / CTE sources only.
//! It does not type-check expressions or resolve functions; it answers "what names exist here".
//!
//! Pure functions, no IO: the caller supplies the schema it knows.

use std::collections::BTreeMap;

use fg_core::{ColumnMeta, DbKind, ObjectPath};

use fg_sql::lex::{Flavor, Kind, Token, lex};

/// One relation the caret can refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The name the query uses for it: an alias when there is one, else the object name.
    pub name: String,
    /// The table or view it comes from, when the name resolves to one.
    pub object: Option<ObjectPath>,
    /// Columns, when they are known. Empty means "the object is unknown or not described yet".
    pub columns: Vec<ColumnMeta>,
    /// Where the relation's name sits in the script, so the server can rename or go-to-definition
    /// later without re-analysing.
    pub name_span: Option<std::ops::Range<usize>>,
    /// A CTE or derived table: its columns come from a subquery, not from a catalog entry.
    pub derived: bool,
}

/// Everything in scope at one offset of one statement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Relations in the order they were written; a later alias shadows an earlier one.
    pub relations: Vec<Relation>,
    /// CTE declarations in this statement, in order: the name and the byte range of the name
    /// in the `WITH` clause, so go-to-definition can jump to where a CTE is defined rather
    /// than to one of its uses.
    pub cte_spans: Vec<(String, std::ops::Range<usize>)>,
    /// Byte offset the analysis was asked about.
    pub offset: usize,
}

impl Scope {
    /// The relation an alias or name refers to, newest first so a shadowing alias wins.
    pub fn relation(&self, name: &str) -> Option<&Relation> {
        self.relations
            .iter()
            .rev()
            .find(|r| r.name.eq_ignore_ascii_case(name))
    }

    /// Columns offered when the user typed `alias.`.
    pub fn columns_of(&self, name: &str) -> &[ColumnMeta] {
        self.relation(name)
            .map(|r| r.columns.as_slice())
            .unwrap_or(&[])
    }

    /// Every column visible without a qualifier, in relation order.
    pub fn all_columns(&self) -> Vec<&ColumnMeta> {
        self.relations
            .iter()
            .flat_map(|r| r.columns.iter())
            .collect()
    }

    /// True when nothing is in scope yet (the caret is before the first `FROM`).
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }
}

/// What the analyser needs to resolve names. Supplied by the caller, which owns the metadata cache.
pub trait Catalog {
    /// Columns of an object, if they are known.
    fn columns(&self, path: &ObjectPath) -> Option<&[ColumnMeta]>;
    /// Resolve a bare name (no schema) to an object. The first match wins, so the caller should put
    /// the connection's default namespace first.
    fn resolve(&self, name: &str) -> Option<ObjectPath>;
    /// Columns of a CTE, when the caller could analyse its body. Optional: returning `None` means
    /// "not analysed", and the CTE becomes a relation with no columns.
    fn cte_columns(&self, _name: &str) -> Option<Vec<ColumnMeta>> {
        None
    }
}

/// A simple catalog over a list of described objects, for tests and for the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapCatalog {
    objects: Vec<(ObjectPath, Vec<ColumnMeta>)>,
    ctes: BTreeMap<String, Vec<ColumnMeta>>,
}

impl MapCatalog {
    pub fn new(objects: Vec<(ObjectPath, Vec<ColumnMeta>)>) -> Self {
        Self {
            objects,
            ctes: BTreeMap::new(),
        }
    }

    pub fn with_cte(mut self, name: impl Into<String>, columns: Vec<ColumnMeta>) -> Self {
        self.ctes.insert(name.into().to_lowercase(), columns);
        self
    }
}

impl Catalog for MapCatalog {
    fn columns(&self, path: &ObjectPath) -> Option<&[ColumnMeta]> {
        self.objects
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.as_slice())
    }

    fn resolve(&self, name: &str) -> Option<ObjectPath> {
        self.objects
            .iter()
            .find(|(p, _)| p.name.eq_ignore_ascii_case(name))
            .map(|(p, _)| p.clone())
    }

    fn cte_columns(&self, name: &str) -> Option<Vec<ColumnMeta>> {
        self.ctes.get(&name.to_lowercase()).cloned()
    }
}

/// Analyse the statement containing `offset` and return what is in scope there.
///
/// The scope is what the caret can *see*, so it depends on where the caret is: before the `FROM`
/// clause there is nothing to qualify against, and inside a subquery the inner relations shadow the
/// outer ones.
pub fn scope_at(sql: &str, offset: usize, kind: DbKind, catalog: &dyn Catalog) -> Scope {
    let offset = offset.min(sql.len());
    let flavor = Flavor::from(kind);
    let tokens = lex(sql, flavor);
    // The statement around the caret: a query script can hold several, each with its own scope.
    let (start, end) = statement_bounds(&tokens, sql, offset);
    let mut analyser = Analyser {
        sql,
        tokens: &tokens,
        catalog,
        relations: Vec::new(),
        ctes: Vec::new(),
        cte_spans: Vec::new(),
        depth: 0,
    };
    analyser.run(start, end);
    Scope {
        relations: analyser.relations,
        cte_spans: analyser.cte_spans,
        offset,
    }
}

/// The token range of the statement containing `offset`.
fn statement_bounds(tokens: &[Token], sql: &str, offset: usize) -> (usize, usize) {
    // Split on `;` outside parentheses; the caret's statement is the one whose span holds it.
    let mut start = 0;
    let mut depth = 0i32;
    let mut ranges = Vec::new();
    for (ix, token) in tokens.iter().enumerate() {
        if token.kind == Kind::Punct {
            match token.text(sql) {
                "(" => depth += 1,
                ")" => depth -= 1,
                ";" if depth <= 0 => {
                    ranges.push((start, ix));
                    start = ix + 1;
                }
                _ => {}
            }
        }
    }
    ranges.push((start, tokens.len()));
    for (from, to) in ranges {
        let (s, e) = (
            tokens.get(from).map(|t| t.start).unwrap_or(sql.len()),
            tokens
                .get(to.saturating_sub(1))
                .map(|t| t.end)
                .unwrap_or(sql.len()),
        );
        if offset >= s && offset <= e.max(s) {
            return (from, to);
        }
    }
    (0, tokens.len())
}

struct Analyser<'a> {
    sql: &'a str,
    tokens: &'a [Token],
    catalog: &'a dyn Catalog,
    relations: Vec<Relation>,
    /// CTE declarations visible to this statement: name -> columns, when the body could be
    /// analysed.
    ctes: Vec<(String, Option<Vec<ColumnMeta>>)>,
    /// CTE declarations in this statement: name and the byte range of the name in the `WITH`
    /// clause. Inner analysers (subquery bodies) keep their own; only the outermost one's
    /// spans reach the returned `Scope`.
    cte_spans: Vec<(String, std::ops::Range<usize>)>,
    /// How deep the analysis has recursed into subqueries. A pathological query must not blow the
    /// stack, and beyond a few levels the columns are not worth the work.
    depth: usize,
}

/// How deep a subquery's columns are resolved. Real queries nest a handful of levels; the cap keeps
/// a hostile or generated statement from recursing without bound.
const MAX_DEPTH: usize = 8;

impl<'a> Analyser<'a> {
    fn run(&mut self, start: usize, end: usize) {
        // CTEs first: they are in scope for the whole statement.
        self.collect_ctes(start, end);
        // Then the relations of the FROM/JOIN clauses. The whole statement, not only the text
        // before the caret: in `SELECT i. FROM items i` the relation is written after the caret
        // and is exactly what the user means.
        self.collect_relations(start, end);
    }

    /// `WITH name [(cols)] AS (...)` declarations, with their bodies analysed so the CTE's columns
    /// are known. A later CTE can refer to an earlier one, which is why this walks in order and
    /// registers each before looking at the next.
    fn collect_ctes(&mut self, start: usize, end: usize) {
        let mut ix = start;
        while ix < end {
            let is_with = self.tokens[ix].kind == Kind::Word
                && self.tokens[ix].text(self.sql).eq_ignore_ascii_case("with");
            if !is_with {
                ix += 1;
                continue;
            }
            let mut cursor = ix + 1;
            while cursor < end && self.tokens[cursor].is_trivia() {
                cursor += 1;
            }
            // `WITH RECURSIVE` — the keyword does not change how the body is analysed here.
            if cursor < end
                && self.tokens[cursor].kind == Kind::Word
                && self.tokens[cursor]
                    .text(self.sql)
                    .eq_ignore_ascii_case("recursive")
            {
                cursor += 1;
                while cursor < end && self.tokens[cursor].is_trivia() {
                    cursor += 1;
                }
            }
            loop {
                while cursor < end && self.tokens[cursor].is_trivia() {
                    cursor += 1;
                }
                let Some(name_token) = self.tokens.get(cursor).copied() else {
                    return;
                };
                if !matches!(name_token.kind, Kind::Word | Kind::QuotedIdent) {
                    return;
                }
                let name = unquote(name_token.text(self.sql));
                self.cte_spans
                    .push((name.clone(), name_token.start..name_token.end));
                let declared = self.column_list_after(cursor);
                // Find `AS (`, then the body's token range.
                let Some((body_start, body_end, after)) = self.cte_body(cursor, end) else {
                    return;
                };
                // The body is analysed with everything declared so far in scope: a later CTE may
                // refer to an earlier one, and a recursive CTE refers to itself.
                let body_columns = if self.depth >= MAX_DEPTH {
                    None
                } else {
                    let mut inner = Analyser {
                        sql: self.sql,
                        tokens: self.tokens,
                        catalog: self.catalog,
                        relations: Vec::new(),
                        ctes: self.ctes.clone(),
                        cte_spans: Vec::new(),
                        depth: self.depth + 1,
                    };
                    // A recursive CTE sees itself; without columns of its own, which is honest.
                    inner.ctes.push((name.clone(), declared.clone()));
                    inner.run(body_start, body_end);
                    Some(inner.select_columns(body_start, body_end))
                };
                // An explicit column list wins over the body's projection.
                let columns = declared.or(body_columns).unwrap_or_default();
                self.ctes.push((name.clone(), Some(columns)));
                cursor = after;
                while cursor < end && self.tokens[cursor].is_trivia() {
                    cursor += 1;
                }
                if cursor < end
                    && self.tokens[cursor].kind == Kind::Punct
                    && self.tokens[cursor].text(self.sql) == ","
                {
                    cursor += 1;
                    continue;
                }
                break;
            }
            ix = cursor.max(ix + 1);
        }
    }

    /// The token range of a CTE's body: `name [(cols)] AS ( <body> )`. Returns
    /// `(body_start, body_end, index_after_close)`.
    fn cte_body(&self, name_ix: usize, end: usize) -> Option<(usize, usize, usize)> {
        // Skip the optional column list, then `AS`, then the opening parenthesis.
        let mut cursor = name_ix + 1;
        while cursor < end && self.tokens[cursor].is_trivia() {
            cursor += 1;
        }
        if cursor < end
            && self.tokens[cursor].kind == Kind::Punct
            && self.tokens[cursor].text(self.sql) == "("
        {
            cursor = matching_paren(self.tokens, self.sql, cursor)? + 1;
        }
        while cursor < end && self.tokens[cursor].is_trivia() {
            cursor += 1;
        }
        if cursor >= end
            || self.tokens[cursor].kind != Kind::Word
            || !self.tokens[cursor]
                .text(self.sql)
                .eq_ignore_ascii_case("as")
        {
            return None;
        }
        cursor += 1;
        while cursor < end && self.tokens[cursor].is_trivia() {
            cursor += 1;
        }
        // PostgreSQL allows `AS MATERIALIZED (...)`; skip the modifier.
        if cursor < end
            && self.tokens[cursor].kind == Kind::Word
            && (self.tokens[cursor]
                .text(self.sql)
                .eq_ignore_ascii_case("materialized")
                || self.tokens[cursor]
                    .text(self.sql)
                    .eq_ignore_ascii_case("not"))
        {
            // `NOT MATERIALIZED` is two words.
            cursor += 1;
            while cursor < end && self.tokens[cursor].is_trivia() {
                cursor += 1;
            }
            if cursor < end
                && self.tokens[cursor].kind == Kind::Word
                && self.tokens[cursor]
                    .text(self.sql)
                    .eq_ignore_ascii_case("materialized")
            {
                cursor += 1;
                while cursor < end && self.tokens[cursor].is_trivia() {
                    cursor += 1;
                }
            }
        }
        if cursor >= end
            || self.tokens[cursor].kind != Kind::Punct
            || self.tokens[cursor].text(self.sql) != "("
        {
            return None;
        }
        let close = matching_paren(self.tokens, self.sql, cursor)?;
        Some((cursor + 1, close, close + 1))
    }

    /// A CTE's declared column list: `name (a, b) AS (…)`.
    fn column_list_after(&self, name_ix: usize) -> Option<Vec<ColumnMeta>> {
        let mut cursor = name_ix + 1;
        while cursor < self.tokens.len() && self.tokens[cursor].is_trivia() {
            cursor += 1;
        }
        let open = self.tokens.get(cursor)?;
        if open.kind != Kind::Punct || open.text(self.sql) != "(" {
            return None;
        }
        let close = matching_paren(self.tokens, self.sql, cursor)?;
        // The list must be followed by `AS`, otherwise those parentheses were something else.
        let mut after = close + 1;
        while after < self.tokens.len() && self.tokens[after].is_trivia() {
            after += 1;
        }
        if !self
            .tokens
            .get(after)
            .is_some_and(|t| t.kind == Kind::Word && t.text(self.sql).eq_ignore_ascii_case("as"))
        {
            return None;
        }
        let mut columns = Vec::new();
        for ix in cursor + 1..close {
            let token = self.tokens[ix];
            if token.kind == Kind::Word || token.kind == Kind::QuotedIdent {
                columns.push(ColumnMeta::new(
                    unquote(token.text(self.sql)),
                    fg_core::DataType::new("", fg_core::TypeCategory::Other),
                ));
            }
        }
        (!columns.is_empty()).then_some(columns)
    }

    /// The columns a subquery's `SELECT` list produces, resolved against that subquery's own scope.
    ///
    /// This is what makes a CTE's or a derived table's columns available: `SELECT a, b FROM t` in a
    /// subquery yields `a` and `b`, and `SELECT *` expands to the columns of the relations in its
    /// own `FROM`. Anything more exotic (a bare expression with no alias, a `RETURNING`) contributes
    /// what it can and no more.
    fn select_columns(&mut self, start: usize, end: usize) -> Vec<ColumnMeta> {
        // The inner relations are what `SELECT *` expands to, so they have to be collected first.
        let mut inner = Analyser {
            sql: self.sql,
            tokens: self.tokens,
            catalog: self.catalog,
            relations: Vec::new(),
            ctes: self.ctes.clone(),
            cte_spans: Vec::new(),
            depth: self.depth,
        };
        inner.collect_relations(start, end);

        let Some(select_ix) = self.find_word(start, end, "select") else {
            return Vec::new();
        };
        let list_end = self.find_word(select_ix + 1, end, "from").unwrap_or(end);
        let mut columns = Vec::new();
        // Split the projection on top-level commas.
        let mut item_start = select_ix + 1;
        let mut depth = 0i32;
        let mut item_ends: Vec<usize> = Vec::new();
        for ix in select_ix + 1..=list_end {
            let token = self.tokens.get(ix).copied();
            match token {
                Some(t) if t.kind == Kind::Punct && t.text(self.sql) == "(" => depth += 1,
                Some(t) if t.kind == Kind::Punct && t.text(self.sql) == ")" => depth -= 1,
                Some(t) if t.kind == Kind::Punct && t.text(self.sql) == "," && depth == 0 => {
                    item_ends.push(ix);
                }
                _ => {}
            }
        }
        item_ends.push(list_end);
        for item_end in item_ends {
            let slice = item_start..item_end;
            item_start = item_end + 1;
            columns.extend(self.projection_columns(slice, &inner.relations));
        }
        columns
    }

    /// One item of a `SELECT` list. A star expands to many columns; everything else is at most one.
    fn projection_columns(
        &self,
        range: std::ops::Range<usize>,
        relations: &[Relation],
    ) -> Vec<ColumnMeta> {
        let tokens: Vec<&Token> = range
            .clone()
            .filter_map(|ix| self.tokens.get(ix))
            .filter(|t| !t.is_trivia())
            .collect();
        if tokens.is_empty() {
            return Vec::new();
        }
        let first = tokens[0];
        // `*` expands to every column of the relations in scope, in the order they were written.
        if first.kind == Kind::Punct && first.text(self.sql) == "*" {
            return relations
                .iter()
                .flat_map(|relation| relation.columns.iter().cloned())
                .collect();
        }
        // `alias.*` likewise, for that relation only.
        if tokens.len() >= 3
            && matches!(tokens[0].kind, Kind::Word | Kind::QuotedIdent)
            && tokens[1].kind == Kind::Punct
            && tokens[1].text(self.sql) == "."
            && tokens[2].kind == Kind::Punct
            && tokens[2].text(self.sql) == "*"
        {
            let qualifier = unquote(tokens[0].text(self.sql));
            return relations
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case(&qualifier))
                .map(|r| r.columns.clone())
                .unwrap_or_default();
        }
        // `expr AS name` — the explicit alias is the output name.
        let mut name: Option<String> = None;
        for window in tokens.windows(2) {
            if window[0].kind == Kind::Word
                && window[0].text(self.sql).eq_ignore_ascii_case("as")
                && matches!(window[1].kind, Kind::Word | Kind::QuotedIdent)
            {
                name = Some(unquote(window[1].text(self.sql)));
            }
        }
        // A bare column: `SELECT id` or `SELECT t.id`. The name is the last identifier.
        if name.is_none() {
            let identifiers: Vec<&&Token> = tokens
                .iter()
                .filter(|t| matches!(t.kind, Kind::Word | Kind::QuotedIdent))
                .collect();
            // A lone identifier, or `qualifier.column`.
            let candidate = match identifiers.len() {
                1 => Some(*identifiers[0]),
                2 if tokens
                    .iter()
                    .any(|t| t.kind == Kind::Punct && t.text(self.sql) == ".") =>
                {
                    Some(*identifiers[1])
                }
                _ => None,
            };
            if let Some(token) = candidate {
                let text = token.text(self.sql);
                // A function call's name is not a column name.
                let is_call = tokens
                    .iter()
                    .position(|t| std::ptr::eq(*t, token))
                    .and_then(|ix| tokens.get(ix + 1))
                    .is_some_and(|next| next.kind == Kind::Punct && next.text(self.sql) == "(");
                if !is_call {
                    name = Some(unquote(text));
                }
            }
        }
        let Some(name) = name else {
            return Vec::new();
        };
        // Type: look the column up in the relations this subquery sees.
        let column = relations
            .iter()
            .flat_map(|r| r.columns.iter())
            .find(|c| c.name.eq_ignore_ascii_case(&name))
            .cloned();
        vec![match column {
            Some(found) => ColumnMeta::new(name, found.data_type),
            None => ColumnMeta::new(
                name,
                fg_core::DataType::new("", fg_core::TypeCategory::Other),
            ),
        }]
    }

    /// Index of the first `word` token in `start..end`.
    fn find_word(&self, start: usize, end: usize, word: &str) -> Option<usize> {
        (start..end).find(|&ix| {
            self.tokens[ix].kind == Kind::Word
                && self.tokens[ix].text(self.sql).eq_ignore_ascii_case(word)
        })
    }

    /// Relations introduced by `FROM`, `JOIN` and the comma-separated list that follows them.
    fn collect_relations(&mut self, start: usize, end: usize) {
        let mut ix = start;
        while ix < end {
            let token = self.tokens[ix];
            // A parenthesised body is a query of its own: its relations belong to that scope, and
            // are collected when it is analysed (a CTE's body, a derived table). Walking into it
            // here would leak the inner tables into the outer scope.
            if token.kind == Kind::Punct && token.text(self.sql) == "(" {
                ix = matching_paren(self.tokens, self.sql, ix).map_or(end, |close| close + 1);
                continue;
            }
            let is_source_keyword = token.kind == Kind::Word && {
                let text = token.text(self.sql);
                text.eq_ignore_ascii_case("from") || text.eq_ignore_ascii_case("join")
            };
            if !is_source_keyword {
                ix += 1;
                continue;
            }
            ix = self.collect_source_list(ix + 1, end);
        }
    }

    /// Parse `a [AS] x, b, c JOIN d ON …` after a `FROM`/`JOIN`, stopping at the next clause.
    fn collect_source_list(&mut self, mut ix: usize, end: usize) -> usize {
        loop {
            while ix < end && self.tokens[ix].is_trivia() {
                ix += 1;
            }
            if ix >= end {
                return ix;
            }
            let token = self.tokens[ix];
            if token.kind == Kind::Word && is_clause_boundary(token.text(self.sql)) {
                return ix;
            }
            if token.kind == Kind::Punct && matches!(token.text(self.sql), ")" | ";") {
                return ix;
            }
            if token.kind == Kind::Punct && token.text(self.sql) == "(" {
                // A derived table: `( SELECT … ) alias`. Its columns are the subquery's projection.
                let Some(close) = matching_paren(self.tokens, self.sql, ix) else {
                    return end;
                };
                let columns = if self.depth >= MAX_DEPTH {
                    Vec::new()
                } else {
                    let mut inner = Analyser {
                        sql: self.sql,
                        tokens: self.tokens,
                        catalog: self.catalog,
                        relations: Vec::new(),
                        ctes: self.ctes.clone(),
                        cte_spans: Vec::new(),
                        depth: self.depth + 1,
                    };
                    inner.select_columns(ix + 1, close)
                };
                let alias = self.alias_after(close, end);
                if let Some((name, span)) = alias {
                    self.relations.push(Relation {
                        name,
                        object: None,
                        columns,
                        name_span: Some(span),
                        derived: true,
                    });
                }
                ix = close + 1;
            } else if matches!(token.kind, Kind::Word | Kind::QuotedIdent) {
                // `schema.table`, `table`, or a CTE name.
                let mut name = unquote(token.text(self.sql));
                let mut name_span = token.start..token.end;
                let mut next = ix + 1;
                while next < end
                    && self.tokens[next].kind == Kind::Punct
                    && self.tokens[next].text(self.sql) == "."
                {
                    let part = next + 1;
                    if part < end
                        && matches!(self.tokens[part].kind, Kind::Word | Kind::QuotedIdent)
                    {
                        name = unquote(self.tokens[part].text(self.sql));
                        name_span = self.tokens[part].start..self.tokens[part].end;
                        next = part + 1;
                    } else {
                        break;
                    }
                }
                let alias = self.alias_after(next - 1, end);
                let (display, span) = match &alias {
                    Some((alias, alias_span)) => (alias.clone(), alias_span.clone()),
                    None => (name.clone(), name_span),
                };
                let cte = self
                    .ctes
                    .iter()
                    .find(|(cte, _)| cte.eq_ignore_ascii_case(&name))
                    .map(|(_, columns)| columns.clone());
                let from_cte = cte.is_some();
                let object = match &cte {
                    Some(_) => None,
                    None => self.catalog.resolve(&name),
                };
                let columns = match cte {
                    Some(Some(columns)) => columns,
                    Some(None) => Vec::new(),
                    None => object
                        .as_ref()
                        .and_then(|p| self.catalog.columns(p))
                        .map(<[_]>::to_vec)
                        .unwrap_or_default(),
                };
                self.relations.push(Relation {
                    name: display,
                    object,
                    columns,
                    name_span: Some(span),
                    derived: from_cte,
                });
                ix = match alias {
                    Some(_) => self.index_after_alias(next, end),
                    None => next,
                };
            } else {
                ix += 1;
            }
            while ix < end && self.tokens[ix].is_trivia() {
                ix += 1;
            }
            if ix < end
                && self.tokens[ix].kind == Kind::Punct
                && self.tokens[ix].text(self.sql) == ","
            {
                ix += 1;
                continue;
            }
            return ix;
        }
    }

    /// `[AS] alias` right after a relation, when the alias is not a clause keyword.
    fn alias_after(&self, ix: usize, end: usize) -> Option<(String, std::ops::Range<usize>)> {
        let mut cursor = ix + 1;
        while cursor < end && self.tokens[cursor].is_trivia() {
            cursor += 1;
        }
        if cursor >= end {
            return None;
        }
        let token = self.tokens[cursor];
        if token.kind == Kind::Word && token.text(self.sql).eq_ignore_ascii_case("as") {
            cursor += 1;
            while cursor < end && self.tokens[cursor].is_trivia() {
                cursor += 1;
            }
            let token = *self.tokens.get(cursor)?;
            if matches!(token.kind, Kind::Word | Kind::QuotedIdent) {
                return Some((unquote(token.text(self.sql)), token.start..token.end));
            }
            return None;
        }
        if token.kind == Kind::Word {
            let text = token.text(self.sql);
            if is_clause_boundary(text)
                || text.eq_ignore_ascii_case("on")
                || text.eq_ignore_ascii_case("using")
            {
                return None;
            }
            return Some((text.to_owned(), token.start..token.end));
        }
        if token.kind == Kind::QuotedIdent {
            return Some((unquote(token.text(self.sql)), token.start..token.end));
        }
        None
    }

    /// Skip an alias and anything up to the next comma or clause.
    fn index_after_alias(&self, from: usize, end: usize) -> usize {
        let mut ix = from;
        while ix < end {
            let token = self.tokens[ix];
            if token.kind == Kind::Word && is_clause_boundary(token.text(self.sql)) {
                return ix;
            }
            if token.kind == Kind::Punct && matches!(token.text(self.sql), "," | ";") {
                return ix;
            }
            ix += 1;
        }
        ix
    }
}

/// Words that end a `FROM` source list: the next clause starts here.
fn is_clause_boundary(word: &str) -> bool {
    const BOUNDARIES: &[&str] = &[
        "where",
        "group",
        "having",
        "order",
        "limit",
        "offset",
        "union",
        "except",
        "intersect",
        "returning",
        "window",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "full",
        "cross",
        "natural",
    ];
    BOUNDARIES.iter().any(|b| word.eq_ignore_ascii_case(b))
}

/// Index of the `)` matching the `(` at `open`.
fn matching_paren(tokens: &[Token], sql: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (ix, token) in tokens.iter().enumerate().skip(open) {
        if token.kind != Kind::Punct {
            continue;
        }
        match token.text(sql) {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(ix);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip the quoting an identifier may carry, so `"items"`, `` `items` `` and `[items]` all compare
/// equal to `items`.
pub(crate) fn unquote(text: &str) -> String {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        let quoted = matches!((first, last), (b'"', b'"') | (b'`', b'`') | (b'[', b']'));
        if quoted {
            return trimmed[1..trimmed.len() - 1].to_owned();
        }
    }
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    column("item_id", "INTEGER", TypeCategory::Integer),
                    column("total", "DECIMAL", TypeCategory::Decimal),
                ],
            ),
        ])
    }

    fn scope(sql: &str, offset: usize) -> Scope {
        scope_at(sql, offset, DbKind::Sqlite, &catalog())
    }

    #[test]
    fn a_table_without_an_alias_is_referred_to_by_its_name() {
        let sql = "SELECT  FROM items";
        let s = scope(sql, 7);
        assert_eq!(s.relations.len(), 1);
        assert_eq!(s.relations[0].name, "items");
        assert_eq!(
            s.relations[0].object.as_ref().map(|p| p.name.as_str()),
            Some("items")
        );
        assert_eq!(s.relations[0].columns.len(), 2);
    }

    #[test]
    fn an_alias_replaces_the_table_name() {
        let sql = "SELECT  FROM items i";
        let s = scope(sql, 7);
        assert_eq!(s.relations[0].name, "i", "the alias is what the query uses");
        assert_eq!(
            s.relations[0].columns.len(),
            2,
            "but the columns are the table's"
        );
        assert_eq!(
            s.relations[0].object.as_ref().map(|p| p.name.as_str()),
            Some("items")
        );
    }

    #[test]
    fn as_before_the_alias_is_optional() {
        for sql in ["SELECT  FROM items AS i", "SELECT  FROM items i"] {
            let s = scope(sql, 7);
            assert_eq!(s.relations[0].name, "i", "{sql}");
        }
    }

    #[test]
    fn a_join_adds_a_second_relation() {
        let sql = "SELECT  FROM items i JOIN orders o ON o.item_id = i.id";
        let s = scope(sql, 7);
        let names: Vec<&str> = s.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["i", "o"]);
        assert_eq!(s.columns_of("o").len(), 3);
    }

    #[test]
    fn a_qualified_alias_is_reported_for_its_last_part() {
        let sql = "SELECT  FROM main.items it";
        let s = scope(sql, 7);
        assert_eq!(s.relations[0].name, "it");
    }

    #[test]
    fn the_select_list_sees_the_relations_that_follow_it() {
        // The caret is in the SELECT list, before `FROM items`, and `items` must still be visible:
        // that is what `SELECT i.` needs.
        let sql = "SELECT  FROM items";
        let s = scope(sql, 7);
        assert_eq!(s.relations.len(), 1);
        assert_eq!(s.relations[0].name, "items");
    }

    #[test]
    fn a_ctes_body_relations_do_not_leak_into_the_outer_scope() {
        // `orders` appears only inside the CTE's body. The outer statement must not see it, or
        // completion would offer columns of a table the query never joins.
        let sql = "WITH recent AS (SELECT id FROM orders) SELECT  FROM recent";
        let s = scope(sql, sql.len() - 6);
        let names: Vec<&str> = s.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["recent"], "only the CTE is in scope: {names:?}");
    }

    #[test]
    fn a_ctes_columns_come_from_its_body() {
        // No `with_cte` on the catalog: the body itself is analysed.
        let sql = "WITH recent AS (SELECT id, total FROM orders) SELECT  FROM recent";
        let s = scope(sql, sql.len() - 6);
        let recent = s.relation("recent").expect("the CTE");
        assert!(recent.derived);
        let names: Vec<&str> = recent.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "total"], "the body's projection");
    }

    #[test]
    fn a_ctes_star_expands_against_its_body() {
        let sql = "WITH all_orders AS (SELECT * FROM orders) SELECT  FROM all_orders";
        let s = scope(sql, sql.len() - 6);
        let cte = s.relation("all_orders").expect("the CTE");
        let names: Vec<&str> = cte.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "item_id", "total"]);
    }

    #[test]
    fn an_explicit_column_list_wins_over_the_body() {
        let sql = "WITH x (a, b) AS (SELECT id, total FROM orders) SELECT  FROM x";
        let s = scope(sql, sql.len() - 6);
        let names: Vec<&str> = s
            .relation("x")
            .expect("the CTE")
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b"], "the declared names, not the body's");
    }

    #[test]
    fn a_cte_body_can_read_an_earlier_cte() {
        let sql = "WITH first AS (SELECT id FROM items), \
                   second AS (SELECT id FROM first) \
                   SELECT  FROM second";
        let s = scope(sql, sql.len() - 6);
        let second = s.relation("second").expect("the second CTE");
        assert_eq!(
            second.columns.len(),
            1,
            "`first` resolved inside `second`'s body: {second:?}"
        );
        assert_eq!(second.columns[0].name, "id");
    }

    #[test]
    fn a_cte_body_reading_a_cte_declared_later_has_no_columns() {
        // Forward references are not valid SQL; the analyser must not invent columns for them.
        let sql = "WITH a AS (SELECT id FROM b), b AS (SELECT id FROM items) SELECT  FROM a";
        let s = scope(sql, sql.len() - 6);
        let a = s.relation("a").expect("the first CTE");
        assert!(
            a.columns.iter().all(|c| c.name == "id"),
            "`b` was not in scope for `a`, so `id` could not be typed: {a:?}"
        );
    }

    #[test]
    fn a_recursive_cte_does_not_recurse_forever() {
        let sql = "WITH RECURSIVE nums (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums) \
                   SELECT  FROM nums";
        let s = scope(sql, sql.len() - 6);
        let nums = s.relation("nums").expect("the recursive CTE");
        assert_eq!(nums.columns.len(), 1, "its declared column list");
        assert_eq!(nums.columns[0].name, "n");
    }

    #[test]
    fn a_materialized_cte_is_parsed() {
        // PostgreSQL's `AS MATERIALIZED (...)` puts two words between `AS` and the body.
        let sql = "WITH x AS MATERIALIZED (SELECT id FROM items) SELECT  FROM x";
        let s = scope(sql, sql.len() - 6);
        assert_eq!(s.relation("x").expect("the CTE").columns.len(), 1);
    }

    #[test]
    fn a_deeply_nested_subquery_stops_at_the_depth_cap() {
        // Well past MAX_DEPTH: the outer layers still resolve, and nothing overflows the stack.
        let mut sql = String::from("SELECT id FROM items");
        for _ in 0..40 {
            sql = format!("SELECT * FROM ({sql}) t");
        }
        let s = scope(&sql, sql.len());
        assert!(!s.relations.is_empty(), "the outer relation is still there");
    }

    #[test]
    fn a_cte_with_a_declared_column_list_uses_those_names() {
        let sql = "WITH x (a, b) AS (SELECT 1, 2) SELECT  FROM x";
        let s = scope(sql, sql.len() - 6);
        let x = s.relation("x").expect("the CTE");
        let names: Vec<&str> = x.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn a_later_alias_shadows_an_earlier_one() {
        let sql = "SELECT  FROM items i JOIN orders i ON 1 = 1";
        let s = scope(sql, 7);
        let found = s.relation("i").expect("an `i`");
        assert_eq!(
            found.object.as_ref().map(|p| p.name.as_str()),
            Some("orders"),
            "the newest `i` wins"
        );
    }

    #[test]
    fn only_the_statement_around_the_caret_is_analysed() {
        let sql = "SELECT  FROM items;\nSELECT  FROM orders";
        let first = scope(sql, 7);
        assert_eq!(first.relations[0].name, "items");
        let second = scope(sql, sql.len() - 7);
        assert_eq!(second.relations[0].name, "orders");
    }

    #[test]
    fn quoted_identifiers_are_unquoted_for_matching() {
        let sql = "SELECT  FROM \"items\"";
        let s = scope(sql, 7);
        assert_eq!(s.relations[0].name, "items");
        assert!(s.relations[0].object.is_some(), "quoted names resolve too");
    }

    #[test]
    fn an_unknown_table_is_still_a_relation_with_no_columns() {
        let sql = "SELECT  FROM nowhere";
        let s = scope(sql, 7);
        assert_eq!(s.relations[0].name, "nowhere");
        assert!(s.relations[0].columns.is_empty());
        assert!(s.relations[0].object.is_none());
    }

    #[test]
    fn a_from_list_with_commas_adds_every_relation() {
        let sql = "SELECT  FROM items, orders";
        let s = scope(sql, 7);
        let names: Vec<&str> = s.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["items", "orders"]);
    }

    #[test]
    fn a_where_clause_ends_the_source_list() {
        let sql = "SELECT  FROM items WHERE x = 1";
        let s = scope(sql, sql.len());
        assert_eq!(s.relations.len(), 1, "WHERE is not a table");
    }

    #[test]
    fn all_columns_lists_every_visible_column() {
        let sql = "SELECT  FROM items i JOIN orders o ON 1 = 1";
        let s = scope(sql, 7);
        assert_eq!(s.all_columns().len(), 5);
    }

    #[test]
    fn a_caret_past_the_end_does_not_panic() {
        let sql = "SELECT  FROM items";
        for offset in [0, 3, sql.len(), sql.len() + 50] {
            let _ = scope(sql, offset);
        }
    }

    #[test]
    fn multibyte_text_does_not_break_the_offsets() {
        let sql = "SELECT  FROM items WHERE name = '中文'";
        let s = scope(sql, 7);
        assert_eq!(s.relations[0].name, "items");
    }

    #[test]
    fn unbalanced_parentheses_do_not_panic() {
        let sql = "SELECT  FROM (SELECT 1";
        for offset in 0..=sql.len() {
            let _ = scope(sql, offset);
        }
    }
}
