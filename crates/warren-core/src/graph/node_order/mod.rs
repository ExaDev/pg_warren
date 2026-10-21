//! Node order: how a graph's dense ids are ordered where they may change,
//! at the first build and at compaction (DESIGN.md §5.2, §14 item 22).
//!
//! [`NodeOrder::Id`] keeps external-id order. [`NodeOrder::Locality`]
//! renumbers the provisional ids of that order by a [`Permutation`] that
//! gives neighbours nearby dense ids, so that the per-node data a kernel
//! reads at each edge (a vector's value, a visited bit) shares cache lines.
//! The permutation is computed from an [`EdgeList`] and applied to it and
//! to the [`IdMap`](super::IdMap) before the CSRs are built.

mod locality;

use std::fmt::{self, Display};
use std::str::FromStr;

use super::{BuildBuffer, BuildMemory, DenseId, EdgeList, GraphError};

/// How dense ids are ordered at the first build and at compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeOrder {
    /// External-id (`id_type`) order.
    #[default]
    Id,
    /// Reverse Cuthill–McKee order over the undirected view of the edges.
    Locality,
}

impl NodeOrder {
    /// The order's SQL name: `'id'` or `'locality'`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Locality => "locality",
        }
    }
}

impl Display for NodeOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for NodeOrder {
    type Err = GraphError;

    fn from_str(name: &str) -> Result<Self, GraphError> {
        [Self::Id, Self::Locality]
            .into_iter()
            .find(|order| order.name() == name)
            .ok_or_else(|| GraphError::UnknownNodeOrder(name.to_owned()))
    }
}

/// A bijection of an id space onto itself: the new dense id of every old
/// dense id. Charged to the [`BuildMemory`] it was computed under.
#[derive(Debug)]
pub struct Permutation {
    new_by_old: BuildBuffer<DenseId>,
}

impl Permutation {
    /// The locality order of `edges` over `id_space` (DESIGN.md §14 item
    /// 22): Reverse Cuthill–McKee over the undirected view of the edges,
    /// ignoring direction, relations, weights, self-loops and parallel
    /// edges. Each connected component is numbered by a breadth-first
    /// search from its node of least degree (lowest id first), visiting each
    /// node's unvisited neighbours by ascending degree, then id; components
    /// are taken in that same order, so isolated nodes come first; the whole
    /// order is then reversed. It depends only on the edges as a set, not
    /// on their order or multiplicity.
    ///
    /// O(N + M) time besides sorting each node's neighbours, and scratch of
    /// 8 bytes per node for the offsets and the permutation, two endpoint
    /// widths per node for the order, and two endpoint widths per edge for
    /// the undirected adjacency, all charged to `memory`.
    pub fn locality(
        edges: &EdgeList,
        id_space: u64,
        memory: &BuildMemory,
    ) -> Result<Self, GraphError> {
        locality::order(edges, id_space, memory)
    }

    /// The new dense id of `old`, which must lie in the id space.
    pub fn new_dense_id(&self, old: DenseId) -> DenseId {
        self.new_by_old[old as usize]
    }

    /// The id space it permutes.
    pub fn id_space(&self) -> u64 {
        self.new_by_old.len() as u64
    }

    /// The heap bytes it holds.
    pub fn bytes(&self) -> u64 {
        self.new_by_old.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::RelationDictionary;
    use proptest::prelude::*;

    /// An unweighted, single-relation edge list over `id_space`.
    pub(crate) fn edge_list(id_space: u64, edges: &[(DenseId, DenseId)]) -> EdgeList {
        let relation = RelationDictionary::new().intern("").unwrap();
        let mut list = EdgeList::for_id_space(id_space, false);
        for &(source, target) in edges {
            list.push(relation, source, target, 1.0).unwrap();
        }
        list
    }

    fn locality(id_space: u64, edges: &[(DenseId, DenseId)]) -> Vec<DenseId> {
        let memory = BuildMemory::unlimited();
        let permutation = Permutation::locality(&edge_list(id_space, edges), id_space, &memory);
        let permutation = permutation.unwrap();
        (0..id_space).map(|old| permutation.new_dense_id(old)).collect()
    }

    #[test]
    fn node_orders_have_sql_names() {
        assert_eq!("id".parse::<NodeOrder>(), Ok(NodeOrder::Id));
        assert_eq!("locality".parse::<NodeOrder>(), Ok(NodeOrder::Locality));
        assert_eq!(
            "Locality".parse::<NodeOrder>(),
            Err(GraphError::UnknownNodeOrder("Locality".to_owned()))
        );
        assert_eq!(NodeOrder::Locality.to_string(), "locality");
        assert_eq!(NodeOrder::default(), NodeOrder::Id);
    }

    /// A path whose ids are scrambled gets consecutive ids along it, from
    /// the end that Cuthill–McKee starts at (the lower id of the two ends
    /// of least degree), reversed.
    #[test]
    fn a_scrambled_path_is_numbered_along_the_path() {
        // 0 - 3 - 5 - 1 - 4 - 2
        let new = locality(6, &[(0, 3), (5, 3), (5, 1), (1, 4), (4, 2)]);
        // Cuthill–McKee from 0: 0 3 5 1 4 2; reversed.
        assert_eq!(new, [5, 2, 0, 4, 1, 3]);
    }

    /// Neighbours are visited by ascending degree, then id; each component
    /// starts at its node of least degree; isolated nodes are components of
    /// their own, numbered first by Cuthill–McKee and so last once reversed.
    #[test]
    fn components_and_isolated_nodes_are_ordered_deterministically() {
        // Star around 4 with leaves 0, 2, 6 and 6 - 7; edge 3 - 5; 1 isolated.
        let new = locality(8, &[(4, 0), (2, 4), (4, 6), (7, 6), (5, 3)]);
        // Cuthill–McKee: 1 | 0 4 2 6 7 | 3 5 (degree 1 before 2, then by id).
        let cuthill_mckee = [1, 0, 4, 2, 6, 7, 3, 5];
        let mut expected = [0; 8];
        for (position, &old) in cuthill_mckee.iter().enumerate() {
            expected[old as usize] = 7 - position as DenseId;
        }
        assert_eq!(new, expected);
    }

    #[test]
    fn the_order_ignores_direction_self_loops_and_parallel_edges() {
        let plain = locality(4, &[(0, 2), (2, 1), (1, 3)]);
        assert_eq!(locality(4, &[(2, 0), (2, 1), (1, 2), (3, 1), (3, 3), (0, 2)]), plain);
    }

    #[test]
    fn an_edge_outside_the_id_space_is_refused() {
        let list = edge_list(4, &[(0, 3)]);
        assert_eq!(
            Permutation::locality(&list, 3, &BuildMemory::unlimited()).unwrap_err(),
            GraphError::EdgeOutsideIdSpace { from: 0, to: 3 }
        );
    }

    #[test]
    fn an_empty_id_space_has_an_empty_permutation() {
        assert_eq!(locality(0, &[]), Vec::<DenseId>::new());
    }

    #[test]
    fn the_scratch_and_the_permutation_are_charged() {
        let edges: Vec<_> = (0..1000).map(|n| (n, (n * 7 + 3) % 1000)).collect();
        let list = edge_list(1000, &edges);
        let memory = BuildMemory::unlimited();
        let before = memory.in_use();
        let permutation = Permutation::locality(&list, 1000, &memory).unwrap();
        assert_eq!(memory.in_use() - before, permutation.bytes());
        assert_eq!(permutation.bytes(), 8 * 1000);
        // Offsets, the permutation, the order and two endpoints per edge.
        assert!(memory.peak() - before >= 8 * 1001 + 8 * 1000 + 4 * 1000 + 4 * 2 * 1000);

        let small = BuildMemory::new(memory.peak() - before - 1);
        assert!(matches!(
            Permutation::locality(&list, 1000, &small),
            Err(GraphError::BuildMemoryLimit { .. })
        ));
        assert_eq!(small.in_use(), 0, "a refusal releases what was charged");
    }

    fn graphs() -> impl Strategy<Value = (u64, Vec<(DenseId, DenseId)>)> {
        (0u64..40).prop_flat_map(|id_space| {
            let edge = (0..id_space.max(1), 0..id_space.max(1));
            let edges = prop::collection::vec(edge, if id_space == 0 { 0..1 } else { 0..120 });
            (Just(id_space), edges)
        })
    }

    proptest! {
        #[test]
        fn the_locality_order_is_a_bijection_of_the_id_space((id_space, edges) in graphs()) {
            let mut new = locality(id_space, &edges);
            new.sort_unstable();
            prop_assert_eq!(new, (0..id_space).collect::<Vec<_>>());
        }

        #[test]
        fn the_locality_order_depends_only_on_the_edge_set(
            (id_space, edges) in graphs(),
            rotation in any::<prop::sample::Index>(),
        ) {
            let mut shuffled: Vec<_> = edges.iter().map(|&(s, t)| (t, s)).collect();
            shuffled.extend(edges.iter().take(5));
            if !shuffled.is_empty() {
                let by = rotation.index(shuffled.len());
                shuffled.rotate_left(by);
            }
            prop_assert_eq!(locality(id_space, &shuffled), locality(id_space, &edges));
        }

        /// Every component gets a contiguous range of new ids.
        #[test]
        fn each_component_gets_a_contiguous_range((id_space, edges) in graphs()) {
            let new = locality(id_space, &edges);
            let mut component: Vec<DenseId> = (0..id_space).collect();
            fn root(parents: &mut [DenseId], mut node: DenseId) -> DenseId {
                while parents[node as usize] != node {
                    node = parents[node as usize];
                }
                node
            }
            for &(source, target) in &edges {
                let (a, b) = (root(&mut component, source), root(&mut component, target));
                component[a.max(b) as usize] = a.min(b);
            }
            let mut by_new = vec![0; id_space as usize];
            for old in 0..id_space {
                by_new[new[old as usize] as usize] = root(&mut component, old);
            }
            let mut seen = Vec::new();
            for window in by_new.chunk_by(|a, b| a == b) {
                prop_assert!(!seen.contains(&window[0]), "component {} split", window[0]);
                seen.push(window[0]);
            }
        }
    }
}
