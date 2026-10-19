//! Which edges an operation follows: its direction and the relations it
//! selects by name (DESIGN.md §6.3, §9.3). Used by every feature that walks
//! or multiplies by the adjacency (`traversal_sql`, `compute_sql`).

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::{GraphCsr, RelationDictionary, RelationIndex, RelationSelection};
use warren_core::traversal::{Adjacency, Direction, TraversalError};

use crate::access::SqlError;
use crate::extension::graph_sqlstate;

/// Why a selection of edges could not be made.
#[derive(Debug, Error)]
pub(crate) enum SelectionError {
    /// `direction` was NULL.
    #[error("direction must not be NULL")]
    NullDirection,
    /// `direction` is not `out`, `in` or `both`.
    #[error("direction must be 'out', 'in' or 'both', not '{0}'")]
    Direction(String),
    /// The adjacency could not be read in that direction.
    #[error("{0}")]
    Adjacency(#[from] TraversalError),
}

impl SqlError for SelectionError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NullDirection => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::Direction(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::Adjacency(TraversalError::ReverseCsrMissing) => {
                PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
            }
            Self::Adjacency(TraversalError::Graph(error)) => graph_sqlstate(error),
            Self::Adjacency(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}

/// `direction`: `out`, `in` or `both` (case-insensitive).
pub(crate) fn direction(text: Option<&str>) -> Result<Direction, SelectionError> {
    let text = text.ok_or(SelectionError::NullDirection)?;
    match text.to_ascii_lowercase().as_str() {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        _ => Err(SelectionError::Direction(text.to_owned())),
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

/// The adjacency of `graph` in `direction` over the relations `rels` names
/// in `dictionary` (all if `None`).
pub(crate) fn adjacency<'g>(
    graph: &'g GraphCsr,
    dictionary: &RelationDictionary,
    direction: Direction,
    rels: Option<&[Option<String>]>,
) -> Result<Adjacency<'g>, SelectionError> {
    let selected = relations(dictionary, rels);
    let selection = selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only);
    Ok(Adjacency::new(graph, direction, selection)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_are_out_in_or_both() {
        assert_eq!(direction(Some("out")).ok(), Some(Direction::Out));
        assert_eq!(direction(Some("IN")).ok(), Some(Direction::In));
        assert_eq!(direction(Some("both")).ok(), Some(Direction::Both));
        assert!(matches!(direction(Some("up")), Err(SelectionError::Direction(d)) if d == "up"));
        assert!(matches!(direction(None), Err(SelectionError::NullDirection)));
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
}
