//! The tombstone bitmap: one bit per dense id in the id space, set for nodes
//! that have disappeared from the graph but keep their dense id (DESIGN.md §5.2).

use super::{memory, DenseId, GraphError};

const WORD_BITS: u64 = u64::BITS as u64;

/// One bit per dense id; a set bit marks a tombstoned node.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tombstones {
    id_space: u64,
    words: Vec<u64>,
}

impl Tombstones {
    /// A bitmap over `id_space` dense ids with no tombstones.
    pub fn new(id_space: u64) -> Result<Self, GraphError> {
        Ok(Self { id_space, words: memory::filled(word_count(id_space), 0)? })
    }

    /// A copy of the bitmap, failing rather than aborting if memory is short.
    pub(crate) fn try_clone(&self) -> Result<Self, GraphError> {
        let mut words = memory::with_capacity(self.words.len())?;
        words.extend_from_slice(&self.words);
        Ok(Self { id_space: self.id_space, words })
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
        memory::held(&self.words)
    }

    /// The number of tombstoned dense ids.
    pub fn count(&self) -> u64 {
        self.words.iter().map(|word| u64::from(word.count_ones())).sum()
    }

    /// Whether `id` is tombstoned. Ids outside the id space are not.
    pub fn contains(&self, id: DenseId) -> bool {
        id < self.id_space && self.words[word_index(id)] & bit(id) != 0
    }

    /// Tombstones `id`. Ids outside the id space are ignored.
    pub fn insert(&mut self, id: DenseId) {
        if id < self.id_space {
            self.words[word_index(id)] |= bit(id);
        }
    }

    /// Clears the tombstone on `id`, if any.
    pub fn remove(&mut self, id: DenseId) {
        if id < self.id_space {
            self.words[word_index(id)] &= !bit(id);
        }
    }

    /// The bitmap's words, least significant bit first.
    pub(crate) fn words(&self) -> &[u64] {
        &self.words
    }

    /// A bitmap from its words, which must cover exactly `id_space` bits.
    pub(crate) fn from_words(id_space: u64, words: Vec<u64>) -> Result<Self, GraphError> {
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
        self.words
            .try_reserve_exact(words - self.words.len())
            .map_err(|_| GraphError::OutOfMemory { elements: words })?;
        self.words.resize(words, 0);
        self.id_space = id_space;
        Ok(())
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
    use super::*;

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
        tombstones.insert(0);
        tombstones.insert(64);
        tombstones.insert(129);
        assert!(tombstones.contains(64));
        assert_eq!(tombstones.count(), 3);
        tombstones.remove(64);
        assert!(!tombstones.contains(64));
        assert_eq!(tombstones.count(), 2);
    }

    #[test]
    fn ids_outside_the_id_space_are_never_tombstoned() {
        let mut tombstones = Tombstones::new(3).unwrap();
        tombstones.insert(3);
        assert!(!tombstones.contains(3));
        assert_eq!(tombstones.count(), 0);
    }

    #[test]
    fn a_fallible_clone_equals_the_original() {
        let mut tombstones = Tombstones::new(70).unwrap();
        tombstones.insert(69);
        assert_eq!(tombstones.try_clone(), Ok(tombstones));
    }

    #[test]
    fn growing_keeps_existing_tombstones_and_adds_live_ids() {
        let mut tombstones = Tombstones::new(63).unwrap();
        tombstones.insert(62);
        tombstones.grow_to(200).unwrap();
        assert_eq!(tombstones.id_space(), 200);
        assert!(tombstones.contains(62));
        assert!((63..200).all(|id| !tombstones.contains(id)));
    }
}
