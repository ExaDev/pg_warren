//! Alignment of a version of one graph to a version of another by external
//! id, for comparing across graphs (DESIGN.md §8: the shadow-schema
//! pattern). The other version is renumbered into a shared id space: nodes
//! of both keep this version's dense ids, and nodes only in the other are
//! appended in external-id order. Relations are matched by name: those
//! only in the other are appended to this version's dictionary.

use super::{GraphVersion, VersionError};
use crate::graph::{memory, DenseId, EdgeList, ExternalId, GraphCsr, RelationDictionary};

/// Another graph's version in this version's id space.
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    /// Entry `i` is the shared dense id of the other version's dense id `i`.
    pub dense_ids: Vec<DenseId>,
    /// The shared id space: this version's, plus the other's own nodes.
    pub id_space: u64,
    /// This version's relation dictionary, plus the other's own relations.
    pub relations: RelationDictionary,
    /// The other version's CSRs in the shared dense ids and relation
    /// indexes.
    pub graph: GraphCsr,
}

/// `other` aligned to `this` by external id.
pub fn align<E: ExternalId>(
    this: &GraphVersion<'_, E>,
    other: &GraphVersion<'_, E>,
) -> Result<Alignment, VersionError> {
    let mut dense_ids = memory::filled(other.id_map().id_space() as usize, 0)?;
    let mut id_space = this.id_map().id_space();
    for (external, dense) in other.id_map().pairs() {
        dense_ids[*dense as usize] = this.id_map().dense_id(external).unwrap_or_else(|| {
            id_space += 1;
            id_space - 1
        });
    }
    let mut relations = this.relations().clone();
    let all = other.graph().all_relations();
    let mut edges = EdgeList::new(all.forward().is_weighted());
    for (name, csr) in other.relations().names().iter().zip(other.graph().relations()) {
        let relation = relations.intern(name)?;
        let csr = csr.forward();
        for source in 0..csr.id_space() {
            let weights = csr.neighbour_weights(source);
            for (i, target) in csr.neighbours(source).enumerate() {
                let weight = weights.map_or(1.0, |weights| weights[i]);
                let (source, target) = (dense_ids[source as usize], dense_ids[target as usize]);
                edges.push(relation, source, target, weight)?;
            }
        }
    }
    let graph = GraphCsr::build(edges, relations.len(), id_space, all.reverse().is_some())?;
    Ok(Alignment { dense_ids, id_space, relations, graph })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::history::Build;

    fn edges(graph: &GraphCsr, relation: usize) -> Vec<(DenseId, DenseId)> {
        let csr = graph.relations()[relation].forward();
        (0..csr.id_space()).flat_map(|s| csr.neighbours(s).map(move |t| (s, t))).collect()
    }

    #[test]
    fn shared_nodes_take_this_versions_dense_ids_and_the_rest_are_appended() {
        // This: 10 → 0, 20 → 1, 30 → 2, relation r0. Other: 5, 20, 30, 40
        // (dense 0..4), relations r1 then r0.
        let this = Build::first(&[10, 20, 30], &[(0, 10, 20, 1)], false, true);
        let other = Build::first(
            &[5, 20, 30, 40],
            &[(1, 5, 40, 1), (0, 20, 30, 1), (0, 40, 20, 1)],
            false,
            true,
        );
        let aligned = align(&this.version(), &other.version()).unwrap();
        assert_eq!(aligned.dense_ids, [3, 1, 2, 4]);
        assert_eq!(aligned.id_space, 5);
        assert_eq!(aligned.relations.names(), ["r0", "r1"]);
        assert_eq!(edges(&aligned.graph, 0), [(1, 2), (4, 1)]);
        assert_eq!(edges(&aligned.graph, 1), [(3, 4)]);
        assert_eq!(aligned.graph.all_relations().forward().id_space(), 5);
        assert!(aligned.graph.all_relations().reverse().is_some());
    }

    #[test]
    fn a_version_aligned_to_itself_is_unchanged() {
        let v = Build::first(&[1, 2, 3], &[(0, 1, 2, 3), (1, 3, 1, 2)], true, false);
        let aligned = align(&v.version(), &v.version()).unwrap();
        assert_eq!(aligned.dense_ids, [0, 1, 2]);
        assert_eq!(aligned.id_space, 3);
        assert_eq!(&aligned.relations, &v.relations);
        assert_eq!(aligned.graph, v.graph);
    }
}
