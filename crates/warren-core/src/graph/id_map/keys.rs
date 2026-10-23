//! The key storage of an id map (DESIGN.md §5.7): how a version holds its
//! external ids, apart from the dense-id logic every id type shares.

use crate::graph::{BuildCharge, BuildMemory, DenseId, GraphError, Permutation};

/// How an id map stores its external ids: both directions between an
/// external id ([`Self::Key`]) and its dense id, over one id space,
/// tombstoned nodes included. [`IdMap`](super::IdMap) adds the tombstones.
pub trait IdKeys: Default {
    /// An external id as lookups take and return it.
    type Key: ?Sized + Ord;
    /// The node ids a rebuild takes.
    type Ids: NodeIds<Key = Self::Key>;
    /// The keys of the next build, as a rebuild fills them.
    type Rebuild: KeysRebuild<Self>;

    /// The high-water mark of dense ids, tombstones included.
    fn id_space(&self) -> u64;

    /// The heap bytes the keys hold.
    fn bytes(&self) -> u64;

    /// The dense id assigned to `key`, tombstoned or not.
    fn dense_id(&self, key: &Self::Key) -> Option<DenseId>;

    /// The key assigned to `dense`, tombstoned or not.
    fn key(&self, dense: DenseId) -> Option<&Self::Key>;

    /// Every (key, dense id), in key order.
    fn in_order(&self) -> impl Iterator<Item = (&Self::Key, DenseId)>;

    /// Moves every key to its dense id under `permutation`, which permutes
    /// this id space; on an error, nothing has changed.
    fn renumber(&mut self, permutation: &Permutation) -> Result<(), GraphError>;

    /// The keys at `old_dense_ids`, which are in key order, given dense ids
    /// `0..old_dense_ids.len()` in that order.
    fn compacted(&self, old_dense_ids: &[DenseId]) -> Result<Self, GraphError>;

    /// Starts the keys of a rebuild that adds `fresh` keys to these,
    /// reserving exactly what they will hold, charged to `memory`.
    fn rebuild(&self, fresh: usize, memory: &BuildMemory) -> Result<Self::Rebuild, GraphError>;
}

/// The node ids a rebuild takes: external ids in any order, duplicates
/// allowed, charged to a build's memory.
pub trait NodeIds {
    /// An external id.
    type Key: ?Sized + Ord;

    /// The memory the ids are charged to.
    fn memory(&self) -> &BuildMemory;

    /// Sorts the ids and removes duplicates, in place.
    fn sort_distinct(&mut self);

    /// The ids, in their current order.
    fn keys(&self) -> impl Iterator<Item = &Self::Key>;
}

/// The keys of a rebuild, filled in key order with every key of the next
/// build: the base's, then fresh ones, interleaved by key.
pub trait KeysRebuild<K: IdKeys> {
    /// Takes a key of the base keys, at its dense id.
    fn keep(&mut self, key: &K::Key, dense: DenseId) -> Result<(), GraphError>;

    /// Takes a fresh key, at its new dense id.
    fn add(&mut self, key: &K::Key, dense: DenseId) -> Result<(), GraphError>;

    /// The rebuilt keys, from the base keys `base` and the keys taken, and
    /// the charge for what they hold.
    fn finish(self, base: &K) -> Result<(K, BuildCharge), GraphError>;
}
