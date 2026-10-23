//! Edge rows taken a batch at a time (DESIGN.md §5.7): a build copies each
//! row's endpoints into a batch it owns, the compute pool maps the whole
//! batch's endpoints to dense ids through an index of the id map, and an
//! [`EdgeIntake`](super::EdgeIntake) then takes the rows in order. Mapping
//! is where the time goes for text ids, whose lookup costs three cache
//! misses, so it runs on every thread of the pool, never calling the
//! caller back.

mod endpoints;

pub use endpoints::{Endpoints, FixedEndpoints, TextEndpoints};

use std::mem::size_of;

use rayon::prelude::*;

use super::{
    BuildBuffer, BuildMemory, DenseId, ExternalId, FixedKeys, GraphError, IdKeys, IdMap,
    RelationIndex, TextIndex, TextKeys,
};
use crate::interrupts::{Cancellation, Cancelled};

/// Rows mapped between two checks of the cancellation flag, per thread.
const ROWS_PER_CHECK: usize = 4096;

/// A key storage whose ids a batch can be mapped through: an index of its
/// live ids that many threads probe at once.
pub trait IndexedKeys: IdKeys + Sync + Send {
    /// The index: the id map itself for a fixed type, whose lookup is one
    /// binary search of its pairs; a [`TextIndex`] for text.
    type Index<'m>: Sync
    where
        Self: 'm;
    /// How a batch holds its endpoints.
    type Endpoints: Endpoints<Key = Self::Key>;

    /// The index of `id_map`'s live ids, charged to `memory`.
    fn index<'m>(
        id_map: &'m IdMap<Self>,
        memory: &BuildMemory,
    ) -> Result<Self::Index<'m>, GraphError>;

    /// The dense id of a live node, as [`IdMap::dense_id`] answers.
    fn probe(index: &Self::Index<'_>, key: &Self::Key) -> Option<DenseId>;
}

impl<E: ExternalId + Send + Sync> IndexedKeys for FixedKeys<E> {
    type Index<'m>
        = &'m IdMap<Self>
    where
        Self: 'm;
    type Endpoints = FixedEndpoints<E>;

    fn index<'m>(id_map: &'m IdMap<Self>, _: &BuildMemory) -> Result<&'m IdMap<Self>, GraphError> {
        Ok(id_map)
    }

    fn probe(index: &&IdMap<Self>, key: &E) -> Option<DenseId> {
        index.dense_id(key)
    }
}

impl IndexedKeys for TextKeys {
    type Index<'m> = TextIndex<'m>;
    type Endpoints = TextEndpoints;

    fn index<'m>(
        id_map: &'m IdMap<Self>,
        memory: &BuildMemory,
    ) -> Result<TextIndex<'m>, GraphError> {
        TextIndex::new(id_map, memory)
    }

    fn probe(index: &TextIndex<'_>, key: &[u8]) -> Option<DenseId> {
        index.dense_id(key)
    }
}

/// The endpoints of a row as dense ids; `None` for a NULL or unknown one.
type Mapped = (Option<DenseId>, Option<DenseId>);

/// Edge rows in a buffer the build owns, every column charged to one
/// build memory.
#[derive(Debug)]
pub struct EdgeBatch<K: IndexedKeys> {
    relations: BuildBuffer<RelationIndex>,
    weights: BuildBuffer<f64>,
    sources: K::Endpoints,
    targets: K::Endpoints,
    /// Each row's endpoints, as [`EdgeBatch::map`] mapped them if
    /// `is_mapped`.
    mapped: BuildBuffer<Mapped>,
    /// Whether every row was mapped since the last was pushed.
    is_mapped: bool,
}

impl<K: IndexedKeys> EdgeBatch<K> {
    /// The bytes a row takes, beyond its text ids' own.
    pub const ROW_BYTES: u64 = (size_of::<RelationIndex>() + size_of::<f64>() + size_of::<Mapped>())
        as u64
        + 2 * K::Endpoints::ROW_BYTES;

    /// An empty batch with room for `rows` rows (but not their text ids'
    /// bytes), charged to `memory`. Emptied, it keeps room for up to
    /// `id_bytes` of text ids, half for each endpoint, and frees the rest.
    pub fn with_capacity(
        memory: &BuildMemory,
        rows: usize,
        id_bytes: u64,
    ) -> Result<Self, GraphError> {
        Ok(Self {
            relations: BuildBuffer::with_capacity(memory, rows)?,
            weights: BuildBuffer::with_capacity(memory, rows)?,
            sources: K::Endpoints::with_capacity(memory, rows, id_bytes / 2)?,
            targets: K::Endpoints::with_capacity(memory, rows, id_bytes / 2)?,
            mapped: BuildBuffer::with_capacity(memory, rows)?,
            is_mapped: false,
        })
    }

    /// Appends a row, copying its endpoints; a NULL (`None`) endpoint is
    /// kept as one, to be dropped and counted by the intake.
    pub fn push(
        &mut self,
        relation: RelationIndex,
        source: Option<&K::Key>,
        target: Option<&K::Key>,
        weight: f64,
    ) -> Result<(), GraphError> {
        self.is_mapped = false;
        self.sources.push(source)?;
        self.targets.push(target)?;
        self.relations.push(relation)?;
        self.weights.push(weight)?;
        self.mapped.push((None, None))
    }

    /// The rows it holds.
    pub fn len(&self) -> usize {
        self.relations.len()
    }

    /// The bytes it holds, capacity included: what it charges its memory.
    pub fn bytes(&self) -> u64 {
        let columns = [self.relations.bytes(), self.weights.bytes(), self.mapped.bytes()];
        columns.iter().sum::<u64>() + self.sources.bytes() + self.targets.bytes()
    }

    /// The bytes of the text ids it holds, capacity excluded: 0 for a
    /// fixed type.
    pub fn len_bytes(&self) -> u64 {
        self.sources.len_bytes() + self.targets.len_bytes()
    }

    /// Whether it holds no rows.
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }

    /// Maps every row's endpoints through `index`, in parallel on the
    /// current `rayon` pool (a job on the compute pool), stopping with
    /// [`Cancelled`] once `cancellation` is raised.
    pub fn map(
        &mut self,
        index: &K::Index<'_>,
        cancellation: &Cancellation,
    ) -> Result<(), Cancelled> {
        self.is_mapped = false;
        let (sources, targets) = (&self.sources, &self.targets);
        let probe =
            |endpoints: &K::Endpoints, row| endpoints.get(row).and_then(|key| K::probe(index, key));
        self.mapped.par_chunks_mut(ROWS_PER_CHECK).enumerate().try_for_each(|(chunk, rows)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            for (at, mapped) in rows.iter_mut().enumerate() {
                let row = chunk * ROWS_PER_CHECK + at;
                *mapped = (probe(sources, row), probe(targets, row));
            }
            Ok(())
        })?;
        self.is_mapped = true;
        Ok(())
    }

    /// Row `row`'s relation, endpoints and weight: the endpoints as
    /// [`EdgeBatch::map`] mapped them, or looked up in `id_map` if the
    /// batch was not mapped since its last row.
    pub(crate) fn row(
        &self,
        row: usize,
        id_map: &IdMap<K>,
    ) -> (RelationIndex, Option<DenseId>, Option<DenseId>, f64) {
        let (source, target) = match self.is_mapped {
            true => self.mapped[row],
            false => {
                let dense = |endpoints: &K::Endpoints| {
                    endpoints.get(row).and_then(|key| id_map.dense_id(key))
                };
                (dense(&self.sources), dense(&self.targets))
            }
        };
        (self.relations[row], source, target, self.weights[row])
    }

    /// Empties it, keeping its capacity, except text ids' room past what
    /// it was made to keep.
    pub(crate) fn clear(&mut self) {
        self.relations.truncate(0);
        self.weights.truncate(0);
        self.sources.clear();
        self.targets.clear();
        self.mapped.truncate(0);
        self.is_mapped = false;
    }
}

#[cfg(test)]
mod tests;
