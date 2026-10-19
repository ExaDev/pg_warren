//! Reachability: whether one node reaches another, and every node a set of
//! starts reaches (DESIGN.md §9.3, `warren.reachable`,
//! `warren.reachable_set`).

use super::level::{Admission, LevelSearch};
use super::stats::Progress;
use super::{Adjacency, Cancellation, TraversalError, Traversed, Visit};
use crate::graph::{memory, DenseId};

/// Whether `target` is within `max_depth` edges of `start` along
/// `adjacency` (unbounded if `None`). A node reaches itself. The search
/// stops as soon as it reaches `target`.
pub fn reachable(
    adjacency: &Adjacency<'_>,
    start: DenseId,
    target: DenseId,
    max_depth: Option<u32>,
    cancellation: &Cancellation,
) -> Result<Traversed<bool>, TraversalError> {
    if target >= adjacency.id_space() {
        return Err(TraversalError::NodeOutsideIdSpace { node: target });
    }
    let mut found = false;
    let mut admit = |visit: Visit| {
        found = visit.node == target;
        Ok(if found { Admission::AdmitAndStop } else { Admission::Admit })
    };
    search(adjacency, &[start], max_depth, cancellation, &mut admit)
        .map(|progress| progress.traversed(found))
}

/// Every node within `max_depth` edges of `starts` along `adjacency`
/// (unbounded if `None`), starts included, once each, in BFS order.
pub fn reachable_set(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    max_depth: Option<u32>,
    cancellation: &Cancellation,
) -> Result<Traversed<Vec<DenseId>>, TraversalError> {
    let mut nodes = Vec::new();
    let mut admit = |visit: Visit| {
        memory::reserve_one(&mut nodes)?;
        nodes.push(visit.node);
        Ok(Admission::Admit)
    };
    search(adjacency, starts, max_depth, cancellation, &mut admit)
        .map(|progress| progress.traversed(nodes))
}

/// Runs an unfiltered level search from `starts` to `max_depth`.
fn search<'c>(
    adjacency: &Adjacency<'_>,
    starts: &[DenseId],
    max_depth: Option<u32>,
    cancellation: &'c Cancellation,
    admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
) -> Result<Progress<'c>, TraversalError> {
    let mut progress = Progress::new(cancellation);
    LevelSearch::new(adjacency, None)?.run(starts, max_depth, &mut progress, admit)?;
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphCsr;
    use crate::traversal::example::directed;
    use crate::traversal::{Direction, RelationSelection};

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
        let never = Cancellation::new();
        assert!(reachable(&out, 0, 3, None, &never).unwrap().result);
        assert!(reachable(&out, 0, 3, Some(3), &never).unwrap().result);
        assert!(!reachable(&out, 0, 3, Some(2), &never).unwrap().result);
        assert!(!reachable(&out, 3, 0, None, &never).unwrap().result);
        assert!(!reachable(&out, 0, 4, None, &never).unwrap().result);
        assert!(reachable(&out, 5, 5, Some(0), &never).unwrap().result);
    }

    #[test]
    fn reachable_stops_once_the_target_is_reached() {
        let graph = directed(4, &[(0, 1), (0, 2), (2, 3)]);
        let found = reachable(&out(&graph), 0, 1, None, &Cancellation::new()).unwrap();
        assert_eq!(found.stats.nodes_visited, 2);
        assert!(!found.stats.truncated);
    }

    #[test]
    fn reachable_rejects_a_target_outside_the_id_space() {
        let graph = example();
        assert_eq!(
            reachable(&out(&graph), 0, 6, None, &Cancellation::new()).unwrap_err(),
            TraversalError::NodeOutsideIdSpace { node: 6 }
        );
    }

    #[test]
    fn reachable_set_holds_the_starts_and_everything_they_reach() {
        let graph = example();
        let out = out(&graph);
        let never = Cancellation::new();
        assert_eq!(reachable_set(&out, &[1, 4], None, &never).unwrap().result, [1, 4, 2, 3]);
        assert_eq!(reachable_set(&out, &[0], Some(1), &never).unwrap().result, [0, 1]);
        assert_eq!(reachable_set(&out, &[], None, &never).unwrap().result, [] as [DenseId; 0]);
    }
}
