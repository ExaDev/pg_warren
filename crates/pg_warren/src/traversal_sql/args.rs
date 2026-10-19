//! Traversal arguments other than node ids: direction, relation names and
//! bounds (DESIGN.md §9.3).

use warren_core::graph::Direction;
use warren_core::graph::{RelationDictionary, RelationIndex};

use super::TraversalSqlError;

/// `direction`: `out`, `in` or `both` (case-insensitive).
pub(crate) fn direction(text: Option<&str>) -> Result<Direction, TraversalSqlError> {
    let text = text.ok_or(TraversalSqlError::NullArgument("direction"))?;
    match text.to_ascii_lowercase().as_str() {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        _ => Err(TraversalSqlError::Direction(text.to_owned())),
    }
}

/// The relations `rels` names (all relations if `None`). Names a version
/// does not have are skipped, as are NULL elements: a relation absent from
/// a version has no edges in it.
pub(crate) fn relations(
    dictionary: &RelationDictionary,
    rels: Option<&[Option<String>]>,
) -> Option<Vec<RelationIndex>> {
    rels.map(|names| names.iter().flatten().filter_map(|name| dictionary.get(name)).collect())
}

/// A depth or count bound: `None` is unbounded; a negative one is an error.
pub(crate) fn bound<T: TryFrom<i64>>(
    name: &'static str,
    value: Option<i64>,
) -> Result<Option<T>, TraversalSqlError> {
    value.map(|value| T::try_from(value).map_err(|_| TraversalSqlError::Negative(name))).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_are_out_in_or_both() {
        assert_eq!(direction(Some("out")).ok(), Some(Direction::Out));
        assert_eq!(direction(Some("IN")).ok(), Some(Direction::In));
        assert_eq!(direction(Some("both")).ok(), Some(Direction::Both));
        assert!(matches!(direction(Some("up")), Err(TraversalSqlError::Direction(d)) if d == "up"));
        assert!(matches!(direction(None), Err(TraversalSqlError::NullArgument("direction"))));
    }

    #[test]
    fn unknown_relation_names_and_nulls_are_skipped() {
        let mut dictionary = RelationDictionary::new();
        let cites = dictionary.intern("cites").unwrap();
        let funds = dictionary.intern("funds").unwrap();
        assert_eq!(relations(&dictionary, None), None);
        let names = [Some("funds".to_owned()), None, Some("nope".to_owned()), Some("cites".into())];
        assert_eq!(relations(&dictionary, Some(&names)), Some(vec![funds, cites]));
        assert_eq!(relations(&dictionary, Some(&[])), Some(vec![]));
    }

    #[test]
    fn bounds_are_unbounded_when_null_and_never_negative() {
        assert_eq!(bound::<u32>("max_depth", None).ok(), Some(None));
        assert_eq!(bound::<u32>("max_depth", Some(3)).ok(), Some(Some(3)));
        assert!(matches!(
            bound::<u64>("max_results", Some(-1)),
            Err(TraversalSqlError::Negative("max_results"))
        ));
    }
}
