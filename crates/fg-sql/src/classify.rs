//! Statement classification: read / write / DDL / ..., and "dangerous" detection.
//!
//! **This is best-effort and must never be the only guard.** A SELECT that calls a function with
//! side effects (`pg_terminate_backend()`, `lo_unlink()`) looks like a read. The real protection is
//! the engine-level read-only session (`Session::set_read_only`); classification is the second
//! layer that gives early, readable refusals and powers safe-mode confirmations. When in doubt a
//! statement is classified as something that may write.

use fg_core::{DangerReason, DbKind, StatementClass};

use crate::lex::{Flavor, Kind, lex};
use crate::split::{StatementSpan, split_statements};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementInfo {
    pub class: StatementClass,
    pub danger: Option<DangerReason>,
}

/// A script with every statement classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub statements: Vec<(StatementSpan, StatementInfo)>,
}

impl Analysis {
    /// True if any statement may change data, schema or session state, or could not be classified.
    pub fn may_write(&self) -> bool {
        self.statements.iter().any(|(_, i)| i.class.may_write())
    }

    pub fn first_danger(&self) -> Option<DangerReason> {
        self.statements.iter().find_map(|(_, i)| i.danger)
    }
}

pub fn analyze(sql: &str, kind: DbKind) -> Analysis {
    Analysis {
        statements: split_statements(sql, kind)
            .into_iter()
            .map(|span| {
                let info = classify(span.sql(sql), kind);
                (span, info)
            })
            .collect(),
    }
}

/// A word token with its position in the significant-token list and its paren depth.
struct Word {
    upper: String,
    depth: usize,
}

/// Significant tokens: words (upper-cased) and parens; everything else is `None` in the stream so
/// that adjacency ("ON DUPLICATE KEY UPDATE") can still be checked on words only.
struct Stream {
    items: Vec<Option<Word>>, // None = non-word significant token
}

fn stream(sql: &str, flavor: Flavor) -> Stream {
    let mut depth = 0usize;
    let mut items = Vec::new();
    for t in lex(sql, flavor) {
        if t.is_trivia() {
            continue;
        }
        if t.is_punct(sql, '(') {
            depth += 1;
            items.push(None);
        } else if t.is_punct(sql, ')') {
            depth = depth.saturating_sub(1);
            items.push(None);
        } else if t.kind == Kind::Word {
            items.push(Some(Word {
                upper: t.text(sql).to_ascii_uppercase(),
                depth,
            }));
        } else {
            items.push(None);
        }
    }
    Stream { items }
}

impl Stream {
    fn words(&self) -> impl Iterator<Item = &Word> {
        self.items.iter().flatten()
    }

    fn first(&self) -> Option<&str> {
        self.words().next().map(|w| w.upper.as_str())
    }

    fn contains(&self, w: &str) -> bool {
        self.words().any(|x| x.upper == w)
    }
}

/// Classify one statement (leading comments and parentheses are fine). For a whole script use
/// [`analyze`]. An empty statement is `Unknown`.
pub fn classify(sql: &str, kind: DbKind) -> StatementInfo {
    let flavor = Flavor::from(kind);
    let s = stream(sql, flavor);
    let class = match s.first() {
        None => StatementClass::Unknown,
        Some(first) => class_of(sql, flavor, &s, first),
    };
    // A DELETE inside `CREATE TRIGGER` is a definition, not an action: only statements that
    // actually write are checked for a missing WHERE.
    let dml_danger = if class == StatementClass::Write {
        unqualified_dml(&s)
    } else {
        None
    };
    StatementInfo {
        class,
        danger: dml_danger.or_else(|| ddl_danger(&s)),
    }
}

fn class_of(sql: &str, flavor: Flavor, s: &Stream, first: &str) -> StatementClass {
    use StatementClass::*;
    match first {
        "SELECT" | "VALUES" | "TABLE" => {
            if s.contains("INTO") {
                Write // SELECT INTO creates a table (PostgreSQL) or writes a file/variable (MySQL)
            } else {
                Read
            }
        }
        "SHOW" | "DESCRIBE" | "DESC" => Read,
        "WITH" => {
            let writes = s.words().enumerate().any(|(i, w)| {
                matches!(
                    w.upper.as_str(),
                    "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE"
                ) && !is_clause_use(s, i)
            }) || s.contains("INTO");
            if writes { Write } else { Read }
        }
        "EXPLAIN" => explain_class(sql, flavor, s),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "UPSERT" => Write,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "GRANT" | "REVOKE"
        | "REINDEX" | "CLUSTER" | "SECURITY" => Ddl,
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" | "ABORT" => {
            Transaction
        }
        "SET" | "USE" | "RESET" | "DISCARD" | "ANALYZE" | "ANALYSE" | "VACUUM" | "CHECKPOINT"
        | "PRAGMA" | "LOCK" | "UNLOCK" | "REFRESH" | "ATTACH" | "DETACH" | "COPY" | "OPTIMIZE"
        | "REPAIR" | "FLUSH" | "KILL" | "LISTEN" | "UNLISTEN" | "NOTIFY" | "DEALLOCATE"
        | "LOAD" => Utility,
        // CALL / DO / EXECUTE / PREPARE run arbitrary code: treat as possibly writing.
        _ => Unknown,
    }
}

/// `FOR UPDATE`, `ON DUPLICATE KEY UPDATE`, `ON CONFLICT DO UPDATE`, `ON DELETE CASCADE`,
/// `WHEN MATCHED THEN DELETE` use the words as clauses, not as the statement itself.
fn is_clause_use(s: &Stream, index: usize) -> bool {
    let words: Vec<&Word> = s.words().collect();
    let word = words[index].upper.as_str();
    let prev = index.checked_sub(1).map(|i| words[i].upper.as_str());
    matches!(
        (word, prev),
        ("UPDATE", Some("FOR" | "KEY" | "DO" | "NO")) | ("UPDATE" | "DELETE", Some("ON" | "THEN"))
    )
}

/// `EXPLAIN` only plans, unless `ANALYZE` makes the engine run the statement: then it is as
/// dangerous as the statement itself.
fn explain_class(sql: &str, flavor: Flavor, s: &Stream) -> StatementClass {
    let analyzes = s
        .words()
        .take(6)
        .any(|w| matches!(w.upper.as_str(), "ANALYZE" | "ANALYSE"));
    if !analyzes {
        return StatementClass::Read;
    }
    // Classify the statement being explained.
    let inner = lex(sql, flavor)
        .into_iter()
        .filter(|t| !t.is_trivia() && t.kind == Kind::Word)
        .find(|t| {
            matches!(
                t.text(sql).to_ascii_uppercase().as_str(),
                "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "VALUES" | "REPLACE" | "MERGE"
            )
        });
    match inner {
        Some(t) => {
            let kind = match flavor {
                Flavor::Sqlite => DbKind::Sqlite,
                Flavor::Postgres => DbKind::Postgres,
                Flavor::MySql => DbKind::MySql,
                Flavor::MsSql => DbKind::MsSql,
            };
            classify(&sql[t.start..], kind).class
        }
        None => StatementClass::Unknown,
    }
}

/// `DELETE`/`UPDATE` (at any depth, so writable CTEs are covered) with no `WHERE` at the same depth.
fn unqualified_dml(s: &Stream) -> Option<DangerReason> {
    let words: Vec<&Word> = s.words().collect();
    for (i, w) in words.iter().enumerate() {
        let reason = match w.upper.as_str() {
            "DELETE" => DangerReason::DeleteWithoutWhere,
            "UPDATE" => DangerReason::UpdateWithoutWhere,
            _ => continue,
        };
        if is_clause_use(s, i) {
            continue;
        }
        let has_where = words[i + 1..]
            .iter()
            .take_while(|x| x.depth >= w.depth)
            .any(|x| x.upper == "WHERE" && x.depth == w.depth);
        if !has_where {
            return Some(reason);
        }
    }
    None
}

fn ddl_danger(s: &Stream) -> Option<DangerReason> {
    match s.first()? {
        "DROP" => Some(DangerReason::DropObject),
        "TRUNCATE" => Some(DangerReason::Truncate),
        "ALTER" if s.contains("DROP") => Some(DangerReason::AlterDestructive),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use StatementClass::*;

    use super::*;

    fn class(sql: &str) -> StatementClass {
        classify(sql, DbKind::Postgres).class
    }

    fn danger(sql: &str) -> Option<DangerReason> {
        classify(sql, DbKind::Postgres).danger
    }

    #[test]
    fn reads() {
        for sql in [
            "SELECT 1",
            "  select * from t where a = 1",
            "-- note\nSELECT 1",
            "/* c */ (SELECT 1) UNION (SELECT 2)",
            "VALUES (1), (2)",
            "TABLE t",
            "SHOW TABLES",
            "DESCRIBE t",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT * FROM t FOR UPDATE",
            "EXPLAIN SELECT 1",
            "EXPLAIN QUERY PLAN SELECT 1",
            "EXPLAIN DELETE FROM t",
        ] {
            assert_eq!(class(sql), Read, "{sql}");
        }
    }

    #[test]
    fn writes() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE b = 2",
            "DELETE FROM t WHERE a = 1",
            "REPLACE INTO t VALUES (1)",
            "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE",
            "SELECT * INTO new_t FROM old_t",
            "SELECT 1 INTO OUTFILE '/tmp/x'",
            "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
            "WITH i AS (INSERT INTO t VALUES (1) RETURNING id) SELECT * FROM i",
            "EXPLAIN ANALYZE DELETE FROM t WHERE a = 1",
            "EXPLAIN (ANALYZE, VERBOSE) UPDATE t SET a = 1 WHERE b = 2",
        ] {
            assert_eq!(class(sql), Write, "{sql}");
        }
    }

    #[test]
    fn clauses_named_update_do_not_make_a_select_a_write() {
        assert_eq!(
            class("WITH x AS (SELECT 1) SELECT * FROM x FOR UPDATE"),
            Read
        );
        assert_eq!(
            class("WITH x AS (SELECT 1) SELECT * FROM x FOR NO KEY UPDATE"),
            Read
        );
        assert_eq!(
            class("INSERT INTO t VALUES (1) ON CONFLICT (a) DO UPDATE SET b = 2"),
            Write
        );
    }

    #[test]
    fn explain_analyze_of_a_select_is_still_a_read() {
        assert_eq!(class("EXPLAIN ANALYZE SELECT * FROM t"), Read);
    }

    #[test]
    fn ddl_transactions_and_utility() {
        for sql in [
            "CREATE TABLE t (a int)",
            "ALTER TABLE t ADD COLUMN b int",
            "DROP TABLE t",
            "TRUNCATE t",
            "GRANT SELECT ON t TO u",
            "CREATE OR REPLACE VIEW v AS SELECT 1",
        ] {
            assert_eq!(class(sql), Ddl, "{sql}");
        }
        for sql in [
            "BEGIN",
            "BEGIN IMMEDIATE",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT a",
            "START TRANSACTION",
        ] {
            assert_eq!(class(sql), Transaction, "{sql}");
        }
        for sql in [
            "SET search_path = x",
            "USE db",
            "PRAGMA foreign_keys = ON",
            "VACUUM",
            "ANALYZE t",
            "COPY t TO '/tmp/x'",
            "ATTACH 'a.db' AS a",
        ] {
            assert_eq!(class(sql), Utility, "{sql}");
        }
    }

    #[test]
    fn code_execution_and_nonsense_are_unknown_and_count_as_writes() {
        for sql in [
            "CALL proc()",
            "DO $$ BEGIN DELETE FROM t; END $$",
            "EXECUTE stmt",
            "PREPARE s AS DELETE FROM t",
            "frobnicate everything",
            "",
            "   ",
            "-- nothing",
        ] {
            let c = class(sql);
            assert_eq!(c, Unknown, "{sql}");
            assert!(c.may_write());
        }
    }

    #[test]
    fn dangerous_statements() {
        assert_eq!(
            danger("DELETE FROM t"),
            Some(DangerReason::DeleteWithoutWhere)
        );
        assert_eq!(
            danger("UPDATE t SET a = 1"),
            Some(DangerReason::UpdateWithoutWhere)
        );
        assert_eq!(danger("DROP TABLE t"), Some(DangerReason::DropObject));
        assert_eq!(danger("TRUNCATE TABLE t"), Some(DangerReason::Truncate));
        assert_eq!(
            danger("ALTER TABLE t DROP COLUMN a"),
            Some(DangerReason::AlterDestructive)
        );
        // A WHERE inside a subquery does not protect the outer statement.
        assert_eq!(
            danger("UPDATE t SET a = (SELECT b FROM u WHERE u.id = 1)"),
            Some(DangerReason::UpdateWithoutWhere)
        );
        assert_eq!(
            danger("DELETE FROM t USING u"),
            Some(DangerReason::DeleteWithoutWhere)
        );
        // A writable CTE is covered too.
        assert_eq!(
            danger("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"),
            Some(DangerReason::DeleteWithoutWhere)
        );
    }

    #[test]
    fn statements_with_where_or_that_are_harmless_are_not_flagged() {
        for sql in [
            "DELETE FROM t WHERE a = 1",
            "UPDATE t SET a = 1 WHERE b IN (SELECT id FROM u)",
            "WITH d AS (DELETE FROM t WHERE a = 1 RETURNING *) SELECT * FROM d",
            "INSERT INTO t VALUES (1) ON CONFLICT (a) DO UPDATE SET b = 2",
            "SELECT * FROM t FOR UPDATE",
            "CREATE TABLE t (a int REFERENCES u(id) ON DELETE CASCADE ON UPDATE CASCADE)",
            "ALTER TABLE t ADD COLUMN b int",
            "SELECT 'DROP TABLE t'",
            "-- DROP TABLE t\nSELECT 1",
        ] {
            assert_eq!(danger(sql), None, "{sql}");
        }
        assert_eq!(
            classify(
                "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE b = 2",
                DbKind::MySql
            )
            .danger,
            None
        );
    }

    #[test]
    fn keywords_inside_strings_and_comments_are_ignored() {
        assert_eq!(class("SELECT 'DELETE FROM t'"), Read);
        assert_eq!(class("SELECT 1 /* DROP TABLE t */"), Read);
        assert_eq!(danger("SELECT \"update\" FROM t"), None);
    }

    #[test]
    fn mysql_executable_comments_cannot_hide_a_statement() {
        let i = classify("/*!50000 DROP TABLE t */", DbKind::MySql);
        assert_eq!((i.class, i.danger), (Ddl, Some(DangerReason::DropObject)));
    }

    #[test]
    fn scripts_are_analyzed_statement_by_statement() {
        let a = analyze("SELECT 1; SELECT 2", DbKind::Sqlite);
        assert!(!a.may_write());
        let a = analyze("SELECT 1; DROP TABLE t; -- done", DbKind::Sqlite);
        assert!(a.may_write());
        assert_eq!(a.first_danger(), Some(DangerReason::DropObject));
        assert_eq!(a.statements.len(), 2);
        // The classic smuggling attempts.
        assert!(analyze("SELECT 1;DELETE FROM t", DbKind::Postgres).may_write());
        assert!(analyze("SELECT 'a'; /* x */ UPDATE t SET a = 1", DbKind::MySql).may_write());
    }

    #[test]
    fn sqlite_trigger_creation_is_ddl() {
        let a = analyze(
            "CREATE TRIGGER t AFTER INSERT ON a BEGIN DELETE FROM b; END",
            DbKind::Sqlite,
        );
        assert_eq!(a.statements.len(), 1);
        assert_eq!(a.statements[0].1.class, Ddl);
        assert_eq!(
            a.first_danger(),
            None,
            "a DELETE in a trigger body is a definition"
        );
    }

    #[test]
    fn merge_clauses_are_not_separate_statements() {
        let sql = "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN UPDATE SET b = 1 WHEN NOT MATCHED THEN INSERT VALUES (1)";
        assert_eq!(classify(sql, DbKind::Postgres).danger, None);
    }
}
