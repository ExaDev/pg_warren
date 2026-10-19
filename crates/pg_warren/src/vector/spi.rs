//! The SPI adapter of the vector store: `warren.vectors` and
//! `warren.vector_chunks`, read and written as the extension owner
//! (DESIGN.md §5.1, §7).

use pgrx::prelude::*;
use pgrx::spi::SpiHeapTupleData;
use warren_core::compute::Dtype;

use super::{ChunkVisitor, VectorError, VectorId, VectorRecord, VectorStore};
use crate::access::as_extension_owner;
use crate::catalog::VersionId;

/// `warren.vectors` and `warren.vector_chunks`.
pub(crate) struct SpiVectorStore;

/// A `vectors` row as (vector_id, name, dtype, width, length).
fn record(row: &SpiHeapTupleData<'_>) -> Result<VectorRecord, VectorError> {
    let vector: VectorId = row.get(1)?.unwrap_or_default();
    let dtype: String = row.get(3)?.unwrap_or_default();
    Ok(VectorRecord {
        vector,
        name: row.get(2)?.unwrap_or_default(),
        dtype: dtype.parse::<Dtype>().map_err(|_| VectorError::UnknownDtype { vector, dtype })?,
        width: usize::try_from(row.get::<i32>(4)?.unwrap_or(1)).unwrap_or(1),
        length: u64::try_from(row.get::<i64>(5)?.unwrap_or_default()).unwrap_or_default(),
    })
}

impl VectorStore for SpiVectorStore {
    fn bound(&mut self, version: VersionId) -> Result<Vec<VectorRecord>, VectorError> {
        as_extension_owner(|| {
            Spi::connect(|client| {
                client
                    .select(
                        "SELECT vector_id, name, dtype, width, length FROM warren.vectors
                         WHERE version_id = $1 ORDER BY name",
                        None,
                        &[version.into()],
                    )?
                    .map(|row| record(&row))
                    .collect()
            })
        })
    }

    fn insert(
        &mut self,
        graph_id: i32,
        version: VersionId,
        name: &str,
        dtype: Dtype,
        width: usize,
        length: u64,
    ) -> Result<VectorId, VectorError> {
        self.delete(version, name)?;
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let length = i64::try_from(length).unwrap_or(i64::MAX);
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                let row = client
                    .update(
                        "INSERT INTO warren.vectors (graph_id, version_id, name, dtype, width, length)
                         VALUES ($1, $2, $3, $4, $5, $6) RETURNING vector_id",
                        Some(1),
                        &[
                            graph_id.into(),
                            version.into(),
                            name.into(),
                            dtype.name().into(),
                            width.into(),
                            length.into(),
                        ],
                    )?
                    .first();
                Ok(row.get::<VectorId>(1)?.unwrap_or_default())
            })
        })
    }

    fn write_chunk(
        &mut self,
        vector: VectorId,
        chunk_no: i32,
        data: &[u8],
    ) -> Result<(), VectorError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "INSERT INTO warren.vector_chunks (vector_id, chunk_no, data)
                     VALUES ($1, $2, $3)",
                    None,
                    &[vector.into(), chunk_no.into(), data.into()],
                )?;
                Ok(())
            })
        })
    }

    fn read_chunks(
        &mut self,
        vector: VectorId,
        each: &mut ChunkVisitor<'_>,
    ) -> Result<(), VectorError> {
        as_extension_owner(|| {
            Spi::connect(|client| {
                let mut cursor = client.try_open_cursor(
                    "SELECT chunk_no, data FROM warren.vector_chunks
                     WHERE vector_id = $1 ORDER BY chunk_no",
                    &[vector.into()],
                )?;
                loop {
                    let chunk = cursor.fetch(1)?;
                    let Some(row) = chunk.into_iter().next() else {
                        return Ok(());
                    };
                    let chunk_no = row.get::<i32>(1)?.unwrap_or_default();
                    each(chunk_no, row.get::<&[u8]>(2)?.unwrap_or_default())?;
                    // SAFETY: SPI_tuptable is the chunk just consumed, which
                    // nothing refers to any more.
                    unsafe { pg_sys::SPI_freetuptable(pg_sys::SPI_tuptable) };
                }
            })
        })
    }

    fn delete(&mut self, version: VersionId, name: &str) -> Result<bool, VectorError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                let deleted = client.update(
                    "DELETE FROM warren.vectors WHERE version_id = $1 AND name = $2",
                    None,
                    &[version.into(), name.into()],
                )?;
                Ok(!deleted.is_empty())
            })
        })
    }

    fn existing(&mut self, vectors: &[VectorId]) -> Result<Vec<VectorId>, VectorError> {
        if vectors.is_empty() {
            return Ok(Vec::new());
        }
        as_extension_owner(|| {
            Ok::<_, VectorError>(
                Spi::get_one_with_args::<Vec<VectorId>>(
                    "SELECT coalesce(array_agg(vector_id), '{}') FROM warren.vectors
                     WHERE vector_id = ANY ($1)",
                    &[vectors.to_vec().into()],
                )?
                .unwrap_or_default(),
            )
        })
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::compute::{AnyVector, ComputeMemory, Vector};

    use super::SpiVectorStore;
    use crate::fixture::{refresh, ring};
    use crate::segment::CHUNK_SIZE;
    use crate::vector::{read_vector, write_vector, VectorStore};

    fn graph_id(name: &str) -> Result<i32, Error> {
        Spi::get_one::<i32>(&format!("SELECT graph_id FROM warren.graphs WHERE name = '{name}'"))?
            .ok_or(Error::InvalidPosition)
    }

    #[pg_test]
    fn a_vector_round_trips_through_its_tables_in_several_chunks() -> Result<(), Error> {
        ring("vs", 300, 2)?;
        let version = refresh("vs", true)?;
        let memory = ComputeMemory::new(1 << 20);
        let values: Vec<f64> = (0..300).map(|i| f64::from(i).sqrt()).collect();
        let vector: AnyVector = Vector::from_values(&memory, 300, 1, values)
            .map_err(|_| Error::InvalidPosition)?
            .into();
        let mut store = SpiVectorStore;
        let record =
            write_vector(&mut store, graph_id("vs")?, version, "roots", &vector, CHUNK_SIZE)
                .map_err(|_| Error::InvalidPosition)?;
        let chunks = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM warren.vector_chunks WHERE vector_id = {}",
            record.vector
        ))?;
        assert_eq!(chunks, Some(3), "2400 bytes in 1 KiB chunks");
        let row = Spi::get_one::<String>(&format!(
            "SELECT name || ' ' || dtype || ' ' || width || ' ' || length FROM warren.vectors
             WHERE version_id = {version}"
        ))?;
        assert_eq!(row.as_deref(), Some("roots f64 1 300"));
        let back = read_vector(&mut store, &memory, &record).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(back, vector);
        assert_eq!(store.bound(version).map_err(|_| Error::InvalidPosition)?, [record]);
        Ok(())
    }

    #[pg_test]
    fn writing_a_name_again_replaces_the_row_and_its_chunks() -> Result<(), Error> {
        ring("vs", 10, 2)?;
        let version = refresh("vs", true)?;
        let memory = ComputeMemory::new(1 << 20);
        let ones: AnyVector =
            Vector::filled(&memory, 10, 1, 1i64).map_err(|_| Error::InvalidPosition)?.into();
        let mut store = SpiVectorStore;
        let graph = graph_id("vs")?;
        let first = write_vector(&mut store, graph, version, "x", &ones, CHUNK_SIZE)
            .map_err(|_| Error::InvalidPosition)?;
        let second = write_vector(&mut store, graph, version, "x", &ones, CHUNK_SIZE)
            .map_err(|_| Error::InvalidPosition)?;
        let existing =
            store.existing(&[first.vector, second.vector]).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(existing, [second.vector]);
        let chunks = Spi::get_one::<i64>("SELECT count(*) FROM warren.vector_chunks")?;
        assert_eq!(chunks, Some(1));
        assert!(store.delete(version, "x").map_err(|_| Error::InvalidPosition)?);
        assert!(!store.delete(version, "x").map_err(|_| Error::InvalidPosition)?);
        let left = Spi::get_one::<i64>("SELECT count(*) FROM warren.vector_chunks")?;
        assert_eq!(left, Some(0));
        Ok(())
    }
}
