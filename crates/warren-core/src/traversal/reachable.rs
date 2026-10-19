//! Reachability: whether one node reaches another, and every node a set of
//! starts reaches (DESIGN.md §9.3, `warren.reachable`,
//! `warren.reachable_set`).

use std::ops::ControlFlow;

use super::level::{Admission, LevelSearch};
use super::scratch::Rows;
use super::stats::Progress;
use super::{bfs_each, Adjacency, Limits, TraversalError, TraversalStats, Traversed, Visit};
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
    LevelSearch::new(adjacency, None)?.run(&[start], max_depth, &mut progress, &mut admit)?;
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
pub fn reachable_set_each(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    max_depth: Option<u32>,
    interrupts: &mut dyn Interrupts,
    mut visitor: impl FnMut(DenseId) -> ControlFlow<()>,
) -> Result<TraversalStats, TraversalError> {
    let limits = Limits { max_depth, max_results: None };
    bfs_each(adjacency, starts, None, limits, interrupts, |visit| visitor(visit.node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Direction;
    use crate::graph::GraphCsr;
    use crate::graph::RelationSelection;
    use crate::interrupts::Uninterrupted;
    use crate::traversal::example::directed;

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
