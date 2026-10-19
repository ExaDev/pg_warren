//! Where `gather`'s messages come from (DESIGN.md §14 item 13): a vector
//! bound to the version, a relation, or a query, told apart by the
//! argument's form. A query is never taken for a vector, and a name that
//! is both a vector and a relation the caller can see is an error rather
//! than silently either (a graph writer could otherwise redirect another
//! user's gather by binding a vector of that name).

/// A `messages` argument, by its form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageSource<'a> {
    /// The name of a vector bound to the version: one message, its value,
    /// from every live node.
    Vector(&'a str),
    /// A relation name (an identifier, optionally schema-qualified):
    /// its rows, as of `TABLE <name>`.
    Relation(&'a str),
    /// A query.
    Query(&'a str),
    /// The name of both a bound vector and a relation the caller sees.
    Ambiguous(&'a str),
}

impl<'a> MessageSource<'a> {
    /// What `text` names. Text that is not an identifier or
    /// `schema.identifier` (each part unquoted or double-quoted) is a
    /// query, whatever vectors are bound. A name is a vector if
    /// `is_vector(text)`, but ambiguous if `is_relation(text)` too, else a
    /// relation.
    pub(super) fn parse(
        text: &'a str,
        is_vector: impl FnOnce(&str) -> bool,
        is_relation: impl FnOnce(&str) -> bool,
    ) -> Self {
        if !qualified_name(text) {
            return Self::Query(text);
        }
        match (is_vector(text), is_relation(text)) {
            (true, true) => Self::Ambiguous(text),
            (true, false) => Self::Vector(text),
            (false, _) => Self::Relation(text),
        }
    }
}

/// Whether `text` is one or two identifiers joined by a dot.
fn qualified_name(text: &str) -> bool {
    let mut rest = text;
    for part in 0..2 {
        let Some(after) = identifier(rest) else { return false };
        match after.strip_prefix('.') {
            Some(next) if part == 0 => rest = next,
            Some(_) => return false,
            None => return after.is_empty(),
        }
    }
    false
}

/// What follows the identifier `text` starts with, if it starts with one:
/// `"…"` with `""` for a quote, or a letter, `_` or non-ASCII character
/// followed by those, digits and `$`.
fn identifier(text: &str) -> Option<&str> {
    if let Some(quoted) = text.strip_prefix('"') {
        let mut chars = quoted.char_indices().peekable();
        let mut empty = true;
        while let Some((index, c)) = chars.next() {
            if c != '"' {
                empty = false;
                continue;
            }
            if chars.next_if(|&(_, c)| c == '"').is_some() {
                empty = false;
                continue;
            }
            return (!empty).then(|| &quoted[index + 1..]);
        }
        return None;
    }
    let start = |c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii();
    let next = |c: char| start(c) || c.is_ascii_digit() || c == '$';
    let first = text.chars().next().filter(|&c| start(c))?;
    let end = text[first.len_utf8()..]
        .find(|c: char| !next(c))
        .map_or(text.len(), |i| i + first.len_utf8());
    Some(&text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` parsed where `score` and `state` are bound vectors and
    /// `state` and `public.state` are also relations.
    fn parse(text: &str) -> MessageSource<'_> {
        let relations = ["state", "public.state", "t"];
        MessageSource::parse(
            text,
            |name| ["score", "state", "SELECT 1"].contains(&name),
            |name| relations.contains(&name),
        )
    }

    #[test]
    fn a_bound_vector_name_is_a_vector() {
        assert_eq!(parse("score"), MessageSource::Vector("score"));
    }

    #[test]
    fn a_name_of_both_a_vector_and_a_relation_is_ambiguous() {
        assert_eq!(parse("state"), MessageSource::Ambiguous("state"));
    }

    #[test]
    fn query_text_is_never_a_vector() {
        assert_eq!(parse("SELECT 1"), MessageSource::Query("SELECT 1"));
    }

    #[test]
    fn an_identifier_is_a_relation() {
        for name in ["t", "public.state", "\"My \"\"State\"\"\"", "s.\"t\"", "_t1$", "Ünïcode"] {
            assert_eq!(parse(name), MessageSource::Relation(name), "{name}");
        }
    }

    #[test]
    fn anything_else_is_a_query() {
        for query in [
            "SELECT id, v FROM t",
            "TABLE t",
            "a.b.c",
            "1abc",
            "\"unterminated",
            "a..b",
            "",
            "t;",
            "state ",
        ] {
            assert_eq!(parse(query), MessageSource::Query(query), "{query}");
        }
    }
}
