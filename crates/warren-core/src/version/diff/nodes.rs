//! The node delta of a diff: live nodes added and removed, from the two
//! versions' tombstones and id spaces.

use crate::graph::{DenseId, ExternalId, IdMap};

/// Live nodes before and after, and how many were added and removed. A node
/// whose tombstone was cleared counts as added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeDelta {
    /// Live nodes in the earlier version.
    pub before: u64,
    /// Live nodes in the later version.
    pub after: u64,
    /// Nodes live in the later version but not the earlier.
    pub added: u64,
    /// Nodes live in the earlier version but not the later.
    pub removed: u64,
}

/// Whether `dense` is a live node of `id_map`.
pub(super) fn is_live<E: ExternalId>(id_map: &IdMap<E>, dense: DenseId) -> bool {
    dense < id_map.id_space() && !id_map.tombstones().contains(dense)
}

/// The node delta between two id maps of the same id space, `after`'s
/// covering `before`'s. O(id space).
pub(super) fn node_delta<E: ExternalId>(before: &IdMap<E>, after: &IdMap<E>) -> NodeDelta {
    let mut delta = NodeDelta {
        before: before.node_count(),
        after: after.node_count(),
        ..NodeDelta::default()
    };
    for dense in 0..before.id_space().max(after.id_space()) {
        match (is_live(before, dense), is_live(after, dense)) {
            (false, true) => delta.added += 1,
            (true, false) => delta.removed += 1,
            _ => {}
        }
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_removed_and_resurrected_nodes_are_counted() {
        // 1 → 0, 2 → 1, 3 → 2; v2 drops 2; v3 brings 2 back, drops 3, adds 4.
        let v1 = IdMap::new().rebuild(vec![1i64, 2, 3]).unwrap();
        let v2 = v1.rebuild(vec![1, 3]).unwrap();
        let v3 = v2.rebuild(vec![1, 2, 4]).unwrap();
        assert_eq!(node_delta(&v1, &v2), NodeDelta { before: 3, after: 2, added: 0, removed: 1 });
        assert_eq!(node_delta(&v2, &v3), NodeDelta { before: 2, after: 3, added: 2, removed: 1 });
        assert_eq!(node_delta(&v3, &v3), NodeDelta { before: 3, after: 3, added: 0, removed: 0 });
    }
}
