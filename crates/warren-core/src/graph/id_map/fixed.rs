//! The key storage of fixed-width external ids (`int4`, `int8`, `uuid`):
//! (external id, dense id) pairs sorted by external id and an inverse
//! array from dense id to external id (DESIGN.md §5.2).

use std::fmt::Debug;

use super::{check_tombstones, FreshKeys, IdKeys, IdMap, KeysRebuild, NodeIds};
use crate::graph::{
    memory, BuildBuffer, BuildCharge, BuildMemory, DenseId, GraphError, Permutation, SegmentBuffer,
    Tombstones,
};

/// An external id type: the type of a graph's node ids in its user tables
/// (`id_type`). Dense ids follow this type's order, which must match the
/// Postgres sort order of the column type.
pub trait ExternalId: Copy + Ord + Debug {
    /// The encoded width in bytes.
    const WIDTH: usize;

    /// Appends the segment encoding of `self` (little-endian for integers).
    fn write_le(&self, out: &mut Vec<u8>);

    /// Decodes an id from exactly [`Self::WIDTH`] bytes.
    fn read_le(bytes: &[u8]) -> Self;
}

macro_rules! integer_external_id {
    ($type:ty, $doc:literal) => {
        #[doc = $doc]
        impl ExternalId for $type {
            const WIDTH: usize = std::mem::size_of::<$type>();

            fn write_le(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn read_le(bytes: &[u8]) -> Self {
                let mut array = [0; std::mem::size_of::<$type>()];
                array.copy_from_slice(bytes);
                <$type>::from_le_bytes(array)
            }
        }
    };
}

integer_external_id!(i32, "`int4`.");
integer_external_id!(i64, "`int8`.");

/// `uuid`, in its 16 bytes: Postgres compares uuids bytewise, as `[u8; 16]`
/// does.
impl ExternalId for [u8; 16] {
    const WIDTH: usize = 16;

    fn write_le(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }

    fn read_le(bytes: &[u8]) -> Self {
        let mut uuid = [0; 16];
        uuid.copy_from_slice(bytes);
        uuid
    }
}

/// An (external id, dense id) pair of the id map, in a fixed layout so that
/// a shared version can hold the pairs in place (DESIGN.md §5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct IdPair<E> {
    /// The node's external id.
    pub external: E,
    /// The dense id assigned to it.
    pub dense: DenseId,
}

/// The keys of a fixed-width id type `E`: (external id, dense id) pairs
/// sorted by external id, and an inverse array from dense id to external
/// id, each owned or a shared view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedKeys<E> {
    by_external: SegmentBuffer<IdPair<E>>,
    by_dense: SegmentBuffer<E>,
}

impl<E> Default for FixedKeys<E> {
    fn default() -> Self {
        Self { by_external: SegmentBuffer::default(), by_dense: SegmentBuffer::default() }
    }
}

impl<E: ExternalId> IdKeys for FixedKeys<E> {
    type Key = E;
    type Ids = BuildBuffer<E>;
    type Rebuild = FixedRebuild<E>;

    fn id_space(&self) -> u64 {
        self.by_dense.len() as u64
    }

    fn bytes(&self) -> u64 {
        self.by_external.bytes() + self.by_dense.bytes()
    }

    fn dense_id(&self, key: &E) -> Option<DenseId> {
        let index = self.by_external.binary_search_by(|pair| pair.external.cmp(key)).ok()?;
        Some(self.by_external[index].dense)
    }

    fn key(&self, dense: DenseId) -> Option<&E> {
        usize::try_from(dense).ok().and_then(|index| self.by_dense.get(index))
    }

    fn in_order(&self) -> impl Iterator<Item = (&E, DenseId)> {
        self.by_external.iter().map(|pair| (&pair.external, pair.dense))
    }

    fn renumber(&mut self, permutation: &Permutation, _: &BuildMemory) -> Result<(), GraphError> {
        // Renumbered in place; a shared id map is copied first, uncharged
        // (see `IdMap::renumber`).
        let by_external = self.by_external.make_mut()?;
        let by_dense = self.by_dense.make_mut()?;
        for pair in by_external {
            pair.dense = permutation.new_dense_id(pair.dense);
            by_dense[pair.dense as usize] = pair.external;
        }
        Ok(())
    }

    fn compacted(&self, old_dense_ids: &[DenseId]) -> Result<Self, GraphError> {
        let mut pairs = memory::with_capacity(old_dense_ids.len())?;
        for (new, &old) in old_dense_ids.iter().enumerate() {
            let external = *self.key(old).ok_or(GraphError::IdMapNotPermutation)?;
            pairs.push(IdPair { external, dense: new as DenseId });
        }
        Self::from_pairs(pairs)
    }

    fn rebuild(
        &self,
        fresh: FreshKeys,
        memory: &BuildMemory,
    ) -> Result<FixedRebuild<E>, GraphError> {
        Ok(FixedRebuild {
            by_external: BuildBuffer::with_capacity(memory, self.by_external.len() + fresh.count)?,
            fresh: BuildBuffer::with_capacity(memory, fresh.count)?,
        })
    }
}

impl<E: ExternalId> FixedKeys<E> {
    /// The keys of `by_external`, checking that the pairs are strictly
    /// sorted by external id and that the dense ids are exactly
    /// `0..id_space`; builds the inverse.
    fn from_pairs(by_external: Vec<IdPair<E>>) -> Result<Self, GraphError> {
        check_sorted(&by_external)?;
        let mut by_dense = memory::filled(by_external.len(), None)?;
        for &IdPair { external, dense } in &by_external {
            match usize::try_from(dense).ok().and_then(|d| by_dense.get_mut(d)) {
                Some(slot @ None) => *slot = Some(external),
                _ => return Err(GraphError::IdMapNotPermutation),
            }
        }
        let mut dense_to_external = memory::with_capacity(by_dense.len())?;
        for external in by_dense {
            dense_to_external.push(external.ok_or(GraphError::IdMapNotPermutation)?);
        }
        Ok(Self { by_external: by_external.into(), by_dense: dense_to_external.into() })
    }
}

/// The keys of a fixed-width rebuild: the pairs in external-id order and
/// the fresh ids in dense-id order, which extend the base's inverse.
#[derive(Debug)]
pub struct FixedRebuild<E> {
    by_external: BuildBuffer<IdPair<E>>,
    fresh: BuildBuffer<E>,
}

impl<E: ExternalId> KeysRebuild<FixedKeys<E>> for FixedRebuild<E> {
    fn keep(&mut self, key: &E, dense: DenseId) -> Result<(), GraphError> {
        self.by_external.push(IdPair { external: *key, dense })
    }

    fn add(&mut self, key: &E, dense: DenseId) -> Result<(), GraphError> {
        self.by_external.push(IdPair { external: *key, dense })?;
        self.fresh.push(*key)
    }

    fn finish(self, base: &FixedKeys<E>) -> Result<(FixedKeys<E>, BuildCharge), GraphError> {
        let Self { by_external, fresh } = self;
        let memory = by_external.memory().clone();
        let mut by_dense = BuildBuffer::with_capacity(&memory, base.by_dense.len() + fresh.len())?;
        by_dense.extend(base.by_dense.iter().copied());
        by_dense.extend(fresh.iter().copied());
        drop(fresh);
        let (by_external, mut charge) = by_external.into_parts();
        let (by_dense, by_dense_charge) = by_dense.into_parts();
        charge.absorb(by_dense_charge);
        let keys = FixedKeys { by_external: by_external.into(), by_dense: by_dense.into() };
        Ok((keys, charge))
    }
}

impl<E: ExternalId> NodeIds for BuildBuffer<E> {
    type Key = E;

    fn memory(&self) -> &BuildMemory {
        BuildBuffer::memory(self)
    }

    fn sort_distinct(&mut self) {
        self.sort_unstable();
        self.dedup();
    }

    fn keys(&self) -> impl Iterator<Item = &E> {
        self.iter()
    }
}

impl<E: ExternalId> IdMap<FixedKeys<E>> {
    /// The id map of the next build, whose live nodes are `externals`
    /// (duplicates allowed). Nodes in both keep their dense id; new nodes are
    /// appended at the high-water mark in external-id order; nodes that have
    /// disappeared are tombstoned. Dense ids are never reused, so a node that
    /// reappears after being tombstoned gets its old dense id back.
    pub fn rebuild(&self, externals: Vec<E>) -> Result<Self, GraphError> {
        let externals = BuildBuffer::from_vec(&BuildMemory::unlimited(), externals);
        Ok(self.rebuild_within(externals)?.0)
    }

    /// An id map from its (external id, dense id) pairs and tombstones,
    /// checking that the pairs are strictly sorted by external id, that the
    /// dense ids are exactly `0..id_space`, and that the tombstones cover
    /// the same id space.
    pub(crate) fn from_parts(
        by_external: Vec<IdPair<E>>,
        tombstones: Tombstones,
    ) -> Result<Self, GraphError> {
        check_tombstones(by_external.len() as u64, &tombstones)?;
        Ok(Self { keys: FixedKeys::from_pairs(by_external)?, tombstones })
    }

    /// An id map from its pairs, its inverse map and its tombstones, as
    /// segment buffers, owned or shared views. It checks that the pairs are
    /// strictly sorted by external id, that the dense ids are exactly
    /// `0..id_space`, that the tombstones cover the same id space, and that
    /// `by_dense` is the pairs' inverse, which it reads rather than builds.
    pub fn from_segments(
        by_external: SegmentBuffer<IdPair<E>>,
        by_dense: SegmentBuffer<E>,
        tombstones: Tombstones,
    ) -> Result<Self, GraphError> {
        check_tombstones(by_external.len() as u64, &tombstones)?;
        check_sorted(&by_external)?;
        if by_dense.len() != by_external.len() {
            return Err(GraphError::SegmentLength {
                expected: by_external.len() as u64,
                found: by_dense.len() as u64,
            });
        }
        // Distinct externals, each the inverse's entry at its dense id, so
        // the dense ids are distinct too: a permutation of `0..id_space`.
        let inverse = |pair: &IdPair<E>| {
            usize::try_from(pair.dense).ok().and_then(|dense| by_dense.get(dense))
                == Some(&pair.external)
        };
        if !by_external.iter().all(inverse) {
            return Err(GraphError::IdMapNotPermutation);
        }
        Ok(Self { keys: FixedKeys { by_external, by_dense }, tombstones })
    }

    /// The (external id, dense id) pairs, sorted by external id.
    pub(crate) fn pairs(&self) -> &[IdPair<E>] {
        &self.keys.by_external
    }
}

#[cfg(test)]
impl<E: ExternalId + Send + Sync + 'static> IdMap<FixedKeys<E>> {
    /// The same id map with every buffer a shared view of a heap segment.
    pub(crate) fn shared_views(&self) -> Self {
        use crate::graph::HeapSegment;
        let keys = FixedKeys {
            by_external: SegmentBuffer::shared(HeapSegment::of(&self.keys.by_external)),
            by_dense: SegmentBuffer::shared(HeapSegment::of(&self.keys.by_dense)),
        };
        Self { keys, tombstones: self.tombstones.shared_views() }
    }
}

/// Checks that the pairs are strictly sorted by external id.
fn check_sorted<E: Ord>(pairs: &[IdPair<E>]) -> Result<(), GraphError> {
    if pairs.windows(2).any(|pair| pair[0].external >= pair[1].external) {
        return Err(GraphError::IdMapNotSorted);
    }
    Ok(())
}
