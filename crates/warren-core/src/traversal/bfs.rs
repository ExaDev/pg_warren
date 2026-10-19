//! Breadth-first search from a set of starts, reporting each node reached
//! with its depth and the node it was reached from (DESIGN.md §9.3,
//! `warren.bfs`).

use super::level::{Admission, LevelSearch};
use super::stats::Progress;
use super::{Adjacency, Cancellation, Limits, NodeSet, TraversalError, Traversed};
use crate::graph::{memory, DenseId};

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
/// exceed `limits.max_results` is.
pub fn bfs(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    filter: Option<&NodeSet>,
    limits: Limits,
    cancellation: &Cancellation,
) -> Result<Traversed<Vec<Visit>>, TraversalError> {
    let mut progress = Progress::new(cancellation);
    let mut search = LevelSearch::new(adjacency, filter)?;
    let mut visits = Vec::new();
    let mut truncated = false;
    let mut admit = |visit: Visit| -> Result<Admission, TraversalError> {
        if limits.max_results == Some(visits.len() as u64) {
            truncated = true;
            return Ok(Admission::Stop);
        }
        memory::reserve_one(&mut visits)?;
        visits.push(visit);
        Ok(Admission::Admit)
    };
    search.run(starts, limits.max_depth, &mut progress, &mut admit)?;
    if truncated {
        progress.truncate();
    }
    Ok(progress.traversed(visits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphCsr;
    use crate::traversal::example::directed;
    use crate::traversal::{Direction, RelationSelection, TraversalStats};

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
        bfs(&adjacency, starts, filter, limits, &Cancellation::new())
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
    fn a_raised_cancellation_stops_the_search() {
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let cancellation = Cancellation::new();
        cancellation.cancel();
        assert_eq!(
            bfs(&adjacency, &[0], None, Limits::default(), &cancellation).unwrap_err(),
            TraversalError::Cancelled
        );
    }
}
