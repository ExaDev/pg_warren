//! The cheapest path between two nodes by Dijkstra's algorithm (DESIGN.md
//! §9.3, `warren.dijkstra`).
//!
//! Edge weights come from each selected relation's own CSR (1 when the
//! graph is unweighted), so an edge present in several relations costs its
//! cheapest weight. Weights are checked as edges are scanned: a negative or
//! NaN weight stops the search with [`TraversalError::InvalidWeight`]. An
//! infinite weight is allowed and never improves a path.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use super::scratch::{NodeMap, Visited};
use super::stats::Progress;
use super::{Adjacency, Interrupts, TraversalError, Traversed};
use crate::graph::{DenseId, GraphError};

/// A path with its total weight.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedPath {
    /// The dense ids along the path, from start to target.
    pub path: Vec<DenseId>,
    /// The sum of the weights of its edges.
    pub cost: f64,
}

/// The cheapest path from `start` to `target` along `adjacency`; `None` if
/// there is none. Ties go to the path settled first, preferring smaller
/// dense ids.
pub fn dijkstra(
    adjacency: &Adjacency<'_>,
    start: DenseId,
    target: DenseId,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<Option<WeightedPath>>, TraversalError> {
    for node in [start, target] {
        if node >= adjacency.id_space() {
            return Err(TraversalError::NodeOutsideIdSpace { node });
        }
    }
    let mut progress = Progress::new(interrupts);
    let mut labels = NodeMap::new(adjacency.id_space(), UNREACHED);
    let mut settled = Visited::new(adjacency.id_space());
    let mut queue = BinaryHeap::new();
    labels.insert(start, Label { cost: 0.0, predecessor: start })?;
    queue.push(Reverse(Queued { cost: 0.0, node: start }));
    while let Some(Reverse(Queued { cost, node })) = queue.pop() {
        if !settled.insert(node)? {
            continue;
        }
        progress.node_visited()?;
        if node == target {
            let mut path = labels.trail(target, |label| label.predecessor)?;
            path.reverse();
            return Ok(progress.traversed(Some(WeightedPath { path, cost })));
        }
        for (next, weight) in adjacency.weighted_neighbours(node) {
            progress.edge_traversed();
            if weight.is_nan() || weight < 0.0 {
                return Err(TraversalError::InvalidWeight { from: node, to: next, weight });
            }
            let cost = cost + weight;
            if settled.contains(next) || labels.get(next).is_some_and(|label| label.cost <= cost) {
                continue;
            }
            labels.insert(next, Label { cost, predecessor: node })?;
            queue
                .try_reserve(1)
                .map_err(|_| GraphError::OutOfMemory { elements: queue.len() + 1 })?;
            queue.push(Reverse(Queued { cost, node: next }));
        }
    }
    Ok(progress.traversed(None))
}

/// The cheapest known way to reach a node.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Label {
    cost: f64,
    /// The node it is reached from; the start is its own predecessor.
    predecessor: DenseId,
}

const UNREACHED: Label = Label { cost: f64::INFINITY, predecessor: DenseId::MAX };

/// A node waiting in the queue, ordered by cost, then dense id.
#[derive(Debug, Clone, Copy)]
struct Queued {
    cost: f64,
    node: DenseId,
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cost.total_cmp(&other.cost).then(self.node.cmp(&other.node))
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Queued {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphCsr;
    use crate::traversal::example::{directed, graph};
    use crate::traversal::{Cancellation, Uninterrupted};
    use crate::traversal::{Direction, RelationSelection};

    fn cheapest(
        graph: &GraphCsr,
        direction: Direction,
        start: DenseId,
        target: DenseId,
    ) -> Result<Option<WeightedPath>, TraversalError> {
        let adjacency = Adjacency::new(graph, direction, RelationSelection::All)?;
        Ok(dijkstra(&adjacency, start, target, &mut Uninterrupted)?.result)
    }

    fn weighted_path(path: &[DenseId], cost: f64) -> Option<WeightedPath> {
        Some(WeightedPath { path: path.to_vec(), cost })
    }

    // 0 →(4) 1, 0 →(1) 2, 2 →(2) 1, 1 →(1) 3, 4 isolated
    fn example() -> GraphCsr {
        graph(5, 1, &[(0, 0, 1, 4.0), (0, 0, 2, 1.0), (0, 2, 1, 2.0), (0, 1, 3, 1.0)], true)
    }

    #[test]
    fn dijkstra_finds_the_cheapest_path_and_its_cost() {
        let graph = example();
        assert_eq!(cheapest(&graph, Direction::Out, 0, 3), Ok(weighted_path(&[0, 2, 1, 3], 4.0)));
        assert_eq!(cheapest(&graph, Direction::In, 3, 0), Ok(weighted_path(&[3, 1, 2, 0], 4.0)));
        assert_eq!(cheapest(&graph, Direction::Out, 4, 4), Ok(weighted_path(&[4], 0.0)));
        assert_eq!(cheapest(&graph, Direction::Out, 3, 0), Ok(None));
    }

    #[test]
    fn an_unweighted_graph_costs_one_per_edge() {
        let graph = directed(4, &[(0, 1), (1, 2), (0, 3), (3, 2)]);
        assert_eq!(cheapest(&graph, Direction::Out, 0, 2), Ok(weighted_path(&[0, 1, 2], 2.0)));
    }

    #[test]
    fn an_edge_in_several_relations_costs_its_cheapest_weight() {
        let graph = graph(2, 2, &[(0, 0, 1, 2.0), (1, 0, 1, 3.0)], true);
        assert_eq!(cheapest(&graph, Direction::Out, 0, 1), Ok(weighted_path(&[0, 1], 2.0)));
    }

    #[test]
    fn zero_weights_are_allowed() {
        let graph = graph(3, 1, &[(0, 0, 1, 0.0), (0, 1, 2, 0.0), (0, 0, 2, 5.0)], true);
        assert_eq!(cheapest(&graph, Direction::Out, 0, 2), Ok(weighted_path(&[0, 1, 2], 0.0)));
    }

    #[test]
    fn negative_or_nan_weights_reached_are_errors() {
        for weight in [-1.0, f64::NAN] {
            let graph = graph(3, 1, &[(0, 0, 1, 1.0), (0, 1, 2, weight)], true);
            match cheapest(&graph, Direction::Out, 0, 2) {
                Err(TraversalError::InvalidWeight { from: 1, to: 2, weight: found }) => {
                    assert!(found.is_nan() || found == weight)
                }
                other => panic!("expected InvalidWeight, got {other:?}"),
            }
        }
    }

    #[test]
    fn endpoints_outside_the_id_space_are_errors() {
        assert_eq!(
            cheapest(&example(), Direction::Out, 5, 0),
            Err(TraversalError::NodeOutsideIdSpace { node: 5 })
        );
    }

    #[test]
    fn a_raised_cancellation_stops_the_search() {
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let mut cancellation = Cancellation::new();
        cancellation.cancel();
        assert_eq!(dijkstra(&adjacency, 0, 3, &mut cancellation), Err(TraversalError::Cancelled));
    }
}

#[cfg(test)]
mod properties {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::traversal::reference::{directions, scenarios, selections, Scenario};
    use crate::traversal::Uninterrupted;

    /// The cheapest cost of every edge, by `(from, to)`.
    fn cheapest_edges(steps: &[(DenseId, DenseId, f64)]) -> BTreeMap<(DenseId, DenseId), f64> {
        let mut cheapest = BTreeMap::new();
        for &(from, to, weight) in steps {
            let entry = cheapest.entry((from, to)).or_insert(f64::INFINITY);
            *entry = f64::min(*entry, weight);
        }
        cheapest
    }

    /// Bellman–Ford distances from `start`.
    fn distances(id_space: u64, steps: &[(DenseId, DenseId, f64)], start: DenseId) -> Vec<f64> {
        let mut distance = vec![f64::INFINITY; id_space as usize];
        distance[start as usize] = 0.0;
        for _ in 0..id_space {
            for &(from, to, weight) in steps {
                let through = distance[from as usize] + weight;
                if through < distance[to as usize] {
                    distance[to as usize] = through;
                }
            }
        }
        distance
    }

    proptest! {
        #[test]
        fn dijkstra_matches_bellman_ford(
            (scenario, selection, start, target) in scenarios().prop_flat_map(|scenario| {
                let n = scenario.id_space;
                let relation_count = scenario.relation_count;
                (Just(scenario), selections(relation_count), 0..n, 0..n)
            }),
            direction in directions(),
        ) {
            let graph = scenario.graph();
            let indexes = scenario.indexes(&selection);
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let found = dijkstra(&adjacency, start, target, &mut Uninterrupted).unwrap().result;

            let steps = scenario.steps(direction, &selection);
            let expected = distances(scenario.id_space, &steps, start)[target as usize];
            match found {
                None => prop_assert!(expected.is_infinite()),
                Some(WeightedPath { path, cost }) => {
                    prop_assert_eq!(cost, expected);
                    prop_assert_eq!(path.first(), Some(&start));
                    prop_assert_eq!(path.last(), Some(&target));
                    let cheapest = cheapest_edges(&steps);
                    let mut walked = 0.0;
                    for hop in path.windows(2) {
                        let weight = cheapest.get(&(hop[0], hop[1]));
                        prop_assert!(weight.is_some(), "no edge {:?}", hop);
                        walked += weight.copied().unwrap_or_default();
                    }
                    prop_assert_eq!(walked, cost);
                }
            }
        }
    }
}
