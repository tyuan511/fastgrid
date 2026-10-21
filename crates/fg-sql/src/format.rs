//! SQL formatting: re-indent and re-break a statement without changing what it means.
//!
//! Deliberately conservative. A formatter that reflows an expression it half-understands is worse
//! than no formatter, so this one only does things it can do without a parse tree:
//!
//! * puts each top-level clause (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, …) on its own line,
//! * indents the contents of parentheses by one level and breaks after a comma inside them,
//! * normalises keyword case and collapses runs of whitespace,
//! * leaves string literals, quoted identifiers and comments byte-for-byte as they were.
//!
//! It never moves a comment away from the token it was written next to, never reorders tokens, and
//! never drops anything. Anything it cannot lay out tidily is emitted on the current line, which
//! keeps the output valid SQL even when the input is odd.

use fg_core::DbKind;

use crate::lex::{Flavor, Kind, Token, lex};

/// How many spaces one indent level is.
const INDENT: usize = 4;

/// Clause keywords that start a new line at the current indent level.
const CLAUSES: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "HAVING",
    "ORDER BY",
    "LIMIT",
    "OFFSET",
    "UNION ALL",
    "UNION",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "WITH",
];

/// Keywords that stay on one line with what follows them.
const KEEP_CASE: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "AS",
    "ON",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "DISTINCT",
    "ASC",
    "DESC",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "UNION",
    "ALL",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "RETURNING",
    "WITH",
    "CREATE",
    "TABLE",
    "ALTER",
    "DROP",
    "ADD",
    "COLUMN",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "NOT",
    "DEFAULT",
    "UNIQUE",
    "INDEX",
    "VIEW",
    "EXISTS",
    "IF",
    "TRUNCATE",
    "REPLACE",
    "TRUE",
    "FALSE",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "CAST",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "EXPLAIN",
];

/// Format one script. Statements are separated by a blank line; the trailing newline is kept as the
/// input had it (formatted output always ends with exactly one).
pub fn format_sql(sql: &str, kind: DbKind) -> String {
    let flavor = Flavor::from(kind);
    let tokens = lex(sql, flavor);
    let mut out = Formatter::default();
    for (index, token) in tokens.iter().enumerate() {
        out.push(token, sql, &tokens, index);
    }
    out.finish()
}

/// Does this script need no formatting? The editor uses it to disable the menu item.
pub fn is_formatted(sql: &str, kind: DbKind) -> bool {
    format_sql(sql, kind) == sql
}

struct Formatter {
    out: String,
    /// Current parenthesis depth; also the indent level for wrapped lines.
    depth: usize,
    /// True when the last thing written was a line break, so the next token must not add a space.
    at_line_start: bool,
    /// Set right after a comma inside parentheses: the next token starts a new line.
    break_before_next: bool,
    /// True when nothing has been written yet, so the first clause does not get a leading break.
    empty: bool,
    /// Set when a two-word clause (`ORDER BY`) already consumed the following word.
    skip_next_word: bool,
}

impl Default for Formatter {
    fn default() -> Self {
        Self {
            out: String::new(),
            depth: 0,
            at_line_start: true,
            break_before_next: false,
            empty: true,
            skip_next_word: false,
        }
    }
}

impl Formatter {
    fn push(&mut self, token: &Token, sql: &str, tokens: &[Token], index: usize) {
        let text = token.text(sql);
        match token.kind {
            Kind::Whitespace => {
                // Whitespace is re-derived; only a blank line between statements survives, and
                // that is handled by the `;` branch below.
            }
            Kind::LineComment | Kind::BlockComment => {
                if !self.at_line_start {
                    self.out.push(' ');
                }
                self.out.push_str(text);
                self.wrote();
                if token.kind == Kind::LineComment {
                    self.newline();
                } else {
                    self.out.push(' ');
                    self.at_line_start = false;
                }
            }
            Kind::String | Kind::DollarString | Kind::QuotedIdent | Kind::ExecCommentMark => {
                self.word(text, false, sql, tokens, index);
            }
            Kind::Number => self.word(text, false, sql, tokens, index),
            Kind::Punct => self.punct(text),
            // A function name is not a keyword: `count(` must stay lower case and stay tight.
            Kind::Word => {
                let call = Self::next_is_paren(tokens, sql, index);
                self.word(text, !call, sql, tokens, index);
            }
        }
    }

    /// The clause this word starts, if any, joining a following `BY` / `ALL` so the two-word clauses
    /// (`ORDER BY`, `GROUP BY`, `UNION ALL`) are recognised from the token stream.
    fn clause_at(tokens: &[Token], sql: &str, index: usize) -> Option<String> {
        let first = tokens[index].text(sql).to_ascii_uppercase();
        let second = tokens
            .iter()
            .skip(index + 1)
            .find(|t| t.kind != Kind::Whitespace)
            .map(|t| t.text(sql).to_ascii_uppercase());
        let joined = match (first.as_str(), second.as_deref()) {
            ("ORDER" | "GROUP", Some("BY")) => format!("{first} BY"),
            ("UNION", Some("ALL")) => "UNION ALL".to_owned(),
            ("INSERT", Some("INTO")) => "INSERT INTO".to_owned(),
            ("DELETE", Some("FROM")) => "DELETE FROM".to_owned(),
            _ => first,
        };
        CLAUSES.contains(&joined.as_str()).then_some(joined)
    }

    /// True when the next non-whitespace token is `(`: this word is a function name, so it keeps its
    /// case and sits tight against the parenthesis.
    fn next_is_paren(tokens: &[Token], sql: &str, index: usize) -> bool {
        tokens
            .iter()
            .skip(index + 1)
            .find(|t| t.kind != Kind::Whitespace)
            .is_some_and(|t| t.kind == Kind::Punct && t.text(sql) == "(")
    }

    fn punct(&mut self, text: &str) {
        match text {
            "(" => {
                // A call sits tight against its parenthesis: `count(`, not `count (`. Decide after
                // dropping the space the previous word left behind.
                self.trim_trailing_space();
                let call = self
                    .out
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"' || c == '`');
                if !call && !self.at_line_start {
                    self.out.push(' ');
                }
                self.out.push('(');
                self.wrote();
                self.depth += 1;
                self.at_line_start = false;
            }
            ")" => {
                self.depth = self.depth.saturating_sub(1);
                self.trim_trailing_space();
                self.out.push(')');
                self.wrote();
                self.at_line_start = false;
            }
            "," => {
                self.trim_trailing_space();
                self.out.push(',');
                self.wrote();
                if self.depth > 0 {
                    self.break_before_next = true;
                }
                // At depth 0 the following word supplies the space; pushing one here doubles it.
                self.at_line_start = false;
            }
            ";" => {
                self.trim_trailing_space();
                self.out.push(';');
                self.wrote();
                // A blank line between statements, but never at the very end.
                self.out.push('\n');
                self.out.push('\n');
                self.at_line_start = true;
                self.break_before_next = false;
            }
            "." => {
                self.trim_trailing_space();
                self.out.push('.');
                self.wrote();
                self.at_line_start = false;
            }
            _ => {
                // No space right after `(`: `count(*)`, not `count( *`.
                let after_open = self.out.ends_with('(');
                if !self.at_line_start && !after_open {
                    self.out.push(' ');
                }
                self.out.push_str(text);
                self.wrote();
                self.at_line_start = false;
            }
        }
    }

    /// A word: keyword, identifier or literal. `keyword_case` decides whether its case is normalised.
    fn word(&mut self, text: &str, maybe_keyword: bool, sql: &str, tokens: &[Token], index: usize) {
        if self.skip_next_word {
            self.skip_next_word = false;
            return;
        }
        if self.break_before_next {
            self.break_before_next = false;
            self.newline();
        }
        let upper = text.to_ascii_uppercase();
        if let Some(clause) = maybe_keyword
            .then(|| Self::clause_at(tokens, sql, index))
            .flatten()
        {
            // A clause starts a line, unless it is the first thing written.
            if !self.empty {
                self.newline();
            }
            self.write_indent();
            self.out.push_str(&clause);
            self.wrote();
            self.at_line_start = false;
            // A two-word clause consumed the following word: skip it on the next iteration.
            if clause.contains(' ') {
                self.skip_next_word = true;
            }
            return;
        }
        if self.at_line_start {
            self.write_indent();
        } else if !self.out.ends_with('(') {
            // No space right after `(`: `coalesce(a`, not `coalesce( a`.
            self.out.push(' ');
        }
        if maybe_keyword && KEEP_CASE.contains(&upper.as_str()) {
            self.out.push_str(&upper);
        } else {
            self.out.push_str(text);
        }
        self.wrote();
        self.at_line_start = false;
    }

    fn wrote(&mut self) {
        self.empty = false;
    }

    fn newline(&mut self) {
        self.trim_trailing_space();
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.at_line_start = true;
    }

    fn write_indent(&mut self) {
        if !self.at_line_start {
            return;
        }
        // A top-level statement gets no indent; wrapped lines inside parentheses get one level.
        let spaces = self.depth.min(4) * INDENT;
        for _ in 0..spaces {
            self.out.push(' ');
        }
    }

    fn trim_trailing_space(&mut self) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
    }

    fn finish(mut self) -> String {
        self.trim_trailing_space();
        // Collapse the blank line a trailing `;` left, then end with exactly one newline.
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sql: &str) -> String {
        format_sql(sql, DbKind::Sqlite)
    }

    #[test]
    fn clauses_go_on_their_own_lines() {
        let out = fmt("select id, name from items where id > 3 order by name");
        assert_eq!(
            out,
            "SELECT id, name\nFROM items\nWHERE id > 3\nORDER BY name\n"
        );
    }

    #[test]
    fn keywords_are_upper_cased_and_identifiers_are_not() {
        let out = fmt("select Name from Items");
        assert_eq!(out, "SELECT Name\nFROM Items\n");
    }

    #[test]
    fn a_comma_inside_parentheses_breaks_the_line_and_indents() {
        let out = fmt("select count(*), coalesce(a, b, c) from t");
        assert_eq!(
            out,
            "SELECT count(*), coalesce(a,\n    b,\n    c)\nFROM t\n"
        );
    }

    #[test]
    fn a_function_call_stays_tight() {
        assert_eq!(fmt("select count(*) from t"), "SELECT count(*)\nFROM t\n");
    }

    #[test]
    fn string_literals_are_untouched() {
        let sql = "select 'a  b' from t where x = 'FROM  WHERE'";
        let out = fmt(sql);
        assert!(out.contains("'a  b'"), "inner whitespace survives: {out}");
        assert!(
            out.contains("'FROM  WHERE'"),
            "keywords inside a string are not re-cased or split: {out}"
        );
    }

    #[test]
    fn comments_stay_with_their_line() {
        let out = fmt("select id -- the key\nfrom items");
        assert_eq!(out, "SELECT id -- the key\nFROM items\n");
    }

    #[test]
    fn a_block_comment_survives() {
        let out = fmt("select /* keep me */ id from t");
        assert!(out.contains("/* keep me */"), "{out}");
    }

    #[test]
    fn statements_are_separated_by_a_blank_line() {
        let out = fmt("select 1; select 2;");
        assert_eq!(out, "SELECT 1;\n\nSELECT 2;\n");
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        let once = fmt("select count(*), coalesce(a, b) from t where x=1 order by y desc");
        let twice = fmt(&once);
        assert_eq!(once, twice, "the formatter is idempotent");
        assert!(is_formatted(&once, DbKind::Sqlite));
    }

    #[test]
    fn already_formatted_text_is_reported_as_such() {
        assert!(is_formatted("SELECT 1;\n", DbKind::Sqlite));
        assert!(!is_formatted("select 1", DbKind::Sqlite));
    }

    #[test]
    fn empty_input_is_left_empty() {
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("   "), "");
        assert_eq!(fmt("\n\n"), "");
    }

    #[test]
    fn quoted_identifiers_and_multibyte_text_survive() {
        let out = fmt("select \"odd column\", `back tick`, '中文' from 表");
        assert!(out.contains("\"odd column\""), "{out}");
        assert!(out.contains("`back tick`"), "{out}");
        assert!(out.contains("'中文'"), "{out}");
        assert!(out.contains("表"), "{out}");
    }

    #[test]
    fn a_trailing_comment_does_not_swallow_the_next_statement() {
        let out = fmt("select 1; -- done\nselect 2;");
        assert_eq!(out, "SELECT 1;\n\n-- done\nSELECT 2;\n");
    }

    #[test]
    fn unbalanced_parentheses_do_not_panic_or_lose_text() {
        let out = fmt("select (a, b from t");
        assert!(out.contains("a"), "{out}");
        assert!(out.contains("b"), "{out}");
    }

    #[test]
    fn mysql_backtick_identifiers_are_kept() {
        let out = format_sql("select `select` from `from`", DbKind::MySql);
        assert!(out.contains("`select`"), "{out}");
        assert!(out.contains("`from`"), "{out}");
    }
}
