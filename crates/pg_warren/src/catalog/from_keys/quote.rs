//! How the generated SQL writes names and strings: identifiers always
//! double-quoted, string constants as `quote_literal` writes them, and the
//! readable names that text ids and relation names start with.

/// `name` as a quoted SQL identifier: `"name"`, with `"` doubled.
pub(super) fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `text` as a SQL string constant, as `quote_literal` writes it: `'` is
/// doubled, and a constant holding `\` is an escape string (`E'...'`)
/// with `\` doubled, so it reads the same whatever
/// `standard_conforming_strings` is.
pub(super) fn literal(text: &str) -> String {
    let quoted = text.replace('\'', "''");
    if text.contains('\\') {
        format!("E'{}'", quoted.replace('\\', "\\\\"))
    } else {
        format!("'{quoted}'")
    }
}

/// `name` as text ids and relation names show it: as it is when it is
/// lowercase letters, digits and underscores, not starting with a digit,
/// otherwise double-quoted with `"` doubled. So a readable name has no
/// `.`, `:` or `"` outside quotes, and `schema.table:` prefixes of two
/// tables are never the same, nor one a prefix of the other.
pub(super) fn readable(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_owned()
    } else {
        ident(name)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn identifiers_are_always_quoted() {
        assert_eq!(ident("orders"), "\"orders\"");
        assert_eq!(ident("Odd \"name\""), "\"Odd \"\"name\"\"\"");
    }

    #[test]
    fn literals_double_quotes_and_escape_backslashes() {
        assert_eq!(literal("fk"), "'fk'");
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal("a\\b'c"), "E'a\\\\b''c'");
    }

    #[test]
    fn readable_names_quote_anything_but_plain_lowercase() {
        assert_eq!(readable("order_items2"), "order_items2");
        assert_eq!(readable("_x"), "_x");
        for odd in ["Orders", "2nd", "a.b", "a:b", "a\"b", "", "a b", "é"] {
            assert!(readable(odd).starts_with('"'), "{odd}");
        }
        assert_eq!(readable("a\"b"), "\"a\"\"b\"");
    }
}
