//! Line comments: toggle `--` on the lines a selection touches.
//!
//! The reason this lives in `fg-sql` rather than in the editor: deciding whether a line is
//! *already* commented needs the lexer. A line whose first non-space text is `--` inside a string
//! (`SELECT '--x'`) is not commented, and toggling it must not strip anything. Doing this with
//! `trim_start().starts_with("--")` would corrupt such a line, so the token stream decides.
//!
//! Scope: line comments only. Block comments (`/* */`) are not toggled - uncommenting a block that
//! spans lines has no single obvious answer, and half-doing it is worse than leaving the key to the
//! common case.

use fg_core::DbKind;

use crate::lex::{Flavor, Kind, lex};

/// What a toggle did, so the caller can move the caret/selection sensibly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentEdit {
    /// The new text for the range that was edited.
    pub text: String,
    /// True when the edit added comment markers, false when it removed them.
    pub commented: bool,
    /// How many characters were inserted (positive) or removed (negative) on each affected line,
    /// so a caller can shift its selection. `0` for lines that already had the right shape.
    pub delta_per_line: i32,
}

/// Toggle `--` on every line touched by `range`.
///
/// * All affected lines get commented unless **every** non-blank line is already a line comment, in
///   which case they are all uncommented. Mixed selections therefore comment everything, which is
///   what users expect from every other editor.
/// * Blank lines are skipped: they get no marker and lose none.
/// * The marker is inserted after the line's indentation, not at column 0, so indented SQL stays
///   indented.
/// * `range` may be empty (a bare caret): the line it sits on is the target.
pub fn toggle_line_comments(
    text: &str,
    range: std::ops::Range<usize>,
    kind: DbKind,
) -> CommentEdit {
    let flavor = Flavor::from(kind);
    let lines = touched_lines(text, &range);
    let tokens = lex(text, flavor);

    // Which lines are already commented, judged by the lexer rather than by string prefixes.
    let commented: Vec<bool> = lines
        .iter()
        .map(|&(start, end)| is_commented_line(&tokens, start, end))
        .collect();
    let non_blank: Vec<bool> = lines
        .iter()
        .map(|&(start, end)| !text[start..end].trim().is_empty())
        .collect();
    let any_content = non_blank.iter().any(|b| *b);
    if !any_content {
        // Nothing to toggle: an all-blank selection is left exactly as it was.
        return CommentEdit {
            text: text[range.start.min(text.len())..range.end.min(text.len())].to_owned(),
            commented: false,
            delta_per_line: 0,
        };
    }
    let uncomment = commented
        .iter()
        .zip(&non_blank)
        .all(|(commented, content)| *commented || !*content);

    let mut out = String::with_capacity(text.len() + lines.len() * 3);
    // Copy everything before the first touched line untouched.
    let first_start = lines.first().map(|(s, _)| *s).unwrap_or(0);
    out.push_str(&text[..first_start]);
    let mut delta_per_line = 0;
    for (index, &(start, end)) in lines.iter().enumerate() {
        let line = &text[start..end];
        if !non_blank[index] {
            out.push_str(line);
        } else if uncomment {
            let (new_line, removed) = strip_marker(line, &tokens, start);
            out.push_str(&new_line);
            delta_per_line = -(removed as i32);
        } else {
            let (new_line, inserted) = add_marker(line);
            out.push_str(&new_line);
            delta_per_line = inserted as i32;
        }
        // Put back the newline that separates this line from the next (the ranges exclude it).
        if end < text.len() && text[end..].starts_with('\n') {
            out.push('\n');
        }
    }
    let last_end = lines.last().map(|(_, e)| *e).unwrap_or(text.len());
    let tail_start = if last_end < text.len() && text[last_end..].starts_with('\n') {
        last_end + 1
    } else {
        last_end
    };
    out.push_str(&text[tail_start..]);

    CommentEdit {
        text: out,
        commented: !uncomment,
        delta_per_line,
    }
}

/// Byte ranges `[start, end)` of the lines the selection touches, `end` excluding the newline.
fn touched_lines(text: &str, range: &std::ops::Range<usize>) -> Vec<(usize, usize)> {
    let len = text.len();
    let start = range.start.min(len);
    let end = range.end.min(len);
    // A selection ending exactly at a line start does not include that line (shift-selecting to
    // the beginning of the next line is how people select whole lines).
    let end = if end > start && text[..end].ends_with('\n') {
        end - 1
    } else {
        end
    };
    let mut lines = Vec::new();
    let mut line_start = 0;
    for (ix, ch) in text.char_indices() {
        if ch == '\n' {
            if ix >= start && line_start <= end {
                lines.push((line_start, ix));
            }
            line_start = ix + 1;
        }
    }
    if line_start <= len && (line_start <= end || lines.is_empty()) {
        lines.push((line_start, len));
    }
    // Keep only the lines that actually intersect the selection.
    lines.retain(|(s, e)| *s <= end && *e >= start);
    if lines.is_empty() {
        lines.push((line_start.min(len), len));
    }
    lines
}

/// Is the first non-whitespace token of this line a line comment?
fn is_commented_line(tokens: &[crate::lex::Token], start: usize, end: usize) -> bool {
    tokens
        .iter()
        // Skip the indentation: the first *visible* token decides. A line whose dash pair sits
        // inside a string (`SELECT '--x'`) has `SELECT` there, so it is not mistaken for a comment.
        .filter(|t| t.kind != Kind::Whitespace)
        .find(|t| t.start >= start && t.start < end)
        .map(|t| t.kind == Kind::LineComment)
        .unwrap_or(false)
}

/// Remove the leading `--` (and one following space, if any) from a commented line.
fn strip_marker(line: &str, tokens: &[crate::lex::Token], line_start: usize) -> (String, usize) {
    let indent = line.len() - line.trim_start().len();
    let comment = tokens.iter().find(|t| {
        t.start >= line_start && t.kind == Kind::LineComment && t.start < line_start + line.len()
    });
    let Some(comment) = comment else {
        return (line.to_owned(), 0);
    };
    // Position of the marker inside this line.
    let marker_at = comment.start - line_start;
    if marker_at < indent {
        return (line.to_owned(), 0);
    }
    let mut rest = &line[marker_at..];
    let mut removed = 0;
    for _ in 0..2 {
        if let Some(stripped) = rest.strip_prefix('-') {
            rest = stripped;
            removed += 1;
        }
    }
    // `--  text` -> `text`: drop the single space the comment convention adds, not the user's
    // indentation inside the comment.
    if let Some(stripped) = rest.strip_prefix(' ') {
        rest = stripped;
        removed += 1;
    }
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..marker_at]);
    out.push_str(rest);
    (out, removed)
}

/// Insert `-- ` after the line's indentation.
fn add_marker(line: &str) -> (String, usize) {
    let indent = line.len() - line.trim_start().len();
    let mut out = String::with_capacity(line.len() + 3);
    out.push_str(&line[..indent]);
    out.push_str("-- ");
    out.push_str(&line[indent..]);
    (out, 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggle(text: &str, range: std::ops::Range<usize>) -> CommentEdit {
        toggle_line_comments(text, range, DbKind::Sqlite)
    }

    #[test]
    fn one_line_gets_commented_then_uncommented() {
        let text = "SELECT 1;";
        let on = toggle(text, 0..0);
        assert_eq!(on.text, "-- SELECT 1;");
        assert!(on.commented);
        assert_eq!(on.delta_per_line, 3);

        let off = toggle(&on.text, 0..0);
        assert_eq!(off.text, "SELECT 1;");
        assert!(!off.commented);
        assert_eq!(off.delta_per_line, -3, "the marker and its space go");
    }

    #[test]
    fn indentation_is_preserved() {
        let text = "    SELECT 1;";
        let on = toggle(text, 6..6);
        assert_eq!(on.text, "    -- SELECT 1;");
        assert_eq!(toggle(&on.text, 6..6).text, text);
    }

    #[test]
    fn every_line_of_a_selection_is_toggled_together() {
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let on = toggle(text, 0..text.len());
        assert_eq!(on.text, "-- SELECT 1;\n-- SELECT 2;\n-- SELECT 3;");
        assert_eq!(toggle(&on.text, 0..on.text.len()).text, text);
    }

    #[test]
    fn a_partly_commented_selection_gets_commented_rather_than_mixed() {
        let text = "-- SELECT 1;\nSELECT 2;";
        let out = toggle(text, 0..text.len());
        assert_eq!(
            out.text, "-- -- SELECT 1;\n-- SELECT 2;",
            "mixed selections comment everything, the usual editor behaviour"
        );
    }

    #[test]
    fn blank_lines_get_no_marker_and_lose_none() {
        let text = "SELECT 1;\n\nSELECT 2;";
        let on = toggle(text, 0..text.len());
        assert_eq!(on.text, "-- SELECT 1;\n\n-- SELECT 2;");
        assert_eq!(toggle(&on.text, 0..on.text.len()).text, text);
    }

    #[test]
    fn a_marker_inside_a_string_is_not_a_comment() {
        let text = "SELECT '--x';";
        let on = toggle(text, 0..0);
        assert_eq!(
            on.text, "-- SELECT '--x';",
            "the line is not commented just because it contains a dash pair in a string"
        );
        assert!(on.commented);
    }

    #[test]
    fn a_commented_line_that_starts_with_a_string_is_uncommented_correctly() {
        let text = "-- SELECT '--x';";
        let off = toggle(text, 0..0);
        assert_eq!(off.text, "SELECT '--x';");
    }

    #[test]
    fn a_selection_that_ends_at_the_next_line_start_does_not_include_it() {
        let text = "SELECT 1;\nSELECT 2;";
        // Selecting exactly the first line, including its newline.
        let out = toggle(text, 0..10);
        assert_eq!(out.text, "-- SELECT 1;\nSELECT 2;");
    }

    #[test]
    fn an_all_blank_selection_is_left_alone() {
        let text = "SELECT 1;\n   \n";
        let out = toggle(text, 10..14);
        assert_eq!(out.text, "   \n");
        assert!(!out.commented);
        assert_eq!(out.delta_per_line, 0);
    }

    #[test]
    fn a_caret_past_the_end_of_the_text_does_not_panic() {
        let text = "SELECT 1;";
        let out = toggle(text, 999..1000);
        assert_eq!(out.text, "-- SELECT 1;");
    }

    #[test]
    fn multibyte_text_survives_the_round_trip() {
        let text = "SELECT '中文' AS 名字;";
        let on = toggle(text, 0..0);
        assert_eq!(on.text, format!("-- {text}"));
        assert_eq!(toggle(&on.text, 0..0).text, text);
    }

    #[test]
    fn mysql_line_comments_work_too() {
        let text = "SELECT 1;";
        let on = toggle_line_comments(text, 0..0, DbKind::MySql);
        assert_eq!(on.text, "-- SELECT 1;");
        assert_eq!(
            toggle_line_comments(&on.text, 0..0, DbKind::MySql).text,
            text
        );
    }
}
