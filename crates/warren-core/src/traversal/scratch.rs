//! Per-call scratch over the id space: a visited set that starts sparse and
//! turns dense once it holds more than about `id_space/16` nodes, so a small
//! traversal on a big graph does not allocate O(N) (DESIGN.md §9.3).

use std::collections::HashSet;

use super::{NodeSet, TraversalError};
use crate::graph::{DenseId, GraphError};

/// The number of entries above which sparse scratch turns dense.
fn dense_threshold(id_space: u64) -> usize {
    (id_space / 16) as usize
}

fn out_of_memory(elements: usize) -> TraversalError {
    GraphError::OutOfMemory { elements }.into()
}

/// The nodes a traversal has visited.
pub(crate) enum Visited {
    Sparse { id_space: u64, nodes: HashSet<DenseId> },
    Dense(NodeSet),
}

impl Visited {
    /// An empty visited set over `id_space`; allocates nothing yet.
    pub(crate) fn new(id_space: u64) -> Self {
        Self::Sparse { id_space, nodes: HashSet::new() }
    }

    pub(crate) fn contains(&self, node: DenseId) -> bool {
        match self {
            Self::Sparse { nodes, .. } => nodes.contains(&node),
            Self::Dense(nodes) => nodes.contains(node),
        }
    }

    /// Adds `node`, returning whether it was new.
    pub(crate) fn insert(&mut self, node: DenseId) -> Result<bool, TraversalError> {
        match self {
            Self::Dense(nodes) => Ok(nodes.insert(node)),
            Self::Sparse { id_space, nodes } => {
                if nodes.contains(&node) {
                    return Ok(false);
                }
                if nodes.len() >= dense_threshold(*id_space) {
                    let mut dense = NodeSet::from_dense_ids(*id_space, nodes.iter().copied())?;
                    dense.insert(node);
                    *self = Self::Dense(dense);
                    return Ok(true);
                }
                nodes.try_reserve(1).map_err(|_| out_of_memory(nodes.len() + 1))?;
                Ok(nodes.insert(node))
            }
        }
    }

    #[cfg(test)]
    fn is_dense(&self) -> bool {
        matches!(self, Self::Dense(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visited_set_keeps_its_members_when_it_turns_dense() {
        let mut visited = Visited::new(64);
        for id in [5, 9, 5, 63, 0, 17, 33, 9] {
            visited.insert(id).unwrap();
        }
        assert!(visited.is_dense());
        for id in 0..64 {
            assert_eq!(visited.contains(id), [0, 5, 9, 17, 33, 63].contains(&id), "{id}");
        }
        assert!(!visited.insert(17).unwrap());
        assert!(visited.insert(18).unwrap());
    }

    #[test]
    fn a_small_visit_on_a_big_id_space_stays_sparse() {
        let mut visited = Visited::new(1 << 40);
        assert!(visited.insert(1 << 39).unwrap());
        assert!(visited.contains(1 << 39));
        assert!(!visited.is_dense());
    }
}
