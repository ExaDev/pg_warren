//! Compaction: renumbering a graph version to remove its tombstones
//! (DESIGN.md §5.2, `warren.compact`).

use super::{GraphVersion, VersionError};
use crate::graph::{
    memory, DenseId, EdgeList, ExternalId, GraphCsr, GraphError, IdMap, Tombstones,
};

/// The old → new dense-id mapping of a compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renumbering {
    new_by_old: Vec<Option<DenseId>>,
    old_by_new: Vec<DenseId>,
}

impl Renumbering {
    /// The new dense id of the node at `old`; `None` if it was tombstoned
    /// or lies outside the old id space.
    pub fn new_dense_id(&self, old: DenseId) -> Option<DenseId> {
        usize::try_from(old).ok().and_then(|old| self.new_by_old.get(old)).copied().flatten()
    }

    /// The old dense id of the node at `new`.
    pub fn old_dense_id(&self, new: DenseId) -> Option<DenseId> {
        usize::try_from(new).ok().and_then(|new| self.old_by_new.get(new)).copied()
    }

    /// The old dense ids in new dense-id order: entry `i` is the old dense id
    /// of new dense id `i`. Use it to carry a vector across the compaction.
    pub fn old_dense_ids(&self) -> &[DenseId] {
        &self.old_by_new
    }

    /// The id space before compaction.
    pub fn old_id_space(&self) -> u64 {
        self.new_by_old.len() as u64
    }

    /// The id space after compaction: the number of live nodes.
    pub fn new_id_space(&self) -> u64 {
        self.old_by_new.len() as u64
    }
}

/// A compacted version: a new id space with no tombstones.
#[derive(Debug, Clone, PartialEq)]
pub struct Compaction<E> {
    /// Dense ids `0..node_count` in external-id order, no tombstones.
    pub id_map: IdMap<E>,
    /// The version's CSRs in the new dense ids. Its relation dictionary is
    /// unchanged.
    pub graph: GraphCsr,
    /// How the old dense ids map to the new ones.
    pub renumbering: Renumbering,
}

/// Renumbers `version` to remove its tombstones: live nodes get dense ids
/// `0..node_count` in external-id order, and every CSR (per relation,
/// merged, forward and reverse) is rebuilt in the new ids through
/// [`GraphCsr::build`], keeping weights and whether a reverse is built.
///
/// O(N + M) plus the per-row sorts of a build. The compacted version reports
/// no deduplicated edges, since compaction folds none. Peak memory is the
/// old version, its edge list in new ids and the new version.
pub fn compact<E: ExternalId>(
    version: &GraphVersion<'_, E>,
) -> Result<Compaction<E>, VersionError> {
    let (id_map, renumbering) = renumbered(version.id_map())?;
    let graph = rebuilt(version, &renumbering)?;
    Ok(Compaction { id_map, graph, renumbering })
}

/// The compacted id map and the renumbering, from one walk over the id map
/// in external-id order.
fn renumbered<E: ExternalId>(id_map: &IdMap<E>) -> Result<(IdMap<E>, Renumbering), GraphError> {
    let live = (id_map.node_count()) as usize;
    let mut new_by_old = memory::filled(id_map.id_space() as usize, None)?;
    let mut old_by_new = memory::with_capacity(live)?;
    let mut pairs = memory::with_capacity(live)?;
    for &(external, old) in id_map.pairs() {
        if id_map.tombstones().contains(old) {
            continue;
        }
        let new = old_by_new.len() as DenseId;
        new_by_old[old as usize] = Some(new);
        old_by_new.push(old);
        pairs.push((external, new));
    }
    let id_map = IdMap::from_parts(pairs, Tombstones::new(live as u64)?)?;
    Ok((id_map, Renumbering { new_by_old, old_by_new }))
}

/// The version's CSRs rebuilt from its per-relation forward edges in new ids.
fn rebuilt<E: ExternalId>(
    version: &GraphVersion<'_, E>,
    renumbering: &Renumbering,
) -> Result<GraphCsr, VersionError> {
    let graph = version.graph();
    let all = graph.all_relations();
    let mut edges = EdgeList::new(all.forward().is_weighted());
    for name in version.relations().names() {
        let Some(relation) = version.relations().get(name) else { continue };
        let Some(csr) = graph.relation(relation) else { continue };
        let csr = csr.forward();
        for old_source in 0..csr.id_space() {
            let weights = csr.neighbour_weights(old_source);
            for (i, old_target) in csr.neighbours(old_source).enumerate() {
                let new = |old| renumbering.new_dense_id(old);
                let (Some(source), Some(target)) = (new(old_source), new(old_target)) else {
                    return Err(VersionError::EdgeTouchesTombstone {
                        from: old_source,
                        to: old_target,
                    });
                };
                let weight = weights.map_or(1.0, |weights| weights[i]);
                edges.push(relation, source, target, weight)?;
            }
        }
    }
    let relation_count = graph.relations().len();
    let id_space = renumbering.new_id_space();
    Ok(GraphCsr::build(edges, relation_count, id_space, all.reverse().is_some())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::AnyCsr;
    use crate::version::history::{builds, Build};
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    type Edges = BTreeMap<(i64, i64), f64>;

    /// A CSR's edges in external ids.
    fn edges<E: Copy + Into<i64>>(csr: &AnyCsr, external: impl Fn(DenseId) -> E) -> Edges {
        let mut edges = Edges::new();
        for source in 0..csr.id_space() {
            let weights = csr.neighbour_weights(source);
            for (i, target) in csr.neighbours(source).enumerate() {
                let weight = weights.map_or(1.0, |w| w[i]);
                edges.insert((external(source).into(), external(target).into()), weight);
            }
        }
        edges
    }

    #[test]
    fn compaction_renumbers_live_nodes_in_external_id_order() {
        // 10 → 0, 20 → 1, 30 → 2; then 20 tombstoned and 5 → 3 appended.
        let v1 = Build::first(&[10, 20, 30], &[(0, 10, 30, 1)], false, true);
        let v2 = v1.next(&[5, 10, 30], &[(0, 30, 10, 1), (0, 5, 30, 1)], false, true);
        let compacted = compact(&v2.version()).unwrap();

        assert_eq!(compacted.id_map.id_space(), 3);
        assert_eq!(compacted.id_map.tombstones().count(), 0);
        assert_eq!(compacted.id_map.dense_id(&5), Some(0));
        assert_eq!(compacted.id_map.dense_id(&10), Some(1));
        assert_eq!(compacted.id_map.dense_id(&30), Some(2));
        let renumbering = &compacted.renumbering;
        assert_eq!(renumbering.new_dense_id(3), Some(0));
        assert_eq!(renumbering.new_dense_id(1), None);
        assert_eq!(renumbering.new_dense_id(9), None);
        assert_eq!(renumbering.old_dense_ids(), [3, 0, 2]);
        assert_eq!(renumbering.old_dense_id(2), Some(2));
        assert_eq!((renumbering.old_id_space(), renumbering.new_id_space()), (4, 3));
        let forward = compacted.graph.all_relations().forward();
        assert_eq!(forward.neighbours(0).collect::<Vec<_>>(), [2]);
        assert_eq!(forward.neighbours(2).collect::<Vec<_>>(), [1]);
        let reverse = compacted.graph.all_relations().reverse().unwrap();
        assert_eq!(reverse.neighbours(2).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn a_live_edge_on_a_tombstone_is_an_error() {
        let v1 = Build::first(&[10, 20], &[(0, 10, 20, 1)], false, false);
        let v2 = v1.next(&[10], &[], false, false);
        // v1's CSRs under v2's id map: 20 is tombstoned but has an edge.
        let corrupt = GraphVersion::new(&v2.id_map, &v1.relations, &v1.graph).unwrap();
        assert_eq!(
            compact(&corrupt).unwrap_err(),
            VersionError::EdgeTouchesTombstone { from: 0, to: 1 }
        );
    }

    proptest! {
        #[test]
        fn a_compacted_version_is_isomorphic_under_the_renumbering(
            history in builds(3),
            weighted in any::<bool>(),
            reverse in any::<bool>(),
        ) {
            let mut build = Build::first(&history[0].0, &history[0].1, weighted, reverse);
            for (nodes, rows) in &history[1..] {
                build = build.next(nodes, rows, weighted, reverse);
            }
            let compacted = compact(&build.version()).unwrap();
            let (old, new) = (&build.id_map, &compacted.id_map);

            prop_assert_eq!(new.id_space(), old.node_count());
            prop_assert_eq!(new.node_count(), old.node_count());
            let live: Vec<i64> = (0..old.id_space())
                .filter(|&dense| !old.tombstones().contains(dense))
                .filter_map(|dense| old.external_id(dense))
                .collect();
            let mut sorted = live.clone();
            sorted.sort_unstable();
            for (rank, external) in sorted.iter().enumerate() {
                prop_assert_eq!(new.dense_id(external), Some(rank as DenseId));
                let old_dense = old.dense_id(external).unwrap();
                prop_assert_eq!(compacted.renumbering.new_dense_id(old_dense), Some(rank as DenseId));
                prop_assert_eq!(compacted.renumbering.old_dense_id(rank as DenseId), Some(old_dense));
            }

            let old_external = |dense| old.external_id(dense).unwrap();
            let new_external = |dense| new.external_id(dense).unwrap();
            let pairs = build.graph.relations().iter().zip(compacted.graph.relations())
                .chain([(build.graph.all_relations(), compacted.graph.all_relations())]);
            prop_assert_eq!(compacted.graph.relations().len(), build.graph.relations().len());
            for (before, after) in pairs {
                prop_assert_eq!(after.forward().is_weighted(), weighted);
                prop_assert_eq!(
                    edges(after.forward(), new_external),
                    edges(before.forward(), old_external)
                );
                prop_assert_eq!(after.reverse().is_some(), reverse);
                if let (Some(b), Some(a)) = (before.reverse(), after.reverse()) {
                    prop_assert_eq!(edges(a, new_external), edges(b, old_external));
                }
            }
        }
    }
}
