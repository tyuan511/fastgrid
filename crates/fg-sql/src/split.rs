//! Split a script into statements, and find the statement under the cursor.
//!
//! Splitting is lexical: a `;` ends a statement unless it sits inside a string, quoted identifier,
//! comment, dollar-quoted body, parentheses, or a compound body (`CREATE ... BEGIN ... END`).
//! MySQL's client-side `DELIMITER` command is honored: a `DELIMITER $$` line switches the
//! terminator until another `DELIMITER` line switches it back. The directive itself is never
//! emitted as a statement — the server would reject it.
//!
//! Compound bodies are tracked with a stack, not a single flag: a procedure body can nest
//! `BEGIN ... END` blocks (handlers) and mix in `IF ... END IF`, `CASE ... END`, `LOOP ... END
//! LOOP`, `WHILE ... END WHILE`, and `REPEAT ... END REPEAT`. A bare `END` closes the body only
//! when it pops the last open block; the second word of a two-word closer (`END IF`) is consumed
//! as part of the closer, never as a new block.

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

/// A block open inside a compound body. `Begin` is the body's own block;
/// the rest close with a two-word `END <word>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Begin,
    If,
    Case,
    Loop,
    While,
    Repeat,
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

    // Compound bodies (`CREATE ... BEGIN ... END`) and the MySQL terminator.
    let mut words: Vec<&str> = Vec::new(); // first words of the statement
    let mut compound = false; // this statement is a CREATE ... body
    let mut stack: Vec<Block> = Vec::new();
    let mut after_end = false; // the next word completes a two-word closer
    let mut delimiter = ";".to_owned();
    let mut skip_until = 0usize; // byte offset: tokens starting before it are ignored
    let bodies_have_blocks = matches!(flavor, Flavor::Sqlite | Flavor::MySql);

    for (idx, tok) in tokens.iter().enumerate() {
        if tok.start < skip_until {
            continue;
        }
        if tok.kind != Kind::Whitespace {
            first_any.get_or_insert(idx);
        }
        if tok.is_trivia() {
            continue;
        }

        // MySQL's client-side DELIMITER: only at a statement boundary, never
        // inside a body, and never emitted. The delimiter is the rest of the
        // line, trimmed; an empty one leaves the terminator unchanged.
        if flavor == Flavor::MySql
            && first_sig.is_none()
            && stack.is_empty()
            && tok.kind == Kind::Word
            && tok.text(sql).eq_ignore_ascii_case("delimiter")
        {
            let rest = &sql[tok.end..];
            let line_end = rest.find('\n').map(|i| tok.end + i).unwrap_or(sql.len());
            let new_delimiter = sql[tok.end..line_end].trim();
            if !new_delimiter.is_empty() {
                delimiter = new_delimiter.to_owned();
            }
            extent_start = line_end;
            skip_until = line_end;
            first_any = None;
            first_sig = None;
            last_sig = None;
            words.clear();
            continue;
        }

        // A word glued to a multi-character terminator (`END$$`): `$` is a
        // word character for MySQL, so the lexer keeps `END` and `$$` in one
        // token and the plain terminator check below never sees the `$$`.
        // Only a body's closing END glues this way in real scripts; treat
        // the prefix as the END (popping the block) and the suffix as the
        // terminator. The statement text runs to the end of the prefix.
        let glued_end = delimiter != ";"
            && depth == 0
            && tok.kind == Kind::Word
            && tok
                .text(sql)
                .strip_suffix(delimiter.as_str())
                .is_some_and(|prefix| !prefix.is_empty() && prefix.eq_ignore_ascii_case("end"));
        if glued_end {
            stack.pop();
            let prefix_len = tok.text(sql).len() - delimiter.len();
            let text_end = tok.start + prefix_len;
            if first_sig.is_some()
                && let Some(f) = first_any
            {
                out.push(StatementSpan {
                    text: tokens[f].start..text_end,
                    extent: extent_start..tok.end,
                });
            }
            extent_start = tok.end;
            skip_until = tok.end;
            first_any = None;
            first_sig = None;
            last_sig = None;
            words.clear();
            compound = false;
            stack.clear();
            after_end = false;
            continue;
        }

        let ends_statement = if delimiter == ";" {
            tok.is_punct(sql, ';') && depth == 0 && stack.is_empty()
        } else {
            // A multi-character terminator (`$$`, `//`): the source text at
            // this token starts with it. Strings and comments are single
            // tokens, so a match here cannot sit inside one; `//` cannot
            // appear as an operator, and `$` never starts a word token.
            sql[tok.start..].starts_with(delimiter.as_str()) && depth == 0 && stack.is_empty()
        };

        if ends_statement {
            let extent_end = if delimiter == ";" {
                tok.end
            } else {
                tok.start + delimiter.len()
            };
            if let (Some(f), Some(l)) = (first_any.filter(|_| first_sig.is_some()), last_sig) {
                out.push(StatementSpan {
                    text: tokens[f].start..tokens[l].end,
                    extent: extent_start..extent_end,
                });
            }
            extent_start = extent_end;
            skip_until = extent_end;
            first_any = None;
            first_sig = None;
            last_sig = None;
            words.clear();
            compound = false;
            stack.clear();
            after_end = false;
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
            if words.len() < 12 {
                words.push(tok.text(sql));
            }
            if bodies_have_blocks {
                block_state(
                    sql,
                    &tokens,
                    idx,
                    &words,
                    &mut compound,
                    &mut stack,
                    &mut after_end,
                );
            }
        } else {
            // A significant non-word token between `END` and its second
            // word cannot happen in valid SQL, so the closer is over.
            after_end = false;
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

/// Track compound bodies: `CREATE ... TRIGGER | PROCEDURE | FUNCTION ... BEGIN ... END`.
///
/// Detection looks at the statement's opening words (`CREATE` first, then a
/// body keyword within the next eleven words, so `DEFINER = ...` clauses do
/// not hide it). Once the body's `BEGIN` opens, every block keyword pushes
/// and every `END` pops; the body ends when the stack empties. Outside a
/// body, block words are ignored — a top-level `BEGIN` is a transaction, and
/// a top-level `CASE ... END` is an expression.
fn block_state(
    sql: &str,
    tokens: &[Token],
    idx: usize,
    words: &[&str],
    compound: &mut bool,
    stack: &mut Vec<Block>,
    after_end: &mut bool,
) {
    let tok = &tokens[idx];
    let w = tok.text(sql);
    if *after_end {
        // The second word of `END IF` / `END CASE` / `END LOOP` /
        // `END WHILE` / `END REPEAT`: part of the closer, not a new block.
        *after_end = false;
        return;
    }
    if stack.is_empty() && !*compound {
        let is_create_body = words
            .first()
            .is_some_and(|f| f.eq_ignore_ascii_case("create"))
            && words.iter().skip(1).take(11).any(|x| {
                x.eq_ignore_ascii_case("trigger")
                    || x.eq_ignore_ascii_case("procedure")
                    || x.eq_ignore_ascii_case("function")
            });
        if is_create_body {
            *compound = true;
        }
        return;
    }
    if !*compound {
        return;
    }
    if w.eq_ignore_ascii_case("begin") {
        stack.push(Block::Begin);
    } else if w.eq_ignore_ascii_case("end") {
        stack.pop();
        *after_end = true;
    } else if w.eq_ignore_ascii_case("if") {
        // `IF(...)` is a function call, not a statement block: look at the
        // next significant token.
        let is_function = tokens[idx + 1..]
            .iter()
            .find(|t| !t.is_trivia())
            .is_some_and(|t| t.is_punct(sql, '('));
        if !is_function {
            stack.push(Block::If);
        }
    } else if w.eq_ignore_ascii_case("case") {
        stack.push(Block::Case);
    } else if w.eq_ignore_ascii_case("loop") {
        stack.push(Block::Loop);
    } else if w.eq_ignore_ascii_case("while") {
        stack.push(Block::While);
    } else if w.eq_ignore_ascii_case("repeat") {
        stack.push(Block::Repeat);
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
    fn mysql_procedure_bodies_are_one_statement() {
        let sql = "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END; SELECT 3";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("CREATE PROCEDURE") && t[0].ends_with("END"));
        assert_eq!(t[1], "SELECT 3");
    }

    #[test]
    fn nested_blocks_and_handlers_stay_whole() {
        let sql = "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLEXCEPTION BEGIN ROLLBACK; END; INSERT INTO t VALUES (1); END; SELECT 1";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END"));
    }

    #[test]
    fn if_blocks_do_not_close_the_body_early() {
        // The old single-flag tracking closed the body at `END IF`; the
        // stack keeps the outer BEGIN open until its own END.
        let sql =
            "CREATE PROCEDURE p() BEGIN IF TRUE THEN SELECT 1; END IF; SELECT 2; END; SELECT 3";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END"));
        assert_eq!(t[1], "SELECT 3");
    }

    #[test]
    fn if_function_calls_are_not_blocks() {
        let sql = "CREATE PROCEDURE p() BEGIN SET x = IF(a, 1, 2); SELECT x; END; SELECT 1";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
    }

    #[test]
    fn mysql_trigger_with_if_stays_whole() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON a BEGIN IF NEW.x > 0 THEN INSERT INTO log VALUES (1); END IF; END; SELECT 1";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
    }

    #[test]
    fn delimiter_switches_the_terminator() {
        let sql = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("CREATE PROCEDURE") && t[0].ends_with("END"));
        assert_eq!(t[1], "SELECT 2");
    }

    #[test]
    fn delimiter_slashes_work_for_functions() {
        let sql = "DELIMITER //\nCREATE FUNCTION f() RETURNS INT BEGIN RETURN 1; END//\nDELIMITER ;\nSELECT f()";
        let t = texts(sql, DbKind::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[1], "SELECT f()");
    }

    #[test]
    fn delimiter_is_mysql_only() {
        // Other engines have no DELIMITER command: the line stays a
        // statement like any other (and the server will reject it).
        // `//` avoids Postgres dollar-quoting, which would swallow `$$`.
        let t = texts("DELIMITER //; SELECT 1;", DbKind::Postgres);
        assert_eq!(t, ["DELIMITER //", "SELECT 1"]);
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
