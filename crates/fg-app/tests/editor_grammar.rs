#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Gate test for the SQL editor: the editor pane is only worth building if the Tree-sitter SQL
//! grammar is actually present at run time with the feature set `fg-app` selects
//! (`gpui-fast` + `tree-sitter` + `tree-sitter-sql`). A build that merely links would not catch a
//! grammar that failed to register, so this checks the registry itself.

use gpui_kit::component::highlighter::LanguageRegistry;

#[test]
fn the_sql_grammar_is_registered() {
    let registry = LanguageRegistry::singleton();
    let names = registry.languages();
    assert!(
        names.iter().any(|name| name == "sql"),
        "the SQL grammar is missing; languages available: {} of them",
        names.len()
    );
    let config = registry.language("sql");
    assert!(config.is_some(), "`language(\"sql\")` returned None");
}

#[test]
fn an_unknown_grammar_is_not_reported_as_available() {
    // The editor falls back to plain text when a language has no grammar; a typo in the language
    // name must show up as a missing grammar rather than silently highlighting something else.
    assert!(
        LanguageRegistry::singleton()
            .language("definitely-not-a-language")
            .is_none(),
        "an invented language must not resolve"
    );
}
