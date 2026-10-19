//! Weakly connected components (DESIGN.md §9.3, `warren.components`).
//!
//! Union–find over the forward all-relations CSR: edge direction does not
//! matter for weak connectivity, so no reverse CSR is needed. Unions always
//! hang the larger root under the smaller, so every root is the smallest
//! dense id of its component and every node's parent is at most the node.

use super::stats::Progress;
use super::{Interrupts, TraversalError, Traversed};
use crate::graph::{memory, DenseId, GraphCsr};

/// The component id of every dense id in the id space, indexed by dense
/// id: the smallest dense id in its weakly connected component, so labels
/// are deterministic and comparable across graph versions.
///
/// A tombstoned node has no edges, so it is its own component. The domain
/// labels every dense id; leaving tombstoned nodes out of the output is the
/// adapter's job.
pub fn components(
    graph: &GraphCsr,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<Vec<DenseId>>, TraversalError> {
    let forward = graph.all_relations().forward();
    let id_space = forward.id_space();
    let mut progress = Progress::new(interrupts);
    let mut parents = memory::with_capacity(id_space as usize)?;
    parents.extend(0..id_space);
    for source in 0..id_space {
        progress.node_visited()?;
        for target in forward.neighbours(source) {
            progress.edge_traversed()?;
            unite(&mut parents, source, target);
        }
    }
    for node in 0..id_space as usize {
        parents[node] = parents[parents[node] as usize];
    }
    Ok(progress.traversed(parents))
}

/// Joins the sets of `a` and `b` under the smaller of their roots.
fn unite(parents: &mut [DenseId], a: DenseId, b: DenseId) {
    let (a, b) = (root(parents, a), root(parents, b));
    let (smaller, larger) = if a < b { (a, b) } else { (b, a) };
    parents[larger as usize] = smaller;
}

/// The root of `node`'s set, halving the path on the way.
fn root(parents: &mut [DenseId], mut node: DenseId) -> DenseId {
    while parents[node as usize] != node {
        let grandparent = parents[parents[node as usize] as usize];
        parents[node as usize] = grandparent;
        node = grandparent;
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traversal::example::{directed, graph};
    use crate::traversal::TraversalStats;
    use crate::traversal::{Cancellation, Uninterrupted};

    #[test]
    fn each_node_is_labelled_with_the_smallest_dense_id_in_its_component() {
        // {0, 2, 4} via 4 → 2 → 0, {1, 3, 5} via 3 → 1 and 5 → 1, 6 alone
        let graph = directed(7, &[(4, 2), (2, 0), (3, 1), (5, 1), (6, 6)]);
        let found = components(&graph, &mut Uninterrupted).unwrap();
        assert_eq!(found.result, [0, 1, 0, 1, 0, 1, 6]);
        assert_eq!(
            found.stats,
            TraversalStats { nodes_visited: 7, edges_traversed: 5, truncated: false }
        );
    }

    #[test]
    fn components_need_no_reverse_csr_and_unite_all_relations() {
        let graph = graph(4, 2, &[(0, 3, 2, 1.0), (1, 2, 1, 1.0)], false);
        assert_eq!(components(&graph, &mut Uninterrupted).unwrap().result, [0, 1, 1, 1]);
    }

    #[test]
    fn a_raised_cancellation_stops_the_labelling() {
        let graph = directed(2, &[(0, 1)]);
        let mut cancellation = Cancellation::new();
        cancellation.cancel();
        assert_eq!(components(&graph, &mut cancellation), Err(TraversalError::Cancelled));
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::*;
    use crate::traversal::reference::{scenarios, Scenario};
    use crate::traversal::Direction;
    use crate::traversal::Uninterrupted;

    proptest! {
        #[test]
        fn components_match_a_naive_undirected_search(scenario in scenarios()) {
            let found = components(&scenario.graph(), &mut Uninterrupted).unwrap().result;
            prop_assert_eq!(found.len() as u64, scenario.id_space);
            let steps = scenario.steps(Direction::Both, &None);
            for node in 0..scenario.id_space {
                let component = Scenario::depths(&steps, &[node], None);
                let smallest = component.keys().next().copied();
                prop_assert_eq!(Some(found[node as usize]), smallest, "node {}", node);
                if scenario.tombstones.contains(&node) {
                    prop_assert_eq!(found[node as usize], node);
                }
            }
        }
    }
}
