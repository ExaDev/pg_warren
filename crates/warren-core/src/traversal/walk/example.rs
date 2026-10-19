//! A small knowledge graph and a step constructor for the walk tests.

use super::{HopRange, Step};
use crate::graph::Direction;
use crate::graph::{DenseId, GraphCsr};
use crate::traversal::example::{graph, relations};
use crate::traversal::NodeSet;

pub(crate) const CITES: usize = 0;
pub(crate) const AUTHORED_BY: usize = 1;
pub(crate) const AFFILIATED_WITH: usize = 2;

/// Papers 0–4, people 5–7, institutions 8–9.
/// cites: 0 → 1 → 2 → 3, 0 → 4; authored_by: 1 → 5, 2 → 6, 3 → 7,
/// 4 → 5; affiliated_with: 5 → 8, 6 → 9, 7 → 8.
pub(crate) fn knowledge_graph() -> GraphCsr {
    let edges = [
        (CITES, 0, 1),
        (CITES, 1, 2),
        (CITES, 2, 3),
        (CITES, 0, 4),
        (AUTHORED_BY, 1, 5),
        (AUTHORED_BY, 2, 6),
        (AUTHORED_BY, 3, 7),
        (AUTHORED_BY, 4, 5),
        (AFFILIATED_WITH, 5, 8),
        (AFFILIATED_WITH, 6, 9),
        (AFFILIATED_WITH, 7, 8),
    ];
    let edges: Vec<_> = edges.iter().map(|&(r, s, t)| (r, s, t, 1.0)).collect();
    graph(10, 3, &edges, true)
}

/// An outward step along `relation` (every relation if `None`), whose
/// filter covers the knowledge graph's id space.
pub(crate) fn step(
    relation: Option<usize>,
    min: u32,
    max: u32,
    filter: Option<&[DenseId]>,
) -> Step {
    Step {
        relations: relation.map(|r| vec![relations(3)[r]]),
        direction: Direction::Out,
        filter: filter.map(|f| NodeSet::from_dense_ids(10, f.iter().copied()).unwrap()),
        hops: HopRange::new(min, max).unwrap(),
    }
}
