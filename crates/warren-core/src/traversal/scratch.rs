//! Per-call scratch over the id space: a visited set and a node map that
//! start sparse and turn dense once they hold more than about `id_space/16`
//! nodes, so a small traversal on a big graph does not allocate O(N)
//! (DESIGN.md §9.3).

use std::collections::{HashMap, HashSet};

use super::{NodeSet, TraversalError};
use crate::graph::{memory, DenseId, GraphError};

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

/// A value per node, for the nodes a traversal has labelled.
pub(crate) struct NodeMap<V> {
    id_space: u64,
    /// The dense representation's value for an unlabelled node.
    absent: V,
    store: Store<V>,
}

enum Store<V> {
    Sparse(HashMap<DenseId, V>),
    Dense(Vec<V>),
}

impl<V: Copy + PartialEq> NodeMap<V> {
    /// An empty map over `id_space`; `absent` must never be stored as a
    /// value. Allocates nothing yet.
    pub(crate) fn new(id_space: u64, absent: V) -> Self {
        Self { id_space, absent, store: Store::Sparse(HashMap::new()) }
    }

    pub(crate) fn get(&self, node: DenseId) -> Option<V> {
        match &self.store {
            Store::Sparse(values) => values.get(&node).copied(),
            Store::Dense(values) => {
                values.get(node as usize).copied().filter(|v| *v != self.absent)
            }
        }
    }

    /// Sets `node`'s value. `node` must be inside the id space.
    pub(crate) fn insert(&mut self, node: DenseId, value: V) -> Result<(), TraversalError> {
        if let Store::Sparse(values) = &mut self.store {
            if values.len() < dense_threshold(self.id_space) || values.contains_key(&node) {
                values.try_reserve(1).map_err(|_| out_of_memory(values.len() + 1))?;
                values.insert(node, value);
                return Ok(());
            }
            let mut dense = memory::filled(self.id_space as usize, self.absent)?;
            for (&labelled, &value) in values.iter() {
                dense[labelled as usize] = value;
            }
            self.store = Store::Dense(dense);
        }
        if let Store::Dense(values) = &mut self.store {
            if let Some(slot) = values.get_mut(node as usize) {
                *slot = value;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn is_dense(&self) -> bool {
        matches!(self.store, Store::Dense(_))
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

    #[test]
    fn a_node_map_keeps_its_entries_when_it_turns_dense() {
        let mut map = NodeMap::new(64, u64::MAX);
        for id in [5, 9, 63, 0, 17, 33] {
            map.insert(id, id * 10).unwrap();
        }
        map.insert(9, 1).unwrap();
        assert!(map.is_dense());
        assert_eq!(map.get(9), Some(1));
        assert_eq!(map.get(63), Some(630));
        assert_eq!(map.get(1), None);
        assert_eq!(map.get(64), None);
    }
}
