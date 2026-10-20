//! A set of dense ids as a bitmap over the id space: a node filter, or the
//! visited set of a traversal (DESIGN.md §9.2, §9.3).

use super::TraversalError;
use crate::graph::{memory, DenseId};

const WORD_BITS: u64 = u64::BITS as u64;

/// One bit per dense id of an id space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSet {
    id_space: u64,
    words: Vec<u64>,
    len: u64,
}

impl NodeSet {
    /// An empty set over `id_space` dense ids.
    pub fn new(id_space: u64) -> Result<Self, TraversalError> {
        let words = memory::filled(id_space.div_ceil(WORD_BITS) as usize, 0)?;
        Ok(Self { id_space, words, len: 0 })
    }

    /// The set of `ids` over `id_space`; ids outside the id space are
    /// dropped, as an external id missing from the id map would be.
    pub fn from_dense_ids(
        id_space: u64,
        ids: impl IntoIterator<Item = DenseId>,
    ) -> Result<Self, TraversalError> {
        let mut set = Self::new(id_space)?;
        for id in ids {
            set.insert(id);
        }
        Ok(set)
    }

    /// The number of dense ids the set covers.
    pub fn id_space(&self) -> u64 {
        self.id_space
    }

    /// The number of members.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the set has no members.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `id` is a member. Ids outside the id space are not.
    pub fn contains(&self, id: DenseId) -> bool {
        id < self.id_space && self.words[(id / WORD_BITS) as usize] & bit(id) != 0
    }

    /// Adds `id`, returning whether it was new. Ids outside the id space
    /// are ignored.
    pub fn insert(&mut self, id: DenseId) -> bool {
        if id >= self.id_space {
            return false;
        }
        let word = &mut self.words[(id / WORD_BITS) as usize];
        let new = *word & bit(id) == 0;
        *word |= bit(id);
        self.len += u64::from(new);
        new
    }

    /// The number of 64-bit words of the bitmap.
    pub(crate) fn word_count(&self) -> usize {
        self.words.len()
    }

    /// Word `index` of the bitmap: bit `i` is id `64·index + i`. Zero past
    /// the end.
    pub(crate) fn word(&self, index: usize) -> u64 {
        self.words.get(index).copied().unwrap_or(0)
    }
}

fn bit(id: DenseId) -> u64 {
    1 << (id % WORD_BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_set_holds_the_ids_inserted() {
        let mut set = NodeSet::new(130).unwrap();
        assert!(!set.contains(64));
        assert!(set.insert(64));
        assert!(!set.insert(64));
        assert!(set.contains(64));
        assert!(!set.contains(63));
        assert_eq!(set.id_space(), 130);
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
    }

    #[test]
    fn ids_outside_the_id_space_are_never_members() {
        let set = NodeSet::from_dense_ids(3, [0, 2, 3, u64::MAX]).unwrap();
        assert!(set.contains(0) && set.contains(2));
        assert!(!set.contains(3));
        assert_eq!(set.len(), 2);
    }
}
