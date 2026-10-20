//! Reachability: whether one node reaches another, and every node a set of
//! starts reaches (DESIGN.md §9.3, `warren.reachable`,
//! `warren.reachable_set`).

use std::ops::ControlFlow;

use super::bfs::search_each;
use super::level::{Admission, Edges, LevelSearch};
use super::scratch::Rows;
use super::stats::Progress;
use super::{Adjacency, Limits, TraversalError, TraversalStats, Traversed, Visit};
use crate::graph::DenseId;
use crate::interrupts::Interrupts;

/// Whether `target` is within `max_depth` edges of `start` along
/// `adjacency` (unbounded if `None`). A node reaches itself. The search
/// stops as soon as it reaches `target`.
pub fn reachable(
    adjacency: &Adjacency<'_>,
    start: DenseId,
    target: DenseId,
    max_depth: Option<u32>,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<bool>, TraversalError> {
    if target >= adjacency.id_space() {
        return Err(TraversalError::NodeOutsideIdSpace { node: target });
    }
    let mut found = false;
    let mut admit = |visit: Visit| {
        found = visit.node == target;
        Ok(if found { Admission::AdmitAndStop } else { Admission::Admit })
    };
    let mut progress = Progress::new(interrupts);
    let edges = Edges::new(adjacency, None)?;
    LevelSearch::new(adjacency.id_space()).run(
        edges,
        &[start],
        max_depth,
        &mut progress,
        &mut admit,
    )?;
    Ok(progress.traversed(found))
}

/// Every node within `max_depth` edges of `starts` along `adjacency`
/// (unbounded if `None`), starts included, once each, in BFS order.
/// Collects [`reachable_set_each`].
pub fn reachable_set(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    max_depth: Option<u32>,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<Vec<DenseId>>, TraversalError> {
    let mut nodes = Rows::new();
    let stats =
        reachable_set_each(adjacency, starts, max_depth, interrupts, |node| nodes.push(node))?;
    Ok(Traversed { result: nodes.finish()?, stats })
}

/// [`reachable_set`], handing each node to `visitor` as it is reached
/// instead of collecting them; returning [`ControlFlow::Break`] stops the
/// scan after that node.
///
/// Nodes come by depth. A level whose frontier is wide (more than a
/// twentieth of the id space, and more nodes than are left unvisited) is
/// pulled through the adjacency read backwards, when the graph has the
/// CSRs for that, and its nodes come in ascending dense id order; other
/// levels come in the order reached, as in [`bfs`](super::bfs). A pulled
/// level scans only unvisited nodes' rows, and each only until it meets
/// the frontier, so `edges_traversed` counts fewer row entries than a
/// push-only search would.
pub fn reachable_set_each(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    max_depth: Option<u32>,
    interrupts: &mut dyn Interrupts,
    mut visitor: impl FnMut(DenseId) -> ControlFlow<()>,
) -> Result<TraversalStats, TraversalError> {
    let limits = Limits { max_depth, max_results: None };
    let reverse = adjacency.reversed().ok();
    let edges = Edges::new(adjacency, None)?.pulling(reverse.as_ref());
    search_each(edges, starts, limits, interrupts, |visit| visitor(visit.node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Direction;
    use crate::graph::GraphCsr;
    use crate::graph::RelationSelection;
    use crate::interrupts::Uninterrupted;
    use crate::traversal::example::directed;
    use crate::traversal::{bfs, Limits};

    // 0 → 1 → 2 → 3, 4 → 3, 5 isolated
    fn example() -> GraphCsr {
        directed(6, &[(0, 1), (1, 2), (2, 3), (4, 3)])
    }

    fn out(graph: &GraphCsr) -> Adjacency<'_> {
        Adjacency::new(graph, Direction::Out, RelationSelection::All).unwrap()
    }

    #[test]
    fn reachable_follows_edges_within_the_depth() {
        let graph = example();
        let out = out(&graph);
        let mut never = Uninterrupted;
        assert!(reachable(&out, 0, 3, None, &mut never).unwrap().result);
        assert!(reachable(&out, 0, 3, Some(3), &mut never).unwrap().result);
        assert!(!reachable(&out, 0, 3, Some(2), &mut never).unwrap().result);
        assert!(!reachable(&out, 3, 0, None, &mut never).unwrap().result);
        assert!(!reachable(&out, 0, 4, None, &mut never).unwrap().result);
        assert!(reachable(&out, 5, 5, Some(0), &mut never).unwrap().result);
    }

    #[test]
    fn reachable_stops_once_the_target_is_reached() {
        let graph = directed(4, &[(0, 1), (0, 2), (2, 3)]);
        let found = reachable(&out(&graph), 0, 1, None, &mut Uninterrupted).unwrap();
        assert_eq!(found.stats.nodes_visited, 2);
        assert!(!found.stats.truncated);
    }

    #[test]
    fn reachable_rejects_a_target_outside_the_id_space() {
        let graph = example();
        assert_eq!(
            reachable(&out(&graph), 0, 6, None, &mut Uninterrupted).unwrap_err(),
            TraversalError::NodeOutsideIdSpace { node: 6 }
        );
    }

    #[test]
    fn reachable_set_each_streams_nodes_until_the_visitor_stops_it() {
        let graph = example();
        let mut seen = Vec::new();
        let stats = reachable_set_each(&out(&graph), &[0], None, &mut Uninterrupted, |node| {
            seen.push(node);
            if node == 1 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        assert_eq!(seen, [0, 1]);
        assert_eq!(stats.nodes_visited, 2);
    }

    // 0 → 1, 2, 3; 1 → 5; 2 → 4: the frontier {1, 2, 3} outnumbers the
    // unvisited {4, 5}, so depth 2 is pulled from the reverse CSR.
    fn wide() -> [(DenseId, DenseId); 5] {
        [(0, 1), (0, 2), (0, 3), (1, 5), (2, 4)]
    }

    #[test]
    fn reachable_set_pulls_a_wide_level_in_ascending_dense_id_order() {
        let graph = directed(6, &wide());
        let found = reachable_set(&out(&graph), &[0], None, &mut Uninterrupted).unwrap();
        assert_eq!(found.result, [0, 1, 2, 3, 4, 5]);
        assert_eq!(found.stats.nodes_visited, 6);
        assert_eq!(found.stats.edges_traversed, 5, "three pushed, one in-edge each for 4 and 5");
        let bfs = bfs(&out(&graph), &[0], None, Limits::default(), &mut Uninterrupted).unwrap();
        let order: Vec<_> = bfs.result.iter().map(|visit| visit.node).collect();
        assert_eq!(order, [0, 1, 2, 3, 5, 4], "bfs always pushes");
    }

    #[test]
    fn reachable_set_pushes_every_level_without_a_reverse_csr() {
        let edges: Vec<_> = wide().iter().map(|&(s, t)| (0, s, t, 1.0)).collect();
        let graph = crate::traversal::example::graph(6, 1, &edges, false);
        let found = reachable_set(&out(&graph), &[0], None, &mut Uninterrupted).unwrap();
        assert_eq!(found.result, [0, 1, 2, 3, 5, 4]);
    }

    #[test]
    fn reachable_set_holds_the_starts_and_everything_they_reach() {
        let graph = example();
        let out = out(&graph);
        let mut never = Uninterrupted;
        assert_eq!(reachable_set(&out, &[1, 4], None, &mut never).unwrap().result, [1, 4, 2, 3]);
        assert_eq!(reachable_set(&out, &[0], Some(1), &mut never).unwrap().result, [0, 1]);
        assert_eq!(reachable_set(&out, &[], None, &mut never).unwrap().result, [] as [DenseId; 0]);
    }
}

#[cfg(test)]
mod properties {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::interrupts::Uninterrupted;
    use crate::traversal::reference::{directions, scenarios, selections, Scenario};

    proptest! {
        #[test]
        fn reachability_matches_a_naive_reference(
            (scenario, selection, starts) in scenarios().prop_flat_map(|scenario| {
                let n = scenario.id_space;
                let relation_count = scenario.relation_count;
                (Just(scenario), selections(relation_count), prop::collection::vec(0..n, 1..3))
            }),
            direction in directions(),
            max_depth in prop::option::of(0u32..4),
        ) {
            let graph = scenario.graph();
            let indexes = scenario.indexes(&selection);
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let mut never = Uninterrupted;
            let depths = Scenario::depths(&scenario.steps(direction, &selection), &starts, None);
            let expected: BTreeSet<DenseId> = depths
                .iter()
                .filter(|&(_, &depth)| max_depth.is_none_or(|max| depth <= max))
                .map(|(&node, _)| node)
                .collect();

            let set = reachable_set(&adjacency, &starts, max_depth, &mut never).unwrap().result;
            prop_assert!(set.windows(2).all(|w| depths[&w[0]] <= depths[&w[1]]), "not by depth");
            prop_assert_eq!(set.len(), expected.len());
            prop_assert_eq!(set.into_iter().collect::<BTreeSet<_>>(), expected.clone());

            let from = [starts[0]];
            let depths = Scenario::depths(&scenario.steps(direction, &selection), &from, None);
            for target in 0..scenario.id_space {
                let within = depths.get(&target).is_some_and(|&d| max_depth.is_none_or(|max| d <= max));
                let found = reachable(&adjacency, from[0], target, max_depth, &mut never).unwrap();
                prop_assert_eq!(found.result, within, "target {}", target);
            }
        }
    }
}
