//! The key storage of text external ids (DESIGN.md §5.7): every id's bytes
//! in dense-id order (`id_bytes`), where each id starts and ends
//! (`id_offsets`), and the dense ids sorted bytewise (`id_order`). Both
//! directions are stored, so loading builds nothing.

mod ids;
mod index;
mod order;
mod rebuild;

pub use ids::TextIds;
pub use index::TextIndex;
pub use order::IdOrder;
pub use rebuild::TextRebuild;

use std::cmp::Ordering;

use super::{check_tombstones, FreshKeys, IdKeys, IdMap};
use crate::graph::{
    memory, BuildBuffer, BuildMemory, DenseId, GraphError, Permutation, SegmentBuffer, TargetWidth,
    Tombstones,
};

/// The keys of a text id type: the three arrays, each owned or a shared
/// view. Ids compare bytewise (`memcmp`, then length), which is
/// `COLLATE "C"` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextKeys {
    bytes: SegmentBuffer<u8>,
    offsets: SegmentBuffer<u64>,
    order: IdOrder,
}

impl Default for TextKeys {
    fn default() -> Self {
        Self { bytes: SegmentBuffer::default(), offsets: vec![0].into(), order: IdOrder::default() }
    }
}

impl IdKeys for TextKeys {
    type Key = [u8];
    type Ids = TextIds;
    type Rebuild = TextRebuild;

    fn id_space(&self) -> u64 {
        self.offsets.len().saturating_sub(1) as u64
    }

    fn bytes(&self) -> u64 {
        self.bytes.bytes() + self.offsets.bytes() + self.order.bytes()
    }

    /// A binary search of `id_order`.
    fn dense_id(&self, key: &[u8]) -> Option<DenseId> {
        let (mut low, mut high) = (0, self.order.len());
        while low < high {
            let middle = low + (high - low) / 2;
            let dense = self.order.get(middle)?;
            match self.key(dense)?.cmp(key) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(dense),
            }
        }
        None
    }

    fn key(&self, dense: DenseId) -> Option<&[u8]> {
        let dense = usize::try_from(dense).ok()?;
        let start = usize::try_from(*self.offsets.get(dense)?).ok()?;
        let end = usize::try_from(*self.offsets.get(dense.checked_add(1)?)?).ok()?;
        self.bytes.get(start..end)
    }

    fn in_order(&self) -> impl Iterator<Item = (&[u8], DenseId)> {
        (0..self.order.len()).filter_map(|rank| {
            let dense = self.order.get(rank)?;
            Some((self.key(dense)?, dense))
        })
    }

    /// Moves each id's bytes and offsets to its new dense id, and maps each
    /// `id_order` entry through `permutation`; the new arrays are charged
    /// while they are built beside the old ones.
    fn renumber(
        &mut self,
        permutation: &Permutation,
        memory: &BuildMemory,
    ) -> Result<(), GraphError> {
        let id_space = self.id_space() as usize;
        let mut offsets = BuildBuffer::filled(memory, id_space + 1, 0)?;
        for old in 0..self.id_space() {
            let new = permutation.new_dense_id(old) as usize;
            offsets[new + 1] = self.key(old).map_or(0, <[u8]>::len) as u64;
        }
        for dense in 0..id_space {
            offsets[dense + 1] += offsets[dense];
        }
        let mut bytes = BuildBuffer::filled(memory, self.bytes.len(), 0)?;
        for old in 0..self.id_space() {
            let new = permutation.new_dense_id(old) as usize;
            let (start, end) = (offsets[new] as usize, offsets[new + 1] as usize);
            bytes[start..end].copy_from_slice(self.key(old).unwrap_or_default());
        }
        let (order, _order_charge) = self.order.renumbered(permutation, memory)?;
        self.bytes = bytes.into_parts().0.into();
        self.offsets = offsets.into_parts().0.into();
        self.order = order;
        Ok(())
    }

    /// Copies the ids at `old_dense_ids`; the new order is the identity.
    fn compacted(&self, old_dense_ids: &[DenseId]) -> Result<Self, GraphError> {
        let keys = |old| self.key(old).ok_or(GraphError::IdMapNotPermutation);
        let mut length = 0;
        for &old in old_dense_ids {
            length += keys(old)?.len();
        }
        let mut bytes = memory::with_capacity(length)?;
        let mut offsets = memory::with_capacity(old_dense_ids.len() + 1)?;
        offsets.push(0);
        for &old in old_dense_ids {
            bytes.extend_from_slice(keys(old)?);
            offsets.push(bytes.len() as u64);
        }
        let order = IdOrder::identity(old_dense_ids.len() as u64)?;
        Ok(Self { bytes: bytes.into(), offsets: offsets.into(), order })
    }

    fn rebuild(&self, fresh: FreshKeys, memory: &BuildMemory) -> Result<TextRebuild, GraphError> {
        TextRebuild::new(self, fresh, memory)
    }
}

impl TextKeys {
    /// `id_bytes`: every id's bytes, concatenated in dense-id order.
    pub fn id_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `id_offsets`: dense id `d`'s id is
    /// `id_bytes[id_offsets[d]..id_offsets[d + 1]]`.
    pub fn id_offsets(&self) -> &[u64] {
        &self.offsets
    }

    /// `id_order`: the dense ids sorted by their ids, bytewise.
    pub fn id_order(&self) -> &IdOrder {
        &self.order
    }

    /// The keys of the three arrays, after checking that the offsets start
    /// at 0, never decrease and end at the bytes' length, and that the
    /// order is a permutation of the id space (a bitmap, O(N)), at its
    /// target width and strictly increasing by id (O(N + bytes)).
    fn checked(
        bytes: SegmentBuffer<u8>,
        offsets: SegmentBuffer<u64>,
        order: IdOrder,
    ) -> Result<Self, GraphError> {
        if offsets.first() != Some(&0) {
            return Err(GraphError::IdOffsetsStart);
        }
        if let Some(dense) = offsets.windows(2).position(|pair| pair[0] > pair[1]) {
            return Err(GraphError::IdOffsetsNotMonotone { dense: dense as DenseId });
        }
        let end = offsets.last().copied().unwrap_or_default();
        if end != bytes.len() as u64 {
            return Err(GraphError::IdOffsetsEnd { expected: bytes.len() as u64, found: end });
        }
        let keys = Self { bytes, offsets, order };
        let id_space = keys.id_space();
        if keys.order.len() as u64 != id_space {
            let found = keys.order.len() as u64;
            return Err(GraphError::SegmentLength { expected: id_space, found });
        }
        if keys.order.width() != TargetWidth::for_id_space(id_space) {
            return Err(GraphError::TargetWidthMismatch);
        }
        keys.check_order()?;
        Ok(keys)
    }

    /// Checks that `order` is a permutation of the id space, strictly
    /// increasing by id.
    fn check_order(&self) -> Result<(), GraphError> {
        let mut seen = memory::filled(Tombstones::bytes_for(self.id_space()) as usize / 8, 0u64)?;
        let mut previous: Option<&[u8]> = None;
        for rank in 0..self.order.len() {
            let dense = self.order.get(rank).ok_or(GraphError::IdMapNotPermutation)?;
            let word = seen.get_mut((dense / 64) as usize).filter(|_| dense < self.id_space());
            match word {
                Some(word) if *word & (1 << (dense % 64)) == 0 => *word |= 1 << (dense % 64),
                _ => return Err(GraphError::IdMapNotPermutation),
            }
            let key = self.key(dense).ok_or(GraphError::IdMapNotPermutation)?;
            if previous.is_some_and(|previous| previous >= key) {
                return Err(GraphError::IdMapNotSorted);
            }
            previous = Some(key);
        }
        Ok(())
    }
}

impl IdMap<TextKeys> {
    /// A text id map from its three arrays and its tombstones, as segment
    /// buffers, owned or shared views, after checking them (see
    /// `TextKeys::checked`) and that the tombstones cover the same id
    /// space.
    pub fn from_text_segments(
        bytes: SegmentBuffer<u8>,
        offsets: SegmentBuffer<u64>,
        order: IdOrder,
        tombstones: Tombstones,
    ) -> Result<Self, GraphError> {
        let keys = TextKeys::checked(bytes, offsets, order)?;
        check_tombstones(keys.id_space(), &tombstones)?;
        Ok(Self { keys, tombstones })
    }
}

#[cfg(test)]
mod tests;
