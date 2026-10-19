//! Operations across versions of a graph: the structural [`diff`] of two
//! versions in the same id space (DESIGN.md §7), [`compact`], which
//! renumbers a version into a new id space without tombstones (DESIGN.md
//! §5.2), and [`align`], which renumbers a version of another graph into a
//! version's id space by external id (DESIGN.md §8).
//!
//! `graph` owns the structure of one version; this module only reads
//! [`GraphVersion`]s and produces values (or, for compaction, a new version
//! built through [`GraphCsr::build`]).

mod alignment;
mod compaction;
mod dense_ids;
mod diff;
mod error;
#[cfg(test)]
mod history;

pub use alignment::{align, Alignment};
pub use compaction::{compact, Compaction, Renumbering};
pub use dense_ids::shared_dense_ids;
pub use diff::{
    diff, relation_deltas, DegreeCount, DegreeShift, EdgeDelta, NodeDelta, RelationDelta,
    VersionDiff,
};
pub use error::VersionError;

use crate::graph::{ExternalId, GraphCsr, IdMap, RelationDictionary};

/// A borrowed view of one graph version: its id map, relation dictionary and
/// CSRs, checked to agree with each other.
#[derive(Debug, Clone, Copy)]
pub struct GraphVersion<'v, E> {
    id_map: &'v IdMap<E>,
    relations: &'v RelationDictionary,
    graph: &'v GraphCsr,
}

impl<'v, E: ExternalId> GraphVersion<'v, E> {
    /// A view of one version, after checking that the CSRs cover the id
    /// map's id space and hold one relation per dictionary entry.
    pub fn new(
        id_map: &'v IdMap<E>,
        relations: &'v RelationDictionary,
        graph: &'v GraphCsr,
    ) -> Result<Self, VersionError> {
        let csr = graph.all_relations().forward().id_space();
        if csr != id_map.id_space() {
            return Err(VersionError::IdSpaceMismatch { id_map: id_map.id_space(), csr });
        }
        if relations.len() != graph.relations().len() {
            return Err(VersionError::RelationCountMismatch {
                dictionary: relations.len(),
                csr: graph.relations().len(),
            });
        }
        Ok(Self { id_map, relations, graph })
    }

    /// The id map of the version.
    pub fn id_map(&self) -> &'v IdMap<E> {
        self.id_map
    }

    /// The relation dictionary of the version.
    pub fn relations(&self) -> &'v RelationDictionary {
        self.relations
    }

    /// The CSRs of the version.
    pub fn graph(&self) -> &'v GraphCsr {
        self.graph
    }
}

#[cfg(test)]
mod tests {
    use super::history::Build;
    use super::*;

    #[test]
    fn a_version_view_rejects_csrs_from_another_id_space() {
        let v1 = Build::first(&[1, 2], &[(0, 1, 2, 1)], false, false);
        let v2 = v1.next(&[1, 2, 3], &[(0, 1, 2, 1)], false, false);
        assert_eq!(
            GraphVersion::new(&v2.id_map, &v2.relations, &v1.graph).unwrap_err(),
            VersionError::IdSpaceMismatch { id_map: 3, csr: 2 }
        );
    }

    #[test]
    fn a_version_view_rejects_a_dictionary_of_another_size() {
        let v1 = Build::first(&[1, 2], &[(0, 1, 2, 1)], false, false);
        let v2 = v1.next(&[1, 2], &[(1, 1, 2, 1)], false, false);
        assert_eq!(
            GraphVersion::new(&v1.id_map, &v2.relations, &v1.graph).unwrap_err(),
            VersionError::RelationCountMismatch { dictionary: 2, csr: 1 }
        );
    }
}
