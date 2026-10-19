//! Breadth-first search from a set of starts, reporting each node reached
//! with its depth and the node it was reached from (DESIGN.md §9.3,
//! `warren.bfs`).

use std::ops::ControlFlow;

use super::level::{Admission, LevelSearch};
use super::scratch::Rows;
use super::stats::Progress;
use super::{Adjacency, Limits, NodeSet, TraversalError, TraversalStats, Traversed};
use crate::graph::DenseId;
use crate::interrupts::Interrupts;

/// A node reached by a BFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visit {
    /// The node.
    pub node: DenseId,
    /// Its distance in edges from the nearest start.
    pub depth: u32,
    /// The node it was first reached from; `None` for a start.
    pub via: Option<DenseId>,
}

/// Every node reachable from `starts` along `adjacency`, once each, in BFS
/// order (by depth; within a depth, in the order reached).
///
/// Starts are at depth 0 and are always admitted; repeated starts count
/// once. Other nodes are admitted only if they are in `filter`, when given,
/// so the filter prunes the search rather than its output. Stopping at
/// `limits.max_depth` is not truncation; stopping because another row would
/// exceed `limits.max_results` is. Collects [`bfs_each`].
pub fn bfs(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    filter: Option<&NodeSet>,
    limits: Limits,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<Vec<Visit>>, TraversalError> {
    let mut visits = Rows::new();
    let stats =
        bfs_each(adjacency, starts, filter, limits, interrupts, |visit| visits.push(visit))?;
    Ok(Traversed { result: visits.finish()?, stats })
}

/// [`bfs`], handing each visit to `visitor` as it is made instead of
/// collecting them, so a caller (a set-returning function under SQL
/// `LIMIT`) can stop the scan by returning [`ControlFlow::Break`]. The visit
/// that breaks is the last one; stopping this way is not truncation.
pub fn bfs_each(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    filter: Option<&NodeSet>,
    limits: Limits,
    interrupts: &mut dyn Interrupts,
    mut visitor: impl FnMut(Visit) -> ControlFlow<()>,
) -> Result<TraversalStats, TraversalError> {
    let mut progress = Progress::new(interrupts);
    let mut search = LevelSearch::new(adjacency, filter)?;
    let mut emitted = 0u64;
    let mut truncated = false;
    let mut admit = |visit: Visit| -> Result<Admission, TraversalError> {
        if limits.max_results == Some(emitted) {
            truncated = true;
            return Ok(Admission::Stop);
        }
        emitted += 1;
        Ok(match visitor(visit) {
            ControlFlow::Continue(()) => Admission::Admit,
            ControlFlow::Break(()) => Admission::AdmitAndStop,
        })
    };
    search.run(starts, limits.max_depth, &mut progress, &mut admit)?;
    if truncated {
        progress.truncate();
    }
    Ok(progress.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphCsr;
    use crate::graph::RelationSelection;
    use crate::interrupts::{Cancellation, Uninterrupted};
    use crate::traversal::example::directed;
    use crate::traversal::{Direction, TraversalStats};

    // 0 → 1 → 2 → 3, 0 → 4 → 3, 2 → 2, 5 isolated
    fn example() -> GraphCsr {
        directed(6, &[(0, 1), (1, 2), (2, 3), (0, 4), (4, 3), (2, 2)])
    }

    fn run(
        graph: &GraphCsr,
        direction: Direction,
        starts: &[DenseId],
        filter: Option<&NodeSet>,
        limits: Limits,
    ) -> Result<Traversed<Vec<Visit>>, TraversalError> {
        let adjacency = Adjacency::new(graph, direction, RelationSelection::All)?;
        bfs(&adjacency, starts, filter, limits, &mut Uninterrupted)
    }

    fn visit(node: DenseId, depth: u32, via: Option<DenseId>) -> Visit {
        Visit { node, depth, via }
    }

    #[test]
    fn bfs_reports_each_reached_node_once_at_its_shortest_depth() {
        let found = run(&example(), Direction::Out, &[0], None, Limits::default()).unwrap();
        assert_eq!(
            found.result,
            [
                visit(0, 0, None),
                visit(1, 1, Some(0)),
                visit(4, 1, Some(0)),
                visit(2, 2, Some(1)),
                visit(3, 2, Some(4)),
            ]
        );
        assert_eq!(
            found.stats,
            TraversalStats { nodes_visited: 5, edges_traversed: 6, truncated: false }
        );
    }

    #[test]
    fn several_starts_are_all_at_depth_zero_and_repeats_are_dropped() {
        let found = run(&example(), Direction::Out, &[2, 4, 2], None, Limits::default()).unwrap();
        assert_eq!(found.result, [visit(2, 0, None), visit(4, 0, None), visit(3, 1, Some(2))]);
    }

    #[test]
    fn direction_in_follows_edges_backwards() {
        let found = run(&example(), Direction::In, &[3], None, Limits::default()).unwrap();
        let reached: Vec<_> = found.result.iter().map(|v| (v.node, v.depth)).collect();
        assert_eq!(reached, [(3, 0), (2, 1), (4, 1), (1, 2), (0, 2)]);
    }

    #[test]
    fn max_depth_bounds_the_search_without_truncating_it() {
        let limits = Limits { max_depth: Some(1), ..Limits::default() };
        let found = run(&example(), Direction::Out, &[0], None, limits).unwrap();
        assert_eq!(found.result.len(), 3);
        assert!(!found.stats.truncated);
        let limits = Limits { max_depth: Some(0), ..Limits::default() };
        let found = run(&example(), Direction::Out, &[0], None, limits).unwrap();
        assert_eq!(found.result, [visit(0, 0, None)]);
    }

    #[test]
    fn max_results_stops_the_scan_and_marks_it_truncated() {
        let limits = Limits { max_results: Some(2), ..Limits::default() };
        let found = run(&example(), Direction::Out, &[0], None, limits).unwrap();
        assert_eq!(found.result, [visit(0, 0, None), visit(1, 1, Some(0))]);
        assert!(found.stats.truncated);
    }

    #[test]
    fn max_results_that_fits_everything_is_not_truncation() {
        let limits = Limits { max_results: Some(5), ..Limits::default() };
        let found = run(&example(), Direction::Out, &[0], None, limits).unwrap();
        assert_eq!(found.result.len(), 5);
        assert!(!found.stats.truncated);
    }

    #[test]
    fn the_node_filter_prunes_admission_but_starts_are_always_admitted() {
        let graph = example();
        let filter = NodeSet::from_dense_ids(6, [1, 2, 3]).unwrap();
        let found = run(&graph, Direction::Out, &[0], Some(&filter), Limits::default()).unwrap();
        let reached: Vec<_> = found.result.iter().map(|v| (v.node, v.depth)).collect();
        assert_eq!(reached, [(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    fn a_node_filter_over_another_id_space_is_an_error() {
        let filter = NodeSet::new(7).unwrap();
        assert_eq!(
            run(&example(), Direction::Out, &[0], Some(&filter), Limits::default()).unwrap_err(),
            TraversalError::NodeFilterIdSpace { expected: 6, found: 7 }
        );
    }

    #[test]
    fn a_start_outside_the_id_space_is_an_error() {
        assert_eq!(
            run(&example(), Direction::Out, &[6], None, Limits::default()).unwrap_err(),
            TraversalError::NodeOutsideIdSpace { node: 6 }
        );
    }

    #[test]
    fn bfs_each_streams_visits_until_the_visitor_stops_it() {
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let mut seen = Vec::new();
        let stats = bfs_each(&adjacency, &[0], None, Limits::default(), &mut Uninterrupted, |v| {
            seen.push(v);
            if seen.len() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        assert_eq!(seen, [visit(0, 0, None), visit(1, 1, Some(0))]);
        assert_eq!(stats.nodes_visited, 2);
        assert!(!stats.truncated, "a visitor stopping the scan is the caller's LIMIT");
    }

    #[test]
    fn bfs_each_honours_max_results_as_truncation() {
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let limits = Limits { max_results: Some(1), ..Limits::default() };
        let mut count = 0;
        let stats = bfs_each(&adjacency, &[0], None, limits, &mut Uninterrupted, |_| {
            count += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(count, 1);
        assert!(stats.truncated);
    }

    #[test]
    fn a_raised_cancellation_stops_the_search() {
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let mut cancellation = Cancellation::new();
        cancellation.cancel();
        assert_eq!(
            bfs(&adjacency, &[0], None, Limits::default(), &mut cancellation).unwrap_err(),
            TraversalError::Cancelled
        );
    }
}

#[cfg(test)]
mod properties {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::*;
    use crate::graph::RelationSelection;
    use crate::interrupts::Uninterrupted;
    use crate::traversal::reference::{directions, scenarios, selections, Scenario};
    use crate::traversal::Direction;

    proptest! {
        #[test]
        fn bfs_matches_a_naive_reference(
            (scenario, selection, starts, filter) in scenarios().prop_flat_map(|scenario| {
                let n = scenario.id_space;
                let relation_count = scenario.relation_count;
                (
                    Just(scenario),
                    selections(relation_count),
                    prop::collection::vec(0..n, 0..3),
                    prop::option::of(prop::collection::btree_set(0..n, 0..n as usize)),
                )
            }),
            direction in directions(),
            wide in any::<bool>(),
            max_depth in prop::option::of(0u32..4),
        ) {
            let graph = if wide { scenario.wide_graph() } else { scenario.graph() };
            let indexes = scenario.indexes(&selection);
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let node_filter = filter
                .as_ref()
                .map(|f| NodeSet::from_dense_ids(scenario.id_space, f.iter().copied()).unwrap());
            let limits = Limits { max_depth, max_results: None };
            let found = bfs(&adjacency, &starts, node_filter.as_ref(), limits, &mut Uninterrupted)
                .unwrap();

            let steps = scenario.steps(direction, &selection);
            let expected: BTreeMap<DenseId, u32> = Scenario::depths(&steps, &starts, filter.as_ref())
                .into_iter()
                .filter(|&(_, depth)| max_depth.is_none_or(|max| depth <= max))
                .collect();
            let reported: BTreeMap<DenseId, u32> =
                found.result.iter().map(|visit| (visit.node, visit.depth)).collect();
            prop_assert_eq!(reported.len(), found.result.len(), "a node was reported twice");
            prop_assert_eq!(&reported, &expected);
            prop_assert!(found.result.windows(2).all(|w| w[0].depth <= w[1].depth));
            prop_assert!(!found.stats.truncated);
            prop_assert_eq!(found.stats.nodes_visited, expected.len() as u64);
            let edges: BTreeSet<(DenseId, DenseId)> = steps.iter().map(|&(s, t, _)| (s, t)).collect();
            for visit in &found.result {
                match visit.via {
                    None => prop_assert!(visit.depth == 0 && starts.contains(&visit.node)),
                    Some(via) => {
                        prop_assert_eq!(reported.get(&via), Some(&(visit.depth - 1)));
                        prop_assert!(edges.contains(&(via, visit.node)));
                    }
                }
            }
        }

        #[test]
        fn max_results_keeps_a_prefix_of_the_full_bfs(
            scenario in scenarios(),
            start in 0u64..14,
            max_results in 0u64..6,
        ) {
            let graph = scenario.graph();
            let adjacency = Adjacency::new(&graph, Direction::Both, RelationSelection::All).unwrap();
            let start = [start % scenario.id_space];
            let mut never = Uninterrupted;
            let full = bfs(&adjacency, &start, None, Limits::default(), &mut never).unwrap().result;
            let limits = Limits { max_depth: None, max_results: Some(max_results) };
            let capped = bfs(&adjacency, &start, None, limits, &mut never).unwrap();
            let kept = full.len().min(max_results as usize);
            prop_assert_eq!(&capped.result[..], &full[..kept]);
            prop_assert_eq!(capped.stats.truncated, full.len() > kept);
        }
    }
}
