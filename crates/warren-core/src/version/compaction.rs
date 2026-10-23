//! Compaction: renumbering a graph version to remove its tombstones
//! (DESIGN.md §5.2, `warren.compact`).

use super::{GraphVersion, VersionError};
use crate::graph::{
    memory, BuildMemory, DenseId, EdgeList, GraphCsr, GraphError, IdKeys, IdMap, NodeOrder,
    Permutation,
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

    /// Follows the renumbering with `permutation` of the new id space.
    fn permute(&mut self, permutation: &Permutation) -> Result<(), GraphError> {
        let mut old_by_new = memory::filled(self.old_by_new.len(), 0)?;
        for (new, &old) in self.old_by_new.iter().enumerate() {
            let permuted = permutation.new_dense_id(new as DenseId);
            old_by_new[permuted as usize] = old;
            self.new_by_old[old as usize] = Some(permuted);
        }
        self.old_by_new = old_by_new;
        Ok(())
    }
}

/// A compacted version: a new id space with no tombstones.
#[derive(Debug, Clone, PartialEq)]
pub struct Compaction<K> {
    /// Dense ids `0..node_count` in the compaction's node order, no
    /// tombstones.
    pub id_map: IdMap<K>,
    /// The version's CSRs in the new dense ids. Its relation dictionary is
    /// unchanged.
    pub graph: GraphCsr,
    /// How the old dense ids map to the new ones.
    pub renumbering: Renumbering,
}

/// Renumbers `version` to remove its tombstones: live nodes get dense ids
/// `0..node_count` in `order` (DESIGN.md §14 item 22), and every CSR (per
/// relation, merged, forward and reverse) is rebuilt in the new ids through
/// [`GraphCsr::build`], keeping weights and whether a reverse is built.
///
/// O(N + M) plus the per-row sorts of a build. The compacted version reports
/// no deduplicated edges, since compaction folds none. Peak memory is the
/// old version, its edge list in new ids and the new version, plus, for
/// [`NodeOrder::Locality`], the order's scratch and permutation, which are
/// charged to `memory`.
pub fn compact<K: IdKeys>(
    version: &GraphVersion<'_, K>,
    order: NodeOrder,
    memory: &BuildMemory,
) -> Result<Compaction<K>, VersionError> {
    let (mut id_map, mut renumbering) = renumbered(version.id_map())?;
    let mut edges = edge_list(version, &renumbering)?;
    if order == NodeOrder::Locality {
        let permutation = Permutation::locality(&edges, renumbering.new_id_space(), memory)?;
        edges.renumber(&permutation)?;
        id_map.renumber(&permutation, memory)?;
        renumbering.permute(&permutation)?;
    }
    let graph = version.graph();
    let relation_count = graph.relations().len();
    let id_space = renumbering.new_id_space();
    let graph = GraphCsr::build_as(edges, relation_count, id_space, graph.layout())?;
    Ok(Compaction { id_map, graph, renumbering })
}

/// The compacted id map and the renumbering, from one walk over the id map
/// in external-id order.
fn renumbered<K: IdKeys>(id_map: &IdMap<K>) -> Result<(IdMap<K>, Renumbering), GraphError> {
    let live = (id_map.node_count()) as usize;
    let mut new_by_old = memory::filled(id_map.id_space() as usize, None)?;
    let mut old_by_new = memory::with_capacity(live)?;
    for (_, old) in id_map.keys().in_order() {
        if id_map.tombstones().contains(old) {
            continue;
        }
        new_by_old[old as usize] = Some(old_by_new.len() as DenseId);
        old_by_new.push(old);
    }
    let id_map = id_map.compacted(&old_by_new)?;
    Ok((id_map, Renumbering { new_by_old, old_by_new }))
}

/// The version's per-relation forward edges in new ids.
fn edge_list<K: IdKeys>(
    version: &GraphVersion<'_, K>,
    renumbering: &Renumbering,
) -> Result<EdgeList, VersionError> {
    let graph = version.graph();
    let weighted = graph.all_relations().forward().is_weighted();
    let mut edges = EdgeList::for_id_space(renumbering.new_id_space(), weighted);
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
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::AnyCsr;
    use crate::version::history::{builds, Build};
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

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
        let compacted = compact(&v2.version(), NodeOrder::Id, &BuildMemory::unlimited()).unwrap();

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
            compact(&corrupt, NodeOrder::Id, &BuildMemory::unlimited()).unwrap_err(),
            VersionError::EdgeTouchesTombstone { from: 0, to: 1 }
        );
    }

    /// The first build of a graph, in locality order.
    fn locality_build(nodes: &[i64], rows: &[(i64, i64)]) -> Build {
        let id_map = IdMap::new().rebuild(nodes.to_vec()).unwrap();
        let mut relations = crate::graph::RelationDictionary::new();
        let relation = relations.intern("r").unwrap();
        let mut intake = crate::graph::EdgeIntake::directed(&id_map, false);
        for (source, target) in rows {
            intake.push(relation, Some(source), Some(target), 1.0).unwrap();
        }
        let (mut edges, _) = intake.finish();
        let mut id_map = id_map.clone();
        let permutation =
            Permutation::locality(&edges, id_map.id_space(), &BuildMemory::unlimited()).unwrap();
        edges.renumber(&permutation).unwrap();
        id_map.renumber(&permutation, &BuildMemory::unlimited()).unwrap();
        let graph = GraphCsr::build(edges, 1, id_map.id_space(), true).unwrap();
        Build { id_map, relations, graph }
    }

    /// The locality order depends only on the edges and the external-id
    /// order of the live nodes, so compacting a locality build without
    /// tombstones in locality order gives back the same ids.
    #[test]
    fn a_locality_compaction_of_a_locality_build_changes_no_id() {
        let rows = [(50, 10), (10, 40), (40, 20), (20, 30), (60, 60)];
        let build = locality_build(&[10, 20, 30, 40, 50, 60], &rows);
        let memory = BuildMemory::unlimited();
        let compacted = compact(&build.version(), NodeOrder::Locality, &memory).unwrap();
        assert_eq!(compacted.id_map, build.id_map);
        assert_eq!(compacted.graph, build.graph);
        assert_eq!(compacted.renumbering.old_dense_ids(), [0, 1, 2, 3, 4, 5]);
        assert_ne!(build.id_map.dense_id(&10), Some(0), "not external-id order");
    }

    #[test]
    fn a_locality_compaction_orders_the_live_nodes_and_drops_the_tombstones() {
        // A path 30 - 10 - 40 - 20, then 50 appended next to 30, then 10
        // removed: the live graph is 50 - 30 and 40 - 20.
        let v1 = Build::first(
            &[10, 20, 30, 40],
            &[(0, 30, 10, 1), (0, 10, 40, 1), (0, 40, 20, 1)],
            false,
            true,
        );
        let v2 = v1.next(&[20, 30, 40, 50], &[(0, 50, 30, 1), (0, 40, 20, 1)], false, true);
        let memory = BuildMemory::unlimited();
        let compacted = compact(&v2.version(), NodeOrder::Locality, &memory).unwrap();
        let fresh = locality_build(&[20, 30, 40, 50], &[(50, 30), (40, 20)]);
        assert_eq!(compacted.id_map, fresh.id_map);
        assert_eq!(compacted.graph.relations(), fresh.graph.relations());
        let old = |external| v2.id_map.dense_id(&external).unwrap();
        let new = |external| compacted.id_map.dense_id(&external);
        for external in [20, 30, 40, 50] {
            assert_eq!(compacted.renumbering.new_dense_id(old(external)), new(external));
        }
        assert_eq!(compacted.renumbering.new_dense_id(0), None, "10 was tombstoned");
    }

    #[test]
    fn a_locality_compaction_is_refused_past_its_memory_budget() {
        let v1 = Build::first(&[1, 2, 3], &[(0, 1, 2, 1), (0, 2, 3, 1)], false, false);
        let small = BuildMemory::new(16);
        assert!(matches!(
            compact(&v1.version(), NodeOrder::Locality, &small),
            Err(VersionError::Graph(GraphError::BuildMemoryLimit { limit: 16, .. }))
        ));
        assert!(compact(&v1.version(), NodeOrder::Id, &small).is_ok(), "id order needs no scratch");
    }

    proptest! {
        #[test]
        fn a_compacted_version_is_isomorphic_under_the_renumbering(
            history in builds(3),
            weighted in any::<bool>(),
            reverse in any::<bool>(),
            store_merged in any::<bool>(),
            locality in any::<bool>(),
        ) {
            let order = if locality { NodeOrder::Locality } else { NodeOrder::Id };
            let mut build = Build::first(&history[0].0, &history[0].1, weighted, reverse);
            for (nodes, rows) in &history[1..] {
                build = build.next(nodes, rows, weighted, reverse);
            }
            if !store_merged {
                build = build.without_merged();
            }
            let compacted = compact(&build.version(), order, &BuildMemory::unlimited()).unwrap();
            prop_assert_eq!(compacted.graph.layout(), build.graph.layout());
            let (old, new) = (&build.id_map, &compacted.id_map);

            prop_assert_eq!(new.id_space(), old.node_count());
            prop_assert_eq!(new.node_count(), old.node_count());
            let live: Vec<i64> = (0..old.id_space())
                .filter(|&dense| !old.tombstones().contains(dense))
                .filter_map(|dense| old.external_id(dense))
                .copied()
                .collect();
            let mut sorted = live.clone();
            sorted.sort_unstable();
            let mut assigned = Vec::new();
            for (rank, external) in sorted.iter().enumerate() {
                let dense = new.dense_id(external).unwrap();
                if order == NodeOrder::Id {
                    prop_assert_eq!(dense, rank as DenseId);
                }
                assigned.push(dense);
                let old_dense = old.dense_id(external).unwrap();
                prop_assert_eq!(compacted.renumbering.new_dense_id(old_dense), Some(dense));
                prop_assert_eq!(compacted.renumbering.old_dense_id(dense), Some(old_dense));
            }
            assigned.sort_unstable();
            prop_assert_eq!(assigned, (0..old.node_count()).collect::<Vec<_>>());

            let old_external = |dense| *old.external_id(dense).unwrap();
            let new_external = |dense| *new.external_id(dense).unwrap();
            let pairs = build.graph.relations().iter().zip(compacted.graph.relations())
                .chain(build.graph.merged().zip(compacted.graph.merged()));
            prop_assert_eq!(compacted.graph.relations().len(), build.graph.relations().len());
            let all = |graph: &GraphCsr, external: &dyn Fn(DenseId) -> i64| {
                let forward = graph.all_relations().forward();
                let row = |source| forward.neighbours(source).map(move |t| (external(source), external(t)));
                (0..forward.id_space()).flat_map(row).collect::<BTreeSet<_>>()
            };
            prop_assert_eq!(all(&compacted.graph, &new_external), all(&build.graph, &old_external));
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
