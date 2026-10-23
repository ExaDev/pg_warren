//! External ids mapped in bulk by one call (`vector_from_query`, `gather`'s
//! message queries, `similarity`'s pairs; DESIGN.md §5.7): through the id
//! map's own lookup until the call has mapped [`INDEXED_AFTER`] ids, then,
//! for text ids, whose lookup is a binary search of three cache misses a
//! probe, through a hash index of the version's live ids, built once and
//! charged to `warren.max_compute_memory` before it is built. A call whose
//! compute memory has no room for the index keeps the binary search.

use warren_core::compute::{ComputeMemory, MemoryCharge};
use warren_core::graph::{BuildMemory, DenseId, IdMap, IndexedKeys};

use super::ExternalIdError;
use crate::source::IdDatum;

/// The ids a call maps before it builds a hash index: below this a binary
/// search per id costs less than building one (DESIGN.md §5.7).
pub(crate) const INDEXED_AFTER: usize = 65_536;

/// Maps one call's external ids to dense ids.
pub(crate) struct DenseIds<'m, E: IdDatum> {
    id_map: &'m IdMap<E>,
    memory: &'m ComputeMemory,
    lookup: Lookup<'m, E>,
}

/// How a call's ids are looked up.
enum Lookup<'m, E: IndexedKeys + 'm> {
    /// By the id map's binary search, the ids mapped so far counted.
    Searched(usize),
    /// Through a hash index, with its charge.
    Indexed { index: E::Index<'m>, _charge: MemoryCharge },
    /// By the binary search for the rest of the call: a fixed type, or an
    /// index the call's compute memory has no room for.
    Unindexed,
}

impl<'m, E: IdDatum> DenseIds<'m, E> {
    /// The lookups of one call into `id_map`, an index charged to `memory`.
    pub(crate) fn new(id_map: &'m IdMap<E>, memory: &'m ComputeMemory) -> Self {
        Self { id_map, memory, lookup: Lookup::Searched(0) }
    }

    /// The dense id of `key` if it is a live node's, as
    /// [`IdMap::dense_id`] answers; past [`INDEXED_AFTER`] ids a text id
    /// map's hash index answers, built on first use if it fits in the
    /// call's compute memory. An error only if building the index fails.
    pub(crate) fn dense_id(&mut self, key: &E::Key) -> Result<Option<DenseId>, ExternalIdError> {
        if let Lookup::Searched(mapped) = &mut self.lookup {
            *mapped += 1;
            if *mapped > INDEXED_AFTER {
                self.lookup = self.indexed()?;
            }
        }
        Ok(match &self.lookup {
            Lookup::Indexed { index, .. } => E::probe(index, key),
            Lookup::Searched(_) | Lookup::Unindexed => self.id_map.dense_id(key),
        })
    }

    /// The lookup past the threshold: a text id map's hash index, if the
    /// call's compute memory has room for it, else the binary search.
    fn indexed(&self) -> Result<Lookup<'m, E>, ExternalIdError> {
        let Some(bytes) = E::lookup_index_bytes(self.id_map) else {
            return Ok(Lookup::Unindexed);
        };
        let Ok(charge) = self.memory.charge(bytes) else {
            return Ok(Lookup::Unindexed);
        };
        // Charged above, exactly: the index's own budget is unlimited.
        let index = E::index(self.id_map, &BuildMemory::unlimited())?;
        Ok(Lookup::Indexed { index, _charge: charge })
    }

    /// Whether lookups go through a hash index.
    #[cfg(test)]
    fn is_indexed(&self) -> bool {
        matches!(self.lookup, Lookup::Indexed { .. })
    }

    /// Whether the call gave up on an index.
    #[cfg(test)]
    fn is_unindexed(&self) -> bool {
        matches!(self.lookup, Lookup::Unindexed)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use warren_core::graph::{FixedKeys, TextIds, TextIndex, TextKeys};

    use super::*;

    fn text_map(count: usize) -> IdMap<TextKeys> {
        let mut ids = TextIds::new(&BuildMemory::unlimited());
        for n in 0..count {
            ids.push(format!("node-{n}").as_bytes()).unwrap();
        }
        let (first, _) = IdMap::new().rebuild_within(ids).unwrap();
        let mut kept = TextIds::new(&BuildMemory::unlimited());
        for n in (0..count).filter(|n| n % 10 != 3) {
            kept.push(format!("node-{n}").as_bytes()).unwrap();
        }
        first.rebuild_within(kept).unwrap().0
    }

    #[test]
    fn a_call_past_the_threshold_indexes_text_ids_and_answers_as_the_id_map() {
        let id_map = text_map(1000);
        let memory = ComputeMemory::new(u64::MAX);
        let mut ids = DenseIds::new(&id_map, &memory);
        for round in 0..=(INDEXED_AFTER / 1000 + 1) {
            for n in 0..1001 {
                let key = format!("node-{n}");
                let expected = id_map.dense_id(key.as_bytes());
                assert_eq!(ids.dense_id(key.as_bytes()).unwrap(), expected, "{key}, {round}");
            }
        }
        assert!(ids.is_indexed());
        assert_eq!(memory.in_use(), TextIndex::bytes_for(&id_map));
        drop(ids);
        assert_eq!(memory.in_use(), 0);
    }

    /// A call whose index does not fit its compute memory keeps the
    /// binary search, which needs none, and does not try again.
    #[test]
    fn an_index_that_does_not_fit_the_call_keeps_the_binary_search() {
        let id_map = text_map(10);
        let memory = ComputeMemory::new(TextIndex::bytes_for(&id_map) - 1);
        let mut ids = DenseIds::new(&id_map, &memory);
        for _ in 0..=INDEXED_AFTER + 10 {
            assert_eq!(ids.dense_id(b"node-1").unwrap(), Some(1));
            assert_eq!(ids.dense_id(b"node-3").unwrap(), None, "tombstoned");
        }
        assert!(!ids.is_indexed());
        assert!(ids.is_unindexed(), "it gave up on the index");
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn fixed_ids_keep_the_binary_search() {
        let id_map = IdMap::<FixedKeys<i64>>::new().rebuild(vec![5, 7]).unwrap();
        let memory = ComputeMemory::new(0);
        let mut ids = DenseIds::new(&id_map, &memory);
        for _ in 0..=INDEXED_AFTER {
            assert_eq!(ids.dense_id(&7).unwrap(), Some(1));
        }
        assert!(!ids.is_indexed());
    }
}
