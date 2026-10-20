//! A version of another graph in a version's id space, by external id, for
//! comparing across graphs (DESIGN.md §8: the shadow-schema pattern).
//! Nodes of both keep this version's dense ids and nodes only in the other
//! are appended in external-id order; relations are matched by name, those
//! only in the other appended to this version's dictionary. The dense-id
//! mapping is O(N); the other version's CSRs are rebuilt in the shared ids
//! only for `edit`, which compares edges.

use super::super::{Resources, SpectralError};
use crate::compute::{ComputeError, MemoryCharge};
use crate::graph::{
    CsrLayout, DenseId, EdgeList, ExternalId, GraphCsr, RelationDictionary, RelationIndex,
};
use crate::interrupts::Interrupts;
use crate::version::GraphVersion;

/// How often the passes poll the interrupts, in rows.
const ROWS_PER_POLL: u64 = 1 << 16;

/// How another graph's version maps into this version's id space.
#[derive(Debug)]
pub struct IdAlignment {
    /// Entry `i` is the shared dense id of the other version's dense id `i`.
    pub dense_ids: Vec<DenseId>,
    /// The shared id space: this version's, plus the other's own nodes.
    pub id_space: u64,
    /// This version's relation dictionary, plus the other's own relations.
    pub relations: RelationDictionary,
    /// Entry `r` is the shared index of the other version's relation `r`.
    relation_indexes: Vec<RelationIndex>,
    _charge: MemoryCharge,
}

/// Another graph's CSRs rebuilt in the shared dense ids and relation
/// indexes of an [`IdAlignment`].
#[derive(Debug)]
pub struct AlignedGraph {
    /// The CSRs.
    pub graph: GraphCsr,
    _charge: MemoryCharge,
}

/// How `other` maps into `this`'s id space by external id.
pub fn align_ids<E: ExternalId, I: Interrupts>(
    resources: &mut Resources<'_, I>,
    this: &GraphVersion<'_, E>,
    other: &GraphVersion<'_, E>,
) -> Result<IdAlignment, SpectralError> {
    let (mut dense_ids, charge) = resources.memory.filled(other.id_map().id_space() as usize, 0)?;
    let mut id_space = this.id_map().id_space();
    for (row, (external, dense)) in other.id_map().pairs().iter().enumerate() {
        if (row as u64).is_multiple_of(ROWS_PER_POLL) {
            resources.interrupts.check().map_err(ComputeError::from)?;
        }
        dense_ids[*dense as usize] = this.id_map().dense_id(external).unwrap_or_else(|| {
            id_space += 1;
            id_space - 1
        });
    }
    let mut relations = this.relations().clone();
    let relation_indexes = other
        .relations()
        .names()
        .iter()
        .map(|name| relations.intern(name))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IdAlignment { dense_ids, id_space, relations, relation_indexes, _charge: charge })
}

/// `other`'s CSRs in the shared ids of `alignment` (made by [`align_ids`]
/// for `other`), charged to the budget (the edge list it is rebuilt from,
/// the new CSRs and the build's temporaries) and polling the interrupts
/// every 64K rows.
pub fn align_graph<E: ExternalId, I: Interrupts>(
    resources: &mut Resources<'_, I>,
    other: &GraphVersion<'_, E>,
    alignment: &IdAlignment,
) -> Result<AlignedGraph, SpectralError> {
    if alignment.dense_ids.len() as u64 != other.id_map().id_space() {
        let (expected, found) = (other.id_map().id_space(), alignment.dense_ids.len() as u64);
        return Err(SpectralError::MappingLength { expected, found });
    }
    let all = other.graph().all_relations();
    let (weighted, reverse) = (all.forward().is_weighted(), all.reverse().is_some());
    let edges: u64 = other.graph().relations().iter().map(|r| r.forward().edge_count()).sum();
    let relation_count = alignment.relations.len();
    // `edit` reads only the relations' CSRs, so the rebuild needs no
    // merged CSR.
    let layout = CsrLayout { build_reverse: reverse, store_merged: false };
    let charge = resources.memory.charge(rebuild_bytes(
        edges,
        relation_count,
        alignment.id_space,
        layout,
    ))?;
    let mut list = EdgeList::for_id_space(alignment.id_space, weighted);
    list.reserve(edges as usize)?;
    let dense = |old: DenseId| alignment.dense_ids[old as usize];
    for (&relation, csr) in alignment.relation_indexes.iter().zip(other.graph().relations()) {
        let csr = csr.forward();
        for source in 0..csr.id_space() {
            if source.is_multiple_of(ROWS_PER_POLL) {
                resources.interrupts.check().map_err(ComputeError::from)?;
            }
            let weights = csr.neighbour_weights(source);
            for (i, target) in csr.neighbours(source).enumerate() {
                let weight = weights.map_or(1.0, |weights| weights[i]);
                list.push(relation, dense(source), dense(target), weight)?;
            }
        }
    }
    let graph = GraphCsr::build_as(list, relation_count, alignment.id_space, layout)?;
    Ok(AlignedGraph { graph, _charge: charge })
}

/// The bytes a rebuild of `edges` edges holds at its peak, over-estimated:
/// the edge list (endpoints, relation and weight), each relation's and the
/// merged CSR's targets and weights, doubled for a reverse, the offsets,
/// and the build's per-node temporaries. Those are the rows' offsets, kept
/// until the relations' CSRs are split off, and a scatter cursor (one per
/// transpose too, after the rows are freed): 16 bytes a node at once.
fn rebuild_bytes(edges: u64, relation_count: usize, id_space: u64, layout: CsrLayout) -> u64 {
    let directions: u64 = if layout.build_reverse { 2 } else { 1 };
    let list = edges.saturating_mul(32);
    let csrs = edges.saturating_mul(16 * 2 * directions);
    let temporaries = id_space.saturating_add(1).saturating_mul(16);
    list.saturating_add(csrs).saturating_add(temporaries).saturating_add(GraphCsr::offsets_bytes(
        relation_count,
        id_space,
        layout,
    ))
}

// Not under Miri: these tests build the compute pool, whose work stealing
// runs through crossbeam-epoch, which Miri's Stacked Borrows model rejects
// (see compute::workers).
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::{ComputeError, ComputeMemory, Workers};
    use crate::interrupts::{Cancelled, Uninterrupted};
    use crate::version::history::Build;

    fn edges(graph: &GraphCsr, relation: usize) -> Vec<(DenseId, DenseId)> {
        let csr = graph.relations()[relation].forward();
        (0..csr.id_space()).flat_map(|s| csr.neighbours(s).map(move |t| (s, t))).collect()
    }

    fn with<R>(limit: u64, f: impl FnOnce(&mut Resources<'_, Uninterrupted>) -> R) -> R {
        let workers = Workers::new(1).unwrap();
        let memory = ComputeMemory::new(limit);
        f(&mut Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted })
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
        with(1 << 20, |r| {
            let ids = align_ids(r, &this.version(), &other.version()).unwrap();
            assert_eq!(ids.dense_ids, [3, 1, 2, 4]);
            assert_eq!(ids.id_space, 5);
            assert_eq!(ids.relations.names(), ["r0", "r1"]);
            let aligned = align_graph(r, &other.version(), &ids).unwrap().graph;
            assert_eq!(edges(&aligned, 0), [(1, 2), (4, 1)]);
            assert_eq!(edges(&aligned, 1), [(3, 4)]);
            assert_eq!(aligned.all_relations().forward().id_space(), 5);
            assert!(aligned.all_relations().reverse().is_some());
        });
    }

    #[test]
    fn a_version_aligned_to_itself_is_unchanged() {
        let v = Build::first(&[1, 2, 3], &[(0, 1, 2, 3), (1, 3, 1, 2)], true, false);
        with(1 << 20, |r| {
            let ids = align_ids(r, &v.version(), &v.version()).unwrap();
            assert_eq!((ids.dense_ids.as_slice(), ids.id_space), (&[0, 1, 2][..], 3));
            assert_eq!(&ids.relations, &v.relations);
            // Rebuilt without the merged CSR, which `edit` does not read.
            let aligned = align_graph(r, &v.version(), &ids).unwrap().graph;
            assert_eq!(aligned, v.without_merged().graph);
        });
    }

    #[test]
    fn the_rebuilt_csrs_are_charged_to_the_budget() {
        let v = Build::first(&[1, 2, 3], &[(0, 1, 2, 3), (1, 3, 1, 2)], true, true);
        with(200, |r| {
            let ids = align_ids(r, &v.version(), &v.version()).unwrap();
            assert!(matches!(
                align_graph(r, &v.version(), &ids),
                Err(SpectralError::Compute(ComputeError::MemoryLimit { .. }))
            ));
        });
    }

    struct Stop;

    impl Interrupts for Stop {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }

    #[test]
    fn rebuilding_polls_the_interrupts() {
        let v = Build::first(&[1, 2], &[(0, 1, 2, 1)], false, false);
        let workers = Workers::new(1).unwrap();
        let memory = ComputeMemory::new(1 << 20);
        let ids = with(1 << 20, |r| align_ids(r, &v.version(), &v.version()).unwrap());
        let mut resources = Resources { workers: &workers, memory: &memory, interrupts: &mut Stop };
        assert_eq!(
            align_graph(&mut resources, &v.version(), &ids).unwrap_err(),
            SpectralError::Compute(ComputeError::Cancelled)
        );
    }
}
