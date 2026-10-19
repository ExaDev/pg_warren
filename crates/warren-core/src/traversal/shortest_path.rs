//! The shortest path between two nodes by bidirectional BFS (DESIGN.md
//! §9.3, `warren.shortest_path`).
//!
//! One search runs forward from the start, the other backward from the
//! target over the reversed adjacency; each step expands the side with the
//! smaller frontier by one whole level. A node reached by one side that the
//! other has labelled is a meeting node. The meeting node is chosen only
//! after the level is finished, with the smallest combined distance (then
//! the smallest dense id): taking the first meeting node can return a path
//! that is not shortest, a bug pgGraph shipped and fixed. Without a reverse
//! CSR the backward side never expands, and the search is a plain BFS.

use super::scratch::NodeMap;
use super::stats::Progress;
use super::{Adjacency, Interrupts, TraversalError, Traversed};
use crate::graph::{memory, DenseId, GraphError};

/// The shortest path from `start` to `target` along `adjacency`, of at
/// most `max_depth` edges (unbounded if `None`), as the dense ids along it
/// from `start` to `target`; `None` if there is none.
pub fn shortest_path(
    adjacency: &Adjacency<'_>,
    start: DenseId,
    target: DenseId,
    max_depth: Option<u32>,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<Option<Vec<DenseId>>>, TraversalError> {
    for node in [start, target] {
        if node >= adjacency.id_space() {
            return Err(TraversalError::NodeOutsideIdSpace { node });
        }
    }
    let mut progress = Progress::new(interrupts);
    progress.node_visited()?;
    if start == target {
        return Ok(progress.traversed(Some(vec![start])));
    }
    progress.node_visited()?;
    let reversed = adjacency.reversed().ok();
    let id_space = adjacency.id_space();
    let mut forward = Side::new(Some(adjacency), id_space, start)?;
    let mut backward = Side::new(reversed.as_ref(), id_space, target)?;
    loop {
        if max_depth.is_some_and(|max_depth| forward.depth + backward.depth >= max_depth) {
            return Ok(progress.traversed(None));
        }
        let expand_backward =
            backward.adjacency.is_some() && backward.frontier.len() < forward.frontier.len();
        let (side, other) =
            if expand_backward { (&mut backward, &forward) } else { (&mut forward, &backward) };
        if side.frontier.is_empty() {
            return Ok(progress.traversed(None));
        }
        if let Some(meeting) = side.expand(other, &mut progress)? {
            let path = join(&forward, &backward, meeting)?;
            return Ok(progress.traversed(Some(path)));
        }
    }
}

/// How a side reached a node.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Label {
    /// The node it was reached from; a side's own start is its own
    /// predecessor.
    predecessor: DenseId,
    depth: u32,
}

const UNLABELLED: Label = Label { predecessor: DenseId::MAX, depth: u32::MAX };

/// One direction of the bidirectional search.
struct Side<'a, 'g> {
    /// `None` when this side cannot expand.
    adjacency: Option<&'a Adjacency<'g>>,
    labels: NodeMap<Label>,
    frontier: Vec<DenseId>,
    depth: u32,
}

impl<'a, 'g> Side<'a, 'g> {
    fn new(
        adjacency: Option<&'a Adjacency<'g>>,
        id_space: u64,
        origin: DenseId,
    ) -> Result<Self, TraversalError> {
        let mut labels = NodeMap::new(id_space, UNLABELLED);
        labels.insert(origin, Label { predecessor: origin, depth: 0 })?;
        Ok(Self { adjacency, labels, frontier: vec![origin], depth: 0 })
    }

    /// Expands this side by one whole level, then returns the meeting node
    /// with the smallest combined distance, if any.
    fn expand(
        &mut self,
        other: &Side<'_, '_>,
        progress: &mut Progress<'_>,
    ) -> Result<Option<DenseId>, TraversalError> {
        let Some(adjacency) = self.adjacency else { return Ok(None) };
        let depth = self.depth + 1;
        let mut next = Vec::new();
        let mut best: Option<(u32, DenseId)> = None;
        for &via in &self.frontier {
            for node in adjacency.neighbours(via) {
                progress.edge_traversed();
                if self.labels.get(node).is_some() {
                    continue;
                }
                progress.node_visited()?;
                self.labels.insert(node, Label { predecessor: via, depth })?;
                memory::reserve_one(&mut next)?;
                next.push(node);
                if let Some(met) = other.labels.get(node) {
                    let candidate = (depth + met.depth, node);
                    best = Some(best.map_or(candidate, |best| best.min(candidate)));
                }
            }
        }
        self.frontier = next;
        self.depth = depth;
        Ok(best.map(|(_, node)| node))
    }
}

/// The path through `meeting`: the forward trail reversed, then the
/// backward trail after `meeting`.
fn join(
    forward: &Side<'_, '_>,
    backward: &Side<'_, '_>,
    meeting: DenseId,
) -> Result<Vec<DenseId>, TraversalError> {
    let mut path = forward.labels.trail(meeting, |label| label.predecessor)?;
    path.reverse();
    let rest = backward.labels.trail(meeting, |label| label.predecessor)?;
    path.try_reserve(rest.len())
        .map_err(|_| GraphError::OutOfMemory { elements: path.len() + rest.len() })?;
    path.extend_from_slice(&rest[1..]);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphCsr;
    use crate::traversal::example::{directed, graph, relations};
    use crate::traversal::Uninterrupted;
    use crate::traversal::{Direction, RelationSelection};

    // 0 → 1 → 2 → 3 → 4, 0 → 5 → 4, 6 isolated
    fn example() -> GraphCsr {
        directed(7, &[(0, 1), (1, 2), (2, 3), (3, 4), (0, 5), (5, 4), (2, 0)])
    }

    fn path(
        graph: &GraphCsr,
        direction: Direction,
        start: DenseId,
        target: DenseId,
        max_depth: Option<u32>,
    ) -> Option<Vec<DenseId>> {
        let adjacency = Adjacency::new(graph, direction, RelationSelection::All).unwrap();
        shortest_path(&adjacency, start, target, max_depth, &mut Uninterrupted).unwrap().result
    }

    #[test]
    fn the_shortest_path_runs_from_start_to_target() {
        let graph = example();
        assert_eq!(path(&graph, Direction::Out, 0, 4, None), Some(vec![0, 5, 4]));
        assert_eq!(path(&graph, Direction::Out, 1, 4, None), Some(vec![1, 2, 3, 4]));
        assert_eq!(path(&graph, Direction::In, 4, 1, None), Some(vec![4, 3, 2, 1]));
        assert_eq!(path(&graph, Direction::Both, 6, 6, None), Some(vec![6]));
    }

    #[test]
    fn no_path_is_none() {
        let graph = example();
        assert_eq!(path(&graph, Direction::Out, 4, 0, None), None);
        assert_eq!(path(&graph, Direction::Both, 0, 6, None), None);
    }

    #[test]
    fn max_depth_bounds_the_path_length_in_edges() {
        let graph = example();
        assert_eq!(path(&graph, Direction::Out, 1, 4, Some(2)), None);
        assert_eq!(path(&graph, Direction::Out, 1, 4, Some(3)), Some(vec![1, 2, 3, 4]));
        assert_eq!(path(&graph, Direction::Out, 1, 1, Some(0)), Some(vec![1]));
        assert_eq!(path(&graph, Direction::Out, 0, 1, Some(0)), None);
    }

    #[test]
    fn without_a_reverse_csr_the_search_runs_forward_only() {
        let graph = graph(4, 1, &[(0, 0, 1, 1.0), (0, 1, 2, 1.0), (0, 0, 3, 1.0)], false);
        assert_eq!(path(&graph, Direction::Out, 0, 2, None), Some(vec![0, 1, 2]));
        assert_eq!(path(&graph, Direction::Out, 2, 0, None), None);
    }

    #[test]
    fn the_meeting_node_is_chosen_after_the_whole_level_is_scanned() {
        // 0 → 3 → 1 and 0 → 2 → 1, through relations scanned in the order
        // 3 then 2: the level meets at both, and the smaller id wins.
        let graph =
            graph(4, 2, &[(0, 0, 3, 1.0), (0, 3, 1, 1.0), (1, 0, 2, 1.0), (1, 2, 1, 1.0)], true);
        let r = relations(2);
        let adjacency =
            Adjacency::new(&graph, Direction::Out, RelationSelection::Only(&r)).unwrap();
        let found = shortest_path(&adjacency, 0, 1, None, &mut Uninterrupted).unwrap();
        assert_eq!(found.result, Some(vec![0, 2, 1]));
    }

    #[test]
    fn endpoints_outside_the_id_space_are_errors() {
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        assert_eq!(
            shortest_path(&adjacency, 0, 7, None, &mut Uninterrupted).unwrap_err(),
            TraversalError::NodeOutsideIdSpace { node: 7 }
        );
    }
}

#[cfg(test)]
mod properties {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::traversal::reference::{directions, scenarios, selections, Scenario};
    use crate::traversal::Uninterrupted;

    proptest! {
        #[test]
        fn bidirectional_search_matches_single_direction_bfs(
            (scenario, selection, start, target) in scenarios().prop_flat_map(|scenario| {
                let n = scenario.id_space;
                let relation_count = scenario.relation_count;
                (Just(scenario), selections(relation_count), 0..n, 0..n)
            }),
            direction in directions(),
            max_depth in prop::option::of(0u32..5),
        ) {
            let graph = scenario.graph();
            let indexes = scenario.indexes(&selection);
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let found = shortest_path(&adjacency, start, target, max_depth, &mut Uninterrupted)
                .unwrap()
                .result;

            let steps = scenario.steps(direction, &selection);
            let distance = Scenario::depths(&steps, &[start], None)
                .get(&target)
                .copied()
                .filter(|&d| max_depth.is_none_or(|max| d <= max));
            prop_assert_eq!(found.as_ref().map(|path| path.len() as u32 - 1), distance);
            if let Some(path) = found {
                let edges: BTreeSet<(DenseId, DenseId)> = steps.iter().map(|&(s, t, _)| (s, t)).collect();
                prop_assert_eq!(path.first(), Some(&start));
                prop_assert_eq!(path.last(), Some(&target));
                for hop in path.windows(2) {
                    prop_assert!(edges.contains(&(hop[0], hop[1])), "no edge {:?}", hop);
                }
            }
        }
    }
}
