//! The id map: stable dense ids for external ids across versions of a graph
//! (DESIGN.md §5.2), generic over how it stores its external ids (§5.7).

mod fixed;
mod keys;
mod text;

use std::cmp::Ordering;

pub use fixed::{ExternalId, FixedKeys, FixedRebuild, IdPair};
pub use keys::{FreshKeys, IdKeys, KeysRebuild, NodeIds};
pub use text::{IdOrder, TextIds, TextIndex, TextKeys, TextRebuild};

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
    pub fn rebuild_within(&self, ids: K::Ids) -> Result<(Self, BuildCharge), GraphError> {
        let merged = self.merged(ids)?;
        let (keys, charge) = merged.keys.finish(&self.keys)?;
        merged.tombstones.into_map(keys, charge)
    }

    /// [`Self::rebuild_within`], consuming this id map, whose buffers it
    /// may extend in place rather than copy (a text id map's bytes and
    /// offsets, DESIGN.md §5.7). `charge` should be this id map's charge on
    /// the budget `ids` is charged to: it is held until the new id map is
    /// built, then taken over. Whatever it was, the returned charge is on
    /// `ids`' budget and is exactly the bytes the new id map holds, charged
    /// in full if it was on another budget or short (an error if that does
    /// not fit). On an error, this id map and `charge` are dropped.
    pub fn rebuild_into(
        self,
        charge: BuildCharge,
        ids: K::Ids,
    ) -> Result<(Self, BuildCharge), GraphError> {
        let memory = ids.memory().clone();
        let merged = self.merged(ids)?;
        let Self { keys: base, tombstones } = self;
        drop(tombstones);
        let (keys, keys_charge) = merged.keys.finish_into(base)?;
        let (id_map, rebuilt) = merged.tombstones.into_map(keys, keys_charge)?;
        let mut charge = match charge.memory().is(&memory) {
            true => charge,
            false => BuildCharge::none(&memory),
        };
        charge.absorb(rebuilt);
        match charge.bytes().checked_sub(id_map.bytes()) {
            Some(spare) => charge.shrink(spare),
            None => charge.grow(id_map.bytes() - charge.bytes())?,
        }
        Ok((id_map, charge))
    }

    /// The merge walk of a rebuild over `ids`, before its keys are
    /// finished; `ids` are freed when it returns.
    fn merged(&self, mut ids: K::Ids) -> Result<Merged<K>, GraphError> {
        let memory = ids.memory().clone();
        ids.sort_distinct();
        let fresh = self.fresh(&ids);
        let mut keys = self.keys.rebuild(fresh, &memory)?;
        let words = Tombstones::bytes_for(self.id_space() + fresh.count as u64);
        let charge = memory.charge(words)?;
        let mut tombstones = self.tombstones.try_clone()?;
        let id_space = self.merge(&ids, &mut keys, &mut tombstones)?;
        Ok(Merged { keys, tombstones: RebuiltTombstones { tombstones, id_space, charge } })
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

    /// The sorted, distinct `ids` this map does not hold.
    fn fresh(&self, ids: &K::Ids) -> FreshKeys {
        let mut known = self.keys.in_order().map(|(key, _)| key).peekable();
        ids.keys()
            .filter(|&external| {
                while known.next_if(|&known| known < external).is_some() {}
                known.peek() != Some(&external)
            })
            .fold(FreshKeys::default(), |fresh, key| FreshKeys {
                count: fresh.count + 1,
                bytes: fresh.bytes + size_of_val(key) as u64,
            })
    }

    /// Renumbers every dense id, tombstones included, by `permutation`,
    /// which must permute this id map's id space; else changes nothing.
    /// What it builds beside what it replaces is charged to `memory` while
    /// both are held: the renumbered tombstones, if there are any, and for
    /// text keys new bytes, offsets and order, about N·L + 12·N bytes for N
    /// ids of mean length L. Fixed keys are renumbered in place; a shared
    /// fixed id map's copy is not charged: only a first build renumbers,
    /// and its id map is one it built and owns.
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
        self.keys.renumber(permutation, memory)?;
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

/// The keys of a rebuild after its merge walk, with its tombstones.
struct Merged<K: IdKeys> {
    keys: K::Rebuild,
    tombstones: RebuiltTombstones,
}

/// The tombstones of a rebuild, to grow to its id space, and their charge.
struct RebuiltTombstones {
    tombstones: Tombstones,
    id_space: u64,
    charge: BuildCharge,
}

impl RebuiltTombstones {
    /// The rebuilt id map of `keys`, whose charge is `charge`.
    fn into_map<K: IdKeys>(
        self,
        keys: K,
        mut charge: BuildCharge,
    ) -> Result<(IdMap<K>, BuildCharge), GraphError> {
        let Self { mut tombstones, id_space, charge: tombstones_charge } = self;
        tombstones.grow_to(id_space)?;
        charge.absorb(tombstones_charge);
        Ok((IdMap { keys, tombstones }, charge))
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
