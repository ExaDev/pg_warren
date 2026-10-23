//! The id map: stable dense ids for external ids across versions of a graph
//! (DESIGN.md §5.2), generic over how it stores its external ids (§5.7).

mod fixed;
mod keys;

use std::cmp::Ordering;

pub use fixed::{ExternalId, FixedKeys, FixedRebuild, IdPair};
pub use keys::{IdKeys, KeysRebuild, NodeIds};

use super::{BuildCharge, BuildMemory, DenseId, GraphError, Permutation, Tombstones};

/// The id map of one graph version: its keys, which map each external id
/// to its dense id and back, and the tombstones of dense ids whose nodes
/// have disappeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMap<K> {
    keys: K,
    tombstones: Tombstones,
}

impl<K: IdKeys> Default for IdMap<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: IdKeys> IdMap<K> {
    /// The empty id map that precedes a graph's first build.
    pub fn new() -> Self {
        Self { keys: K::default(), tombstones: Tombstones::default() }
    }

    /// The id map of the next build, whose live nodes are `ids` (duplicates
    /// allowed), charging every buffer to the memory `ids` is charged to;
    /// returns the charge for the id map it holds. Nodes in both keep their
    /// dense id; new nodes are appended at the high-water mark in
    /// external-id order; nodes that have disappeared are tombstoned. Dense
    /// ids are never reused, so a node that reappears after being
    /// tombstoned gets its old dense id back. The ids are sorted in place,
    /// the new ids among them counted, and every buffer reserved exactly;
    /// they are freed before the keys are finished.
    pub fn rebuild_within(&self, mut ids: K::Ids) -> Result<(Self, BuildCharge), GraphError> {
        let memory = ids.memory().clone();
        ids.sort_distinct();
        let fresh_count = self.fresh(&ids);
        let mut keys = self.keys.rebuild(fresh_count, &memory)?;
        let words = Tombstones::bytes_for(self.id_space() + fresh_count as u64);
        let tombstones_charge = memory.charge(words)?;
        let mut tombstones = self.tombstones.try_clone()?;
        let next_dense = self.merge(&ids, &mut keys, &mut tombstones)?;
        drop(ids);
        let (keys, mut charge) = keys.finish(&self.keys)?;
        tombstones.grow_to(next_dense)?;
        charge.absorb(tombstones_charge);
        Ok((Self { keys, tombstones }, charge))
    }

    /// Walks these keys and the sorted, distinct `ids` in key order, into
    /// `keys`: a key only in these is tombstoned, a key in both is live, and
    /// a key only in `ids` is fresh, at the next dense id. Returns the new
    /// id space.
    fn merge(
        &self,
        ids: &K::Ids,
        keys: &mut K::Rebuild,
        tombstones: &mut Tombstones,
    ) -> Result<u64, GraphError> {
        let mut next_dense = self.id_space();
        let mut previous = self.keys.in_order().peekable();
        let mut current = ids.keys().peekable();
        loop {
            let order = match (previous.peek(), current.peek()) {
                (None, None) => return Ok(next_dense),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some((old, _)), Some(new)) => (*old).cmp(*new),
            };
            match order {
                Ordering::Less => {
                    if let Some((key, dense)) = previous.next() {
                        tombstones.insert(dense)?;
                        keys.keep(key, dense)?;
                    }
                }
                Ordering::Equal => {
                    if let Some((key, dense)) = previous.next() {
                        tombstones.remove(dense)?;
                        keys.keep(key, dense)?;
                    }
                    current.next();
                }
                Ordering::Greater => {
                    if let Some(new) = current.next() {
                        keys.add(new, next_dense)?;
                        next_dense += 1;
                    }
                }
            }
        }
    }

    /// How many of the sorted, distinct `ids` this map does not hold.
    fn fresh(&self, ids: &K::Ids) -> usize {
        let mut known = self.keys.in_order().map(|(key, _)| key).peekable();
        ids.keys()
            .filter(|&external| {
                while known.next_if(|&known| known < external).is_some() {}
                known.peek() != Some(&external)
            })
            .count()
    }

    /// Renumbers every dense id, tombstones included, by `permutation`,
    /// which must permute this id map's id space; else changes nothing.
    /// The renumbered tombstones, if there are any, are charged to `memory`
    /// while they are built. A shared id map's copy is not charged: only a
    /// first build renumbers, and its id map is one it built and owns.
    pub fn renumber(
        &mut self,
        permutation: &Permutation,
        memory: &BuildMemory,
    ) -> Result<(), GraphError> {
        if permutation.id_space() != self.id_space() {
            return Err(GraphError::IdSpaceMismatch {
                expected: self.id_space(),
                found: permutation.id_space(),
            });
        }
        let id_space = self.id_space();
        let tombstones = if self.tombstones.count() > 0 {
            // Built beside the old bitmap, which it then replaces.
            let _charge = memory.charge(Tombstones::bytes_for(id_space))?;
            let mut tombstones = Tombstones::new(id_space)?;
            for old in (0..id_space).filter(|&old| self.tombstones.contains(old)) {
                tombstones.insert(permutation.new_dense_id(old))?;
            }
            Some(tombstones)
        } else {
            None
        };
        self.keys.renumber(permutation)?;
        if let Some(tombstones) = tombstones {
            self.tombstones = tombstones;
        }
        Ok(())
    }

    /// The id map of the live nodes at `old_dense_ids`, which are in
    /// external-id order, given dense ids `0..old_dense_ids.len()` in that
    /// order and no tombstones.
    pub(crate) fn compacted(&self, old_dense_ids: &[DenseId]) -> Result<Self, GraphError> {
        let keys = self.keys.compacted(old_dense_ids)?;
        Ok(Self { keys, tombstones: Tombstones::new(old_dense_ids.len() as u64)? })
    }

    /// The keys of this id map, tombstoned nodes' included.
    pub fn keys(&self) -> &K {
        &self.keys
    }

    /// The dense id of a live node, by binary search.
    pub fn dense_id(&self, external: &K::Key) -> Option<DenseId> {
        let dense = self.keys.dense_id(external)?;
        (!self.tombstones.contains(dense)).then_some(dense)
    }

    /// The external id that `dense` was assigned to, tombstoned or not.
    pub fn external_id(&self, dense: DenseId) -> Option<&K::Key> {
        self.keys.key(dense)
    }

    /// The high-water mark of dense ids, tombstones included.
    pub fn id_space(&self) -> u64 {
        self.keys.id_space()
    }

    /// The number of live nodes.
    pub fn node_count(&self) -> u64 {
        self.id_space() - self.tombstones.count()
    }

    /// The tombstones of this id map.
    pub fn tombstones(&self) -> &Tombstones {
        &self.tombstones
    }

    /// The heap bytes it holds: both directions of the map and the
    /// tombstones.
    pub fn bytes(&self) -> u64 {
        self.keys.bytes() + self.tombstones.bytes()
    }
}

/// Checks that the tombstones cover an id space of `id_space`.
fn check_tombstones(id_space: u64, tombstones: &Tombstones) -> Result<(), GraphError> {
    if tombstones.id_space() != id_space {
        return Err(GraphError::TombstonesLength {
            expected: id_space,
            found: tombstones.id_space(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
