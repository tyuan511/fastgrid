//! Turning a failed run into a diagnostic the editor can paint.
//!
//! The gap this closes: `fg_driver::Event` reports an error with a `QueryError.position` that is a
//! byte offset **inside the statement that failed**, but nothing said *which* statement that was.
//! The editor therefore could only underline a single-statement run, and a script that failed on
//! its third statement got a message with no location.
//!
//! The answer does not need the driver to change. `fg-sql::split_statements` already knows where
//! every statement starts in the script, and the service runs a script's statements in order and
//! stops at the first failure - so the failing statement is the first one that did not produce a
//! `ResultEnd`. This module does that arithmetic, in one place, with tests.

use std::ops::Range;

use fg_core::{DbKind, QueryError};
use fg_sql::{StatementSpan, split_statements};

/// A problem to paint under a range of the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The bytes to underline, absolute in the script.
    pub range: Range<usize>,
    pub message: String,
    /// Native code or SQLSTATE, when the engine gave one.
    pub code: Option<String>,
    /// Which statement of the script failed, 0-based. `None` for a problem that is not about one
    /// statement (a connection error, a refused script).
    pub statement: Option<usize>,
}

impl Diagnostic {
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

/// Which statements of `script` actually ran, given how many produced a result set.
///
/// The service streams one `ResultStart` per statement in order and stops at the first error, so
/// `started` is the count of statements that began. The failing one is the next after those that
/// completed.
pub fn failing_statement(script: &str, kind: DbKind, started: usize) -> Option<usize> {
    let spans = split_statements(script, kind);
    if spans.is_empty() {
        return None;
    }
    // `started` statements began; if the script has more than that, the next one is the failure.
    // When every statement started, the failure belongs to the last one (it errored mid-flight).
    Some(started.min(spans.len().saturating_sub(1)))
}

/// Build the diagnostic for a failed statement.
///
/// `script` is the whole buffer, `started` is how many statements produced a `ResultStart`, and
/// `error` is what the engine said. The returned range is absolute, so the editor can underline it
/// without knowing anything about statements.
pub fn diagnostic_for(
    script: &str,
    kind: DbKind,
    started: usize,
    error: &QueryError,
) -> Option<Diagnostic> {
    let spans = split_statements(script, kind);
    let index = failing_statement(script, kind, started)?;
    let span = spans.get(index)?;
    let range = underline_range(script, span, error.position);
    Some(Diagnostic {
        range,
        message: error.message.clone(),
        code: error.code.clone(),
        statement: Some(index),
    })
}

/// Where inside `span` to underline.
///
/// The engine's position is relative to the statement, so it is shifted by the statement's start.
/// When there is no position, the whole statement is underlined - better than nothing, and it tells
/// the user which statement failed.
fn underline_range(script: &str, span: &StatementSpan, position: Option<usize>) -> Range<usize> {
    let start = span.text.start;
    let end = span.text.end;
    let Some(position) = position else {
        return start..end;
    };
    let at = start.saturating_add(position).min(end.max(start));
    let at = floor_boundary(script, at);
    // Underline the token starting at the position, not a single character: a one-character
    // squiggle reads as a typo in the editor chrome.
    let rest = &script[at..end.max(at)];
    let len = token_len(rest).max(punct_run_len(rest)).max(1);
    at..at + len.min(rest.len().max(1))
}

/// Step back to a char boundary so a range never splits a UTF-8 character.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Length in bytes of the run of punctuation at the front of `text` (`--`, `/*`, `)`), so a comment
/// or an operator is underlined as itself rather than one byte.
fn punct_run_len(text: &str) -> usize {
    text.char_indices()
        .take_while(|(_, c)| c.is_ascii_punctuation())
        .last()
        .map(|(ix, c)| ix + c.len_utf8())
        .unwrap_or(0)
}

/// Length in bytes of the identifier / number starting at the front of `text`.
fn token_len(text: &str) -> usize {
    text.char_indices()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$' || *c == '.')
        .last()
        .map(|(ix, c)| ix + c.len_utf8())
        .unwrap_or(0)
}

/// A diagnostic for something that is not tied to one statement (a connection failure, a refusal).
pub fn diagnostic_for_script(message: impl Into<String>, script: &str) -> Option<Diagnostic> {
    let range = 0..script.trim_end().len();
    (!range.is_empty()).then(|| Diagnostic {
        range,
        message: message.into(),
        code: None,
        statement: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(message: &str, position: Option<usize>) -> QueryError {
        QueryError {
            message: message.into(),
            code: Some("1".into()),
            position,
        }
    }

    #[test]
    fn a_single_statement_failure_underlines_inside_it() {
        let script = "SELECT 1;";
        let d = diagnostic_for(script, DbKind::Sqlite, 0, &error("syntax error", Some(7)))
            .expect("a diagnostic");
        assert_eq!(d.statement, Some(0));
        assert_eq!(&script[d.range.clone()], "1");
        assert_eq!(d.message, "syntax error");
        assert_eq!(d.code.as_deref(), Some("1"));
    }

    #[test]
    fn a_failure_in_the_third_statement_is_attributed_to_it() {
        // This is the case that used to have no location: two statements ran, the third failed.
        let script = "SELECT 1;\nSELECT 2;\nSELEC 3;";
        // `find("SELEC")` would match the prefix of the first `SELECT`: search for the exact word.
        let third_start = script.rfind("SELEC 3").expect("the third statement");
        let d = diagnostic_for(script, DbKind::Sqlite, 2, &error("near \"SELEC\"", Some(0)))
            .expect("a diagnostic");
        assert_eq!(d.statement, Some(2));
        assert_eq!(
            d.range.start, third_start,
            "the underline is in statement 3"
        );
        assert_eq!(&script[d.range.clone()], "SELEC", "the word that broke");
    }

    #[test]
    fn a_position_is_relative_to_the_statement_not_the_script() {
        let script = "SELECT 1;\nSELECT bad FROM t;";
        let second = script.find("SELECT bad").expect("statement 2");
        // The engine says "position 7", meaning `bad` inside statement 2.
        let d = diagnostic_for(script, DbKind::Sqlite, 1, &error("no such column", Some(7)))
            .expect("a diagnostic");
        assert_eq!(d.statement, Some(1));
        assert_eq!(d.range.start, second + 7);
        assert_eq!(&script[d.range.clone()], "bad");
    }

    #[test]
    fn a_statement_with_a_leading_comment_keeps_its_offset() {
        let script = "-- a note\nSELEC 1;";
        let d = diagnostic_for(script, DbKind::Sqlite, 0, &error("syntax", Some(0)))
            .expect("a diagnostic");
        assert_eq!(&script[d.range.clone()], "--");
        assert_eq!(d.range.start, 0, "the comment belongs to the statement");
    }

    #[test]
    fn a_failure_without_a_position_underlines_the_whole_statement() {
        let script = "SELECT 1;\nDELETE FROM t WHERE;";
        let second = script.find("DELETE").expect("statement 2");
        let d = diagnostic_for(script, DbKind::Sqlite, 1, &error("incomplete", None))
            .expect("a diagnostic");
        assert_eq!(d.statement, Some(1));
        assert_eq!(d.range.start, second);
        // `text.end` stops before the terminating `;`, which is what gets underlined.
        assert_eq!(d.range.end, script.trim_end().len() - 1);
    }

    #[test]
    fn multibyte_text_is_never_split() {
        let script = "SELECT '中文' FROM t WHERE x = ;";
        for position in 0..script.len() {
            if let Some(d) = diagnostic_for(script, DbKind::Sqlite, 0, &error("e", Some(position)))
            {
                assert!(
                    script.is_char_boundary(d.range.start) && script.is_char_boundary(d.range.end),
                    "range {:?} splits a character",
                    d.range
                );
            }
        }
    }

    #[test]
    fn an_empty_script_has_no_diagnostic() {
        assert!(diagnostic_for("", DbKind::Sqlite, 0, &error("e", Some(0))).is_none());
        assert!(diagnostic_for("   ", DbKind::Sqlite, 0, &error("e", None)).is_none());
    }

    #[test]
    fn every_statement_started_blames_the_last_one() {
        // A failure that arrived after all statements began belongs to the last of them.
        let script = "SELECT 1; SELECT 2;";
        let d =
            diagnostic_for(script, DbKind::Sqlite, 2, &error("e", Some(0))).expect("a diagnostic");
        assert_eq!(d.statement, Some(1));
    }

    #[test]
    fn a_script_wide_problem_underlines_the_whole_script() {
        let d =
            diagnostic_for_script("connection lost", "SELECT 1;\nSELECT 2;").expect("a diagnostic");
        assert_eq!(d.statement, None);
        assert_eq!(d.range, 0..19, "everything but the trailing newline");
    }
}
