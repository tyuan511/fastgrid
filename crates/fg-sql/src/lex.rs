//! A small, tolerant SQL lexer shared by the splitter and the classifier.
//!
//! It only needs to know where strings, identifiers and comments begin and end for the three
//! supported engines; it never rejects input (an unterminated string simply runs to the end and
//! is flagged), because it runs on text the user is still typing.

use fg_core::DbKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Sqlite,
    Postgres,
    MySql,
}

impl From<DbKind> for Flavor {
    fn from(kind: DbKind) -> Self {
        match kind {
            DbKind::Sqlite => Flavor::Sqlite,
            DbKind::MySql => Flavor::MySql,
            // Unknown engines get PostgreSQL's (strictest, standard-like) lexing.
            _ => Flavor::Postgres,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Whitespace,
    LineComment,
    BlockComment,
    /// Opening/closing of a MySQL `/*! ... */` comment, whose content *is* executed.
    ExecCommentMark,
    Word,
    QuotedIdent,
    String,
    /// PostgreSQL `$tag$ ... $tag$`.
    DollarString,
    Number,
    Punct,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    /// A string / identifier / comment that never closed.
    pub unterminated: bool,
}

impl Token {
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.start..self.end]
    }

    /// Whitespace and comments: ignored when looking for what a statement *is*.
    pub fn is_trivia(&self) -> bool {
        matches!(
            self.kind,
            Kind::Whitespace | Kind::LineComment | Kind::BlockComment | Kind::ExecCommentMark
        )
    }

    pub fn is_punct(&self, sql: &str, c: char) -> bool {
        self.kind == Kind::Punct && self.text(sql).starts_with(c)
    }
}

fn is_word_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_word_char(c: char, flavor: Flavor) -> bool {
    c == '_' || c.is_alphanumeric() || (c == '$' && flavor != Flavor::Sqlite)
}

pub(crate) fn lex(sql: &str, flavor: Flavor) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut exec_comment_depth = 0usize;

    while i < sql.len() {
        let start = i;
        let c = match sql[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let next = |off: usize| bytes.get(i + off).copied();

        let (kind, end, unterminated) = if c.is_whitespace() {
            let end = sql[i..]
                .find(|ch: char| !ch.is_whitespace())
                .map_or(sql.len(), |p| i + p);
            (Kind::Whitespace, end, false)
        } else if (c == '-' && next(1) == Some(b'-') && line_comment_allowed(sql, i, flavor))
            || (c == '#' && flavor == Flavor::MySql)
        {
            (Kind::LineComment, line_end(sql, i), false)
        } else if c == '/' && next(1) == Some(b'*') {
            if flavor == Flavor::MySql && next(2) == Some(b'!') {
                // Executable comment: keep lexing its content as code.
                exec_comment_depth += 1;
                let mut end = i + 3;
                while end < sql.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                (Kind::ExecCommentMark, end, false)
            } else {
                block_comment(sql, i, flavor == Flavor::Postgres)
            }
        } else if c == '*' && next(1) == Some(b'/') && exec_comment_depth > 0 {
            exec_comment_depth -= 1;
            (Kind::ExecCommentMark, i + 2, false)
        } else if c == '\'' {
            let backslash =
                flavor == Flavor::MySql || postgres_escape_string(sql, &tokens, i, flavor);
            let (end, unterminated) = quoted(sql, i, '\'', backslash);
            (Kind::String, end, unterminated)
        } else if c == '"' {
            if flavor == Flavor::MySql {
                let (end, unterminated) = quoted(sql, i, '"', true);
                (Kind::String, end, unterminated)
            } else {
                let (end, unterminated) = quoted(sql, i, '"', false);
                (Kind::QuotedIdent, end, unterminated)
            }
        } else if c == '`' {
            let (end, unterminated) = quoted(sql, i, '`', false);
            (Kind::QuotedIdent, end, unterminated)
        } else if c == '[' && flavor == Flavor::Sqlite {
            match sql[i..].find(']') {
                Some(p) => (Kind::QuotedIdent, i + p + 1, false),
                None => (Kind::QuotedIdent, sql.len(), true),
            }
        } else if c == '$' && flavor == Flavor::Postgres {
            match dollar_quote(sql, i) {
                Some((end, unterminated)) => (Kind::DollarString, end, unterminated),
                None => {
                    // `$1` style parameter: lex as a word so it is not mistaken for punctuation.
                    let mut end = i + 1;
                    while end < sql.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                    if end > i + 1 {
                        (Kind::Word, end, false)
                    } else {
                        (Kind::Punct, i + 1, false)
                    }
                }
            }
        } else if c.is_ascii_digit() {
            let mut end = i;
            while end < sql.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'.' || bytes[end] == b'_')
            {
                end += 1;
            }
            (Kind::Number, end, false)
        } else if is_word_start(c) {
            let end = sql[i..]
                .find(|ch: char| !is_word_char(ch, flavor))
                .map_or(sql.len(), |p| i + p);
            (Kind::Word, end, false)
        } else {
            (Kind::Punct, i + c.len_utf8(), false)
        };

        tokens.push(Token {
            kind,
            start,
            end,
            unterminated,
        });
        i = end;
    }
    tokens
}

/// MySQL only treats `--` as a comment when followed by whitespace (or the end).
fn line_comment_allowed(sql: &str, at: usize, flavor: Flavor) -> bool {
    if flavor != Flavor::MySql {
        return true;
    }
    sql[at + 2..]
        .chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || c.is_control())
}

fn line_end(sql: &str, from: usize) -> usize {
    sql[from..].find('\n').map_or(sql.len(), |p| from + p)
}

fn block_comment(sql: &str, from: usize, nested: bool) -> (Kind, usize, bool) {
    let bytes = sql.as_bytes();
    let mut depth = 1;
    let mut i = from + 2;
    while i < bytes.len() {
        if nested && bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return (Kind::BlockComment, i, false);
            }
        } else {
            i += 1;
        }
    }
    (Kind::BlockComment, sql.len(), true)
}

/// Scan a quoted run starting at `from` (the opening quote). A doubled quote is an escaped quote;
/// with `backslash`, `\x` escapes the next character. Returns (end, unterminated).
fn quoted(sql: &str, from: usize, quote: char, backslash: bool) -> (usize, bool) {
    let mut chars = sql[from + quote.len_utf8()..].char_indices().peekable();
    let base = from + quote.len_utf8();
    while let Some((off, c)) = chars.next() {
        if backslash && c == '\\' {
            chars.next();
        } else if c == quote {
            if matches!(chars.peek(), Some((_, n)) if *n == quote) {
                chars.next();
            } else {
                return (base + off + quote.len_utf8(), false);
            }
        }
    }
    (sql.len(), true)
}

/// `E'...'` strings in PostgreSQL take backslash escapes: the quote at `quote_at` directly follows
/// an `E` word.
fn postgres_escape_string(sql: &str, tokens: &[Token], quote_at: usize, flavor: Flavor) -> bool {
    flavor == Flavor::Postgres
        && tokens.last().is_some_and(|t| {
            t.kind == Kind::Word && t.end == quote_at && t.text(sql).eq_ignore_ascii_case("e")
        })
}

/// `$tag$ ... $tag$`. Returns None if `from` is not an opening dollar quote.
fn dollar_quote(sql: &str, from: usize) -> Option<(usize, bool)> {
    let rest = &sql[from + 1..];
    let tag_len = rest
        .find(|c: char| !(c == '_' || c.is_alphanumeric()))
        .unwrap_or(rest.len());
    let tag = &rest[..tag_len];
    if rest[tag_len..].starts_with('$') && !tag.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        let opening = format!("${tag}$");
        let body_start = from + opening.len();
        match sql[body_start..].find(&opening) {
            Some(p) => Some((body_start + p + opening.len(), false)),
            None => Some((sql.len(), true)),
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str, f: Flavor) -> Vec<(Kind, &str)> {
        lex(sql, f)
            .into_iter()
            .filter(|t| t.kind != Kind::Whitespace)
            .map(|t| (t.kind, &sql[t.start..t.end]))
            .collect()
    }

    #[test]
    fn strings_idents_and_comments() {
        let k = kinds(
            "SELECT 'a;b', \"c;d\", `e` -- x;\n/* y; */ FROM t",
            Flavor::Sqlite,
        );
        assert!(k.contains(&(Kind::String, "'a;b'")));
        assert!(k.contains(&(Kind::QuotedIdent, "\"c;d\"")));
        assert!(k.contains(&(Kind::QuotedIdent, "`e`")));
        assert!(k.contains(&(Kind::LineComment, "-- x;")));
        assert!(k.contains(&(Kind::BlockComment, "/* y; */")));
    }

    #[test]
    fn doubled_quotes_do_not_end_a_string() {
        let k = kinds("SELECT 'it''s; fine'", Flavor::Postgres);
        assert_eq!(k.last(), Some(&(Kind::String, "'it''s; fine'")));
    }

    #[test]
    fn backslash_escapes_only_where_the_engine_has_them() {
        // MySQL: \' is an escaped quote, so the string continues.
        let my = kinds(r"SELECT 'a\'b;c'", Flavor::MySql);
        assert_eq!(my.last().map(|t| t.0), Some(Kind::String));
        assert_eq!(my.last().map(|t| t.1), Some(r"'a\'b;c'"));
        // PostgreSQL / SQLite: backslash is an ordinary character, the string ends at \'.
        let pg = kinds(r"SELECT 'a\'b", Flavor::Postgres);
        assert_eq!(pg[1], (Kind::String, r"'a\'"));
        // But E'..' in PostgreSQL does escape.
        let e = kinds(r"SELECT E'a\'b;c'", Flavor::Postgres);
        assert_eq!(e.last(), Some(&(Kind::String, r"'a\'b;c'")));
    }

    #[test]
    fn mysql_double_quotes_are_strings_and_hash_is_a_comment() {
        let k = kinds("SELECT \"x;y\" # note;\n", Flavor::MySql);
        assert!(k.contains(&(Kind::String, "\"x;y\"")));
        assert!(k.contains(&(Kind::LineComment, "# note;")));
    }

    #[test]
    fn mysql_double_dash_needs_a_space() {
        let k = kinds("SELECT 1--1", Flavor::MySql);
        assert!(!k.iter().any(|t| t.0 == Kind::LineComment));
        assert!(
            kinds("SELECT 1 -- c", Flavor::MySql)
                .iter()
                .any(|t| t.0 == Kind::LineComment)
        );
    }

    #[test]
    fn dollar_quoting_and_parameters() {
        let k = kinds("SELECT $$a;b$$, $tag$ x; $$ y $tag$, $1", Flavor::Postgres);
        assert!(k.contains(&(Kind::DollarString, "$$a;b$$")));
        assert!(k.contains(&(Kind::DollarString, "$tag$ x; $$ y $tag$")));
        assert!(k.contains(&(Kind::Word, "$1")));
    }

    #[test]
    fn postgres_block_comments_nest_others_do_not() {
        let pg = kinds("/* a /* b */ c */ SELECT 1", Flavor::Postgres);
        assert_eq!(pg[0].0, Kind::BlockComment);
        assert_eq!(pg[0].1, "/* a /* b */ c */");
        let lite = kinds("/* a /* b */ c */", Flavor::Sqlite);
        assert_eq!(lite[0].1, "/* a /* b */");
    }

    #[test]
    fn mysql_executable_comments_are_code() {
        let k = kinds("/*!40101 DROP TABLE t */", Flavor::MySql);
        assert!(
            k.iter().any(|t| t.0 == Kind::Word && t.1 == "DROP"),
            "{k:?}"
        );
    }

    #[test]
    fn sqlite_bracket_identifiers() {
        let k = kinds("SELECT [a;b] FROM t", Flavor::Sqlite);
        assert!(k.contains(&(Kind::QuotedIdent, "[a;b]")));
    }

    #[test]
    fn unterminated_input_is_flagged_not_rejected() {
        let t = lex("SELECT 'oops", Flavor::Sqlite);
        assert!(t.last().is_some_and(|t| t.unterminated));
        let c = lex("SELECT /* oops", Flavor::Sqlite);
        assert!(c.last().is_some_and(|t| t.unterminated));
    }

    #[test]
    fn unicode_does_not_panic() {
        let k = kinds("SELECT 'héllo;世界' AS 名字", Flavor::Postgres);
        assert!(k.iter().any(|t| t.0 == Kind::Word && t.1 == "名字"));
    }
}
