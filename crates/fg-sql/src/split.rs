//! Split a script into statements, and find the statement under the cursor.
//!
//! Splitting is lexical: a `;` ends a statement unless it sits inside a string, quoted identifier,
//! comment, dollar-quoted body, parentheses, or the body of a trigger. Not supported yet: MySQL's
//! client-side `DELIMITER` command and compound `CREATE PROCEDURE ... BEGIN ... END` bodies
//! (M5, together with the MySQL driver).

use std::ops::Range;

use fg_core::DbKind;

use crate::lex::{Flavor, Kind, Token, lex};

/// One statement of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// The statement text: from its first token (a leading comment counts) to its last, without
    /// the terminating `;`.
    pub text: Range<usize>,
    /// Everything attributed to it: from the end of the previous statement through its `;`.
    /// Used to find which statement the cursor is in.
    pub extent: Range<usize>,
}

impl StatementSpan {
    pub fn sql<'a>(&self, script: &'a str) -> &'a str {
        &script[self.text.clone()]
    }
}

pub fn split_statements(sql: &str, kind: DbKind) -> Vec<StatementSpan> {
    let flavor = Flavor::from(kind);
    let tokens = lex(sql, flavor);
    let mut out = Vec::new();

    let mut extent_start = 0usize;
    let mut first_any: Option<usize> = None; // first non-whitespace token (a comment counts)
    let mut first_sig: Option<usize> = None; // index of the first non-trivia token
    let mut last_sig: Option<usize> = None;
    let mut depth = 0usize;

    // Trigger bodies contain `;` and end with a lone `END`.
    let mut words: Vec<&str> = Vec::new(); // first few words of the statement
    let mut in_trigger_body = false;
    let mut at_inner_start = false;
    let triggers_have_bodies = matches!(flavor, Flavor::Sqlite | Flavor::MySql);

    for (idx, tok) in tokens.iter().enumerate() {
        if tok.kind != Kind::Whitespace {
            first_any.get_or_insert(idx);
        }
        if tok.is_trivia() {
            continue;
        }
        let ends_statement = tok.is_punct(sql, ';') && depth == 0 && !in_trigger_body;

        if ends_statement {
            if let (Some(f), Some(l)) = (first_any.filter(|_| first_sig.is_some()), last_sig) {
                out.push(StatementSpan {
                    text: tokens[f].start..tokens[l].end,
                    extent: extent_start..tok.end,
                });
            }
            extent_start = tok.end;
            first_any = None;
            first_sig = None;
            last_sig = None;
            words.clear();
            in_trigger_body = false;
            at_inner_start = false;
            continue;
        }

        first_sig.get_or_insert(idx);
        last_sig = Some(idx);

        if tok.is_punct(sql, '(') {
            depth += 1;
        } else if tok.is_punct(sql, ')') {
            depth = depth.saturating_sub(1);
        }

        if tok.kind == Kind::Word {
            if words.len() < 8 {
                words.push(tok.text(sql));
            }
            if triggers_have_bodies {
                trigger_state(sql, tok, &words, &mut in_trigger_body, &mut at_inner_start);
            }
        } else if in_trigger_body {
            at_inner_start = tok.is_punct(sql, ';');
        }
    }

    if let (Some(f), Some(l)) = (first_any.filter(|_| first_sig.is_some()), last_sig) {
        out.push(StatementSpan {
            text: tokens[f].start..tokens[l].end,
            extent: extent_start..sql.len(),
        });
    }
    out
}

/// Track `CREATE [TEMP] TRIGGER ... BEGIN <statements>; END`.
fn trigger_state(
    sql: &str,
    tok: &Token,
    words: &[&str],
    in_body: &mut bool,
    at_inner_start: &mut bool,
) {
    let w = tok.text(sql);
    let is_create_trigger = words
        .first()
        .is_some_and(|f| f.eq_ignore_ascii_case("create"))
        && words
            .iter()
            .skip(1)
            .take(6)
            .any(|x| x.eq_ignore_ascii_case("trigger"));
    if !is_create_trigger {
        return;
    }
    if *in_body {
        if *at_inner_start && w.eq_ignore_ascii_case("end") {
            *in_body = false;
        }
        *at_inner_start = false;
    } else if w.eq_ignore_ascii_case("begin") {
        *in_body = true;
        *at_inner_start = true;
    }
}

/// Index of the statement the cursor (`offset`, a byte offset) is in. A cursor in the gap after
/// a statement's `;` belongs to the statement just before it (what you just ran), unless that gap
/// is at the very start. Returns None for an empty script.
pub fn statement_at(spans: &[StatementSpan], offset: usize) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }
    if let Some(i) = spans
        .iter()
        .position(|s| offset >= s.text.start && offset <= s.extent.end)
    {
        // `extent.end` of statement i equals `extent.start` of i+1: prefer the earlier one only
        // when the cursor is not already inside the next statement's text.
        if i + 1 < spans.len() && offset >= spans[i + 1].text.start {
            return Some(i + 1);
        }
        return Some(i);
    }
    // In leading whitespace/comments before a statement: that statement.
    spans
        .iter()
        .position(|s| offset < s.text.start)
        .or(Some(spans.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str, kind: DbKind) -> Vec<String> {
        split_statements(sql, kind)
            .iter()
            .map(|s| s.sql(sql).to_owned())
            .collect()
    }

    #[test]
    fn splits_on_semicolons_and_drops_empty_statements() {
        assert_eq!(
            texts("SELECT 1; SELECT 2;; ;\n SELECT 3", DbKind::Sqlite),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
        assert!(texts("  ;;  \n", DbKind::Sqlite).is_empty());
        assert!(texts("-- only a comment\n/* and another */", DbKind::Sqlite).is_empty());
        assert!(texts("", DbKind::Sqlite).is_empty());
    }

    #[test]
    fn semicolons_in_strings_idents_and_comments_do_not_split() {
        let sql = "SELECT 'a;b', \"c;d\", `e;f` -- g;\n, /* h; */ 1; SELECT 2";
        let t = texts(sql, DbKind::Sqlite);
        assert_eq!(t.len(), 2);
        assert!(t[0].contains("'a;b'") && t[0].contains("-- g;"));
    }

    #[test]
    fn dollar_quoted_bodies_stay_whole() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql; SELECT f()";
        let t = texts(sql, DbKind::Postgres);
        assert_eq!(t.len(), 2);
        assert!(t[0].ends_with("LANGUAGE plpgsql"));
        assert_eq!(t[1], "SELECT f()");
    }

    #[test]
    fn backslash_escapes_follow_the_engine() {
        let sql = r"SELECT 'a\'; SELECT 2";
        assert_eq!(
            texts(sql, DbKind::MySql).len(),
            1,
            "MySQL: the quote is escaped, string continues"
        );
        assert_eq!(
            texts(sql, DbKind::Postgres).len(),
            2,
            "PostgreSQL: the string ended at the backslash"
        );
    }

    #[test]
    fn sqlite_trigger_bodies_are_one_statement() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON a BEGIN INSERT INTO log VALUES (1); UPDATE a SET x = CASE WHEN 1 THEN 2 END; END; SELECT 1";
        let t = texts(sql, DbKind::Sqlite);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("CREATE TRIGGER") && t[0].ends_with("END"));
        assert_eq!(t[1], "SELECT 1");
    }

    #[test]
    fn a_begin_transaction_is_not_a_trigger_body() {
        let t = texts("BEGIN; INSERT INTO t VALUES (1); COMMIT;", DbKind::Sqlite);
        assert_eq!(t, ["BEGIN", "INSERT INTO t VALUES (1)", "COMMIT"]);
    }

    #[test]
    fn semicolons_inside_parentheses_do_not_split() {
        assert_eq!(texts("SELECT (1; SELECT 2", DbKind::Sqlite).len(), 1);
    }

    #[test]
    fn leading_comments_belong_to_their_statement() {
        let sql = "-- first\nSELECT 1;\n-- second\nSELECT 2;";
        let t = texts(sql, DbKind::Sqlite);
        assert_eq!(t, ["-- first\nSELECT 1", "-- second\nSELECT 2"]);
    }

    #[test]
    fn spans_index_back_into_the_original_text() {
        let sql = "SELECT 'é;世界'; SELECT 2";
        let spans = split_statements(sql, DbKind::Sqlite);
        assert_eq!(spans[0].sql(sql), "SELECT 'é;世界'");
        assert_eq!(&sql[spans[0].extent.clone()], "SELECT 'é;世界';");
        assert_eq!(spans[1].sql(sql), "SELECT 2");
    }

    #[test]
    fn unterminated_input_still_yields_the_tail() {
        assert_eq!(
            texts("SELECT 1; SELECT 'oops", DbKind::Sqlite),
            ["SELECT 1", "SELECT 'oops"]
        );
    }

    #[test]
    fn statement_at_cursor() {
        let sql = "SELECT 1;\n\nSELECT 2;  \n-- c\nSELECT 3";
        let spans = split_statements(sql, DbKind::Sqlite);
        let at =
            |needle: &str, delta: usize| statement_at(&spans, sql.find(needle).unwrap() + delta);
        assert_eq!(at("SELECT 1", 3), Some(0));
        assert_eq!(at("SELECT 2", 0), Some(1));
        assert_eq!(at("SELECT 2", 8), Some(1), "right after the semicolon");
        assert_eq!(at("SELECT 3", 7), Some(2));
        // Blank line between two statements: the one that follows.
        assert_eq!(statement_at(&spans, sql.find("\n\n").unwrap() + 1), Some(1));
        // End of the text.
        assert_eq!(statement_at(&spans, sql.len()), Some(2));
        assert_eq!(statement_at(&[], 0), None);
    }
}
