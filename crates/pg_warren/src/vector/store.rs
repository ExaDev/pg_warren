//! The port to `warren.vectors` and `warren.vector_chunks`.

use pgrx::pg_sys;
use warren_core::compute::Dtype;

use super::VectorError;
use crate::catalog::VersionId;
use crate::segment::EachChunk;

/// `vectors.vector_id`.
pub(crate) type VectorId = i64;

/// A row of `warren.vectors`: a named vector bound to a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VectorRecord {
    /// Its id.
    pub(crate) vector: VectorId,
    /// Its name, unique within the version.
    pub(crate) name: String,
    /// Its element type.
    pub(crate) dtype: Dtype,
    /// Elements per row.
    pub(crate) width: usize,
    /// Rows: the version's id space.
    pub(crate) length: u64,
    /// The role that wrote it.
    pub(crate) created_by: pg_sys::Oid,
}

/// Where a vector is written: its graph, version and name, and the role
/// writing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Binding<'a> {
    /// `graphs.graph_id`.
    pub(crate) graph_id: i32,
    /// The version it is bound to.
    pub(crate) version: VersionId,
    /// Its name, unique within the version.
    pub(crate) name: &'a str,
    /// The role writing it.
    pub(crate) created_by: pg_sys::Oid,
    /// The vector of that name it replaces, as the writer checked it
    /// (`None`: the writer found none). Any other row of that name is a
    /// concurrent write ([`VectorError::WrittenConcurrently`]).
    pub(crate) replacing: Option<VectorId>,
}

impl<'a> Binding<'a> {
    /// Vector `name` bound to `version` of graph `graph_id`, written by role
    /// `created_by`.
    pub(crate) fn new(
        graph_id: i32,
        version: VersionId,
        name: &'a str,
        created_by: pg_sys::Oid,
    ) -> Self {
        Self { graph_id, version, name, created_by, replacing: None }
    }

    /// The same binding, replacing vector `replacing` (as checked).
    pub(crate) fn replacing(self, replacing: Option<VectorId>) -> Self {
        Self { replacing, ..self }
    }
}

/// Stores the next chunk of a vector.
pub(crate) type ChunkSink<'a> = dyn FnMut(&[u8]) -> Result<(), VectorError> + 'a;

/// Produces a vector's chunks, in order, into a sink.
pub(crate) type ChunkSource<'a> = dyn FnMut(&mut ChunkSink<'_>) -> Result<(), VectorError> + 'a;

/// The port to the vectors bound to each version, and their chunks.
pub(crate) trait VectorStore {
    /// The vectors bound to `version`, by name.
    fn bound(&mut self, version: VersionId) -> Result<Vec<VectorRecord>, VectorError>;

    /// Records a vector at `binding`, replacing `binding.replacing`, and
    /// returns its id. Any other vector of that name (written, replaced or
    /// dropped by a concurrent transaction since the writer checked) is
    /// [`VectorError::WrittenConcurrently`]: nothing unchecked is deleted.
    fn insert(
        &mut self,
        binding: &Binding<'_>,
        dtype: Dtype,
        width: usize,
        length: u64,
    ) -> Result<VectorId, VectorError>;

    /// Stores the chunks `source` produces as chunks `0..n` of `vector`
    /// (named `name`, for errors), through one prepared statement.
    fn write_chunks(
        &mut self,
        vector: VectorId,
        name: &str,
        source: &mut ChunkSource<'_>,
    ) -> Result<(), VectorError>;

    /// Passes each chunk of a vector to `each` as (chunk number, bytes), in
    /// chunk order.
    fn read_chunks(
        &mut self,
        vector: VectorId,
        each: &mut EachChunk<'_, VectorError>,
    ) -> Result<(), VectorError>;

    /// Deletes vector `vector` (its chunks with it); returns whether it
    /// still existed.
    fn delete(&mut self, vector: VectorId) -> Result<bool, VectorError>;

    /// Which of `vectors` still exist.
    fn existing(&mut self, vectors: &[VectorId]) -> Result<Vec<VectorId>, VectorError>;
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory [`VectorStore`].
    #[derive(Debug, Default)]
    pub(crate) struct MemoryVectorStore {
        pub(crate) rows: BTreeMap<VectorId, (VersionId, VectorRecord)>,
        pub(crate) chunks: BTreeMap<(VectorId, i32), Vec<u8>>,
        next: VectorId,
    }

    impl VectorStore for MemoryVectorStore {
        fn bound(&mut self, version: VersionId) -> Result<Vec<VectorRecord>, VectorError> {
            let mut bound: Vec<VectorRecord> = self
                .rows
                .values()
                .filter(|(v, _)| *v == version)
                .map(|(_, record)| record.clone())
                .collect();
            bound.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(bound)
        }

        fn insert(
            &mut self,
            binding: &Binding<'_>,
            dtype: Dtype,
            width: usize,
            length: u64,
        ) -> Result<VectorId, VectorError> {
            let existing: Vec<VectorId> = self
                .rows
                .iter()
                .filter(|(_, (v, record))| *v == binding.version && record.name == binding.name)
                .map(|(id, _)| *id)
                .collect();
            if existing != binding.replacing.into_iter().collect::<Vec<_>>() {
                return Err(VectorError::WrittenConcurrently {
                    name: binding.name.to_owned(),
                    version: binding.version,
                });
            }
            for id in existing {
                self.delete(id)?;
            }
            // (The SPI adapter deletes the checked id, then lets the insert
            // conflict with any other row of that name: the same outcome.)
            self.next += 1;
            let record = VectorRecord {
                vector: self.next,
                name: binding.name.to_owned(),
                dtype,
                width,
                length,
                created_by: binding.created_by,
            };
            self.rows.insert(self.next, (binding.version, record));
            Ok(self.next)
        }

        fn write_chunks(
            &mut self,
            vector: VectorId,
            name: &str,
            source: &mut ChunkSource<'_>,
        ) -> Result<(), VectorError> {
            let mut chunk_no = 0i32;
            source(&mut |data| {
                self.chunks.insert((vector, chunk_no), data.to_vec());
                chunk_no = chunk_no
                    .checked_add(1)
                    .ok_or_else(|| VectorError::TooManyChunks(name.to_owned()))?;
                Ok(())
            })
        }

        fn read_chunks(
            &mut self,
            vector: VectorId,
            each: &mut EachChunk<'_, VectorError>,
        ) -> Result<(), VectorError> {
            for ((_, chunk_no), data) in self.chunks.range((vector, 0)..=(vector, i32::MAX)) {
                each(*chunk_no, data)?;
            }
            Ok(())
        }

        fn delete(&mut self, vector: VectorId) -> Result<bool, VectorError> {
            self.chunks.retain(|(id, _), _| *id != vector);
            Ok(self.rows.remove(&vector).is_some())
        }

        fn existing(&mut self, vectors: &[VectorId]) -> Result<Vec<VectorId>, VectorError> {
            Ok(vectors.iter().copied().filter(|id| self.rows.contains_key(id)).collect())
        }
    }
}
