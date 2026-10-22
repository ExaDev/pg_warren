//! The tombstone bitmap: one bit per dense id in the id space, set for nodes
//! that have disappeared from the graph but keep their dense id (DESIGN.md §5.2).

use super::{memory, DenseId, GraphError, SegmentBuffer};

const WORD_BITS: u64 = u64::BITS as u64;

/// One bit per dense id; a set bit marks a tombstoned node.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tombstones {
    id_space: u64,
    words: SegmentBuffer<u64>,
}

impl Tombstones {
    /// A bitmap over `id_space` dense ids with no tombstones.
    pub fn new(id_space: u64) -> Result<Self, GraphError> {
        Ok(Self { id_space, words: memory::filled(word_count(id_space), 0)?.into() })
    }

    /// A copy of the bitmap, failing rather than aborting if memory is short.
    pub(crate) fn try_clone(&self) -> Result<Self, GraphError> {
        let mut words = memory::with_capacity(self.words.len())?;
        words.extend_from_slice(&self.words);
        Ok(Self { id_space: self.id_space, words: words.into() })
    }

    /// The number of dense ids the bitmap covers.
    pub fn id_space(&self) -> u64 {
        self.id_space
    }

    /// The heap bytes a bitmap over `id_space` dense ids holds.
    pub fn bytes_for(id_space: u64) -> u64 {
        (word_count(id_space) as u64).saturating_mul(8)
    }

    /// The heap bytes the bitmap holds.
    pub fn bytes(&self) -> u64 {
        self.words.bytes()
    }

    /// The number of tombstoned dense ids.
    pub fn count(&self) -> u64 {
        self.words.iter().map(|word| u64::from(word.count_ones())).sum()
    }

    /// Whether `id` is tombstoned. Ids outside the id space are not.
    pub fn contains(&self, id: DenseId) -> bool {
        id < self.id_space && self.words[word_index(id)] & bit(id) != 0
    }

    /// Tombstones `id`. Ids outside the id space are ignored. A shared
    /// bitmap is copied first.
    pub fn insert(&mut self, id: DenseId) -> Result<(), GraphError> {
        if id < self.id_space {
            self.words.make_mut()?[word_index(id)] |= bit(id);
        }
        Ok(())
    }

    /// Clears the tombstone on `id`, if any. A shared bitmap is copied
    /// first.
    pub fn remove(&mut self, id: DenseId) -> Result<(), GraphError> {
        if id < self.id_space {
            self.words.make_mut()?[word_index(id)] &= !bit(id);
        }
        Ok(())
    }

    /// The bitmap's words, least significant bit first.
    pub(crate) fn words(&self) -> &[u64] {
        &self.words
    }

    /// A bitmap from its words, which must cover exactly `id_space` bits.
    pub(crate) fn from_words(id_space: u64, words: Vec<u64>) -> Result<Self, GraphError> {
        Self::from_segment(id_space, words.into())
    }

    /// A bitmap from its words in a segment buffer, owned or a shared view,
    /// which must cover exactly `id_space` bits.
    pub fn from_segment(id_space: u64, words: SegmentBuffer<u64>) -> Result<Self, GraphError> {
        if words.len() != word_count(id_space) {
            return Err(GraphError::SegmentLength {
                expected: word_count(id_space) as u64,
                found: words.len() as u64,
            });
        }
        let spare_bits = words.len() as u64 * WORD_BITS - id_space;
        let spare = words
            .last()
            .map_or(0, |last| last.checked_shr((WORD_BITS - spare_bits) as u32).unwrap_or(0));
        if spare != 0 {
            return Err(GraphError::TombstoneOutsideIdSpace);
        }
        Ok(Self { id_space, words })
    }

    /// Extends the id space to `id_space`; the new ids are live.
    pub fn grow_to(&mut self, id_space: u64) -> Result<(), GraphError> {
        if id_space <= self.id_space {
            return Ok(());
        }
        let words = word_count(id_space);
        let owned = self.words.make_mut()?;
        owned
            .try_reserve_exact(words - owned.len())
            .map_err(|_| GraphError::OutOfMemory { elements: words })?;
        owned.resize(words, 0);
        self.id_space = id_space;
        Ok(())
    }
}

#[cfg(test)]
impl Tombstones {
    /// The same bitmap as a shared view of a heap segment.
    pub(crate) fn shared_views(&self) -> Self {
        let words = SegmentBuffer::shared(super::HeapSegment::of(&self.words));
        Self { id_space: self.id_space, words }
    }
}

fn word_count(id_space: u64) -> usize {
    id_space.div_ceil(WORD_BITS) as usize
}

fn word_index(id: DenseId) -> usize {
    (id / WORD_BITS) as usize
}

fn bit(id: DenseId) -> u64 {
    1 << (id % WORD_BITS)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::graph::{HeapSegment, SharedSegment};

    #[test]
    fn a_bitmap_holds_one_word_per_64_ids() {
        assert_eq!(Tombstones::new(65).unwrap().bytes(), 16);
        assert_eq!(Tombstones::new(0).unwrap().bytes(), 0);
    }

    #[test]
    fn a_new_bitmap_has_no_tombstones() {
        let tombstones = Tombstones::new(130).unwrap();
        assert_eq!(tombstones.id_space(), 130);
        assert_eq!(tombstones.count(), 0);
        assert!((0..130).all(|id| !tombstones.contains(id)));
    }

    #[test]
    fn inserting_and_removing_a_tombstone() {
        let mut tombstones = Tombstones::new(130).unwrap();
        tombstones.insert(0).unwrap();
        tombstones.insert(64).unwrap();
        tombstones.insert(129).unwrap();
        assert!(tombstones.contains(64));
        assert_eq!(tombstones.count(), 3);
        tombstones.remove(64).unwrap();
        assert!(!tombstones.contains(64));
        assert_eq!(tombstones.count(), 2);
    }

    #[test]
    fn ids_outside_the_id_space_are_never_tombstoned() {
        let mut tombstones = Tombstones::new(3).unwrap();
        tombstones.insert(3).unwrap();
        assert!(!tombstones.contains(3));
        assert_eq!(tombstones.count(), 0);
    }

    #[test]
    fn a_fallible_clone_equals_the_original() {
        let mut tombstones = Tombstones::new(70).unwrap();
        tombstones.insert(69).unwrap();
        assert_eq!(tombstones.try_clone(), Ok(tombstones));
    }

    #[test]
    fn a_shared_bitmap_reads_as_the_owned_one() {
        let mut owned = Tombstones::new(130).unwrap();
        [0, 64, 129].into_iter().try_for_each(|id| owned.insert(id)).unwrap();
        let region = HeapSegment::of(owned.words());
        let shared = Tombstones::from_segment(130, SegmentBuffer::shared(region)).unwrap();
        assert_eq!(shared, owned);
        assert_eq!(shared.count(), 3);
        assert!(shared.contains(64) && !shared.contains(63));
        assert_eq!(shared.bytes(), owned.bytes());
    }

    #[test]
    fn a_shared_bitmap_is_validated_as_an_owned_one_is() {
        let words = |words: &[u64]| SegmentBuffer::shared(HeapSegment::of(words));
        assert_eq!(
            Tombstones::from_segment(130, words(&[0, 0])),
            Err(GraphError::SegmentLength { expected: 3, found: 2 })
        );
        assert_eq!(
            Tombstones::from_segment(3, words(&[1 << 3])),
            Err(GraphError::TombstoneOutsideIdSpace)
        );
    }

    #[test]
    fn changing_a_shared_bitmap_copies_it() {
        let region = HeapSegment::of(&[0b10u64]);
        let mut tombstones =
            Tombstones::from_segment(3, SegmentBuffer::shared(Arc::clone(&region) as _)).unwrap();
        tombstones.insert(0).unwrap();
        tombstones.remove(1).unwrap();
        assert!(tombstones.contains(0) && !tombstones.contains(1));
        assert_eq!(region.elements(), &[0b10]);
        tombstones.grow_to(70).unwrap();
        assert_eq!(tombstones.count(), 1);
    }

    #[test]
    fn growing_keeps_existing_tombstones_and_adds_live_ids() {
        let mut tombstones = Tombstones::new(63).unwrap();
        tombstones.insert(62).unwrap();
        tombstones.grow_to(200).unwrap();
        assert_eq!(tombstones.id_space(), 200);
        assert!(tombstones.contains(62));
        assert!((63..200).all(|id| !tombstones.contains(id)));
    }
}
