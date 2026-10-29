//! The SPI adapter of the vector store: `warren.vectors` and
//! `warren.vector_chunks`, read and written as the catalog owner
//! (DESIGN.md §5.1, §7).

use pgrx::prelude::*;
use pgrx::spi::SpiHeapTupleData;
use warren_core::compute::Dtype;

use super::{Binding, Checked, ChunkSource, VectorError, VectorId, VectorRecord, VectorStore};
use crate::access::as_catalog_owner;
use crate::catalog::VersionId;
use crate::read_only::{read_one, select, update_saved, writing};
use crate::segment::{each_chunk, EachChunk};

/// `warren.vectors` and `warren.vector_chunks`. Every statement that
/// writes or locks their rows runs through a saved plan, planned once per
/// backend (`read_only::saved`).
pub(crate) struct SpiVectorStore;

/// [`lock_version`]'s lock on the graph's row.
const LOCK_GRAPH: &str = "SELECT FROM warren.graphs WHERE graph_id = $1 FOR KEY SHARE";

/// [`lock_version`]'s lock on the version's row.
const LOCK_VERSION: &str = "SELECT FROM warren.versions WHERE version_id = $1 FOR KEY SHARE";

/// [`SpiVectorStore::insert`]'s row: its vector id, or none if a row of
/// that name is there.
const INSERT: &str = "INSERT INTO warren.vectors
                          (graph_id, version_id, name, dtype, width, length, created_by)
                      VALUES ($1, $2, $3, $4, $5, $6, $7)
                      ON CONFLICT (graph_id, version_id, name) DO NOTHING
                      RETURNING vector_id";

/// [`SpiVectorStore::write_chunks`]'s chunk.
const INSERT_CHUNK: &str = "INSERT INTO warren.vector_chunks (vector_id, chunk_no, data)
                            VALUES ($1, $2, $3)";

/// [`SpiVectorStore::lock`]'s lock: by id alone, and the creator as
/// locked.
const LOCK: &str = "SELECT created_by::oid FROM warren.vectors WHERE vector_id = $1 FOR UPDATE";

/// [`SpiVectorStore::delete`]'s delete: by id and creator.
const DELETE: &str = "DELETE FROM warren.vectors WHERE vector_id = $1 AND created_by::oid = $2
                      RETURNING vector_id";

/// A `vectors` row as (vector_id, name, dtype, width, length,
/// created_by::oid).
fn record(row: &SpiHeapTupleData<'_>) -> Result<VectorRecord, VectorError> {
    let vector: VectorId = row.get(1)?.unwrap_or_default();
    let dtype: String = row.get(3)?.unwrap_or_default();
    Ok(VectorRecord {
        vector,
        name: row.get(2)?.unwrap_or_default(),
        dtype: dtype.parse::<Dtype>().map_err(|_| VectorError::UnknownDtype { vector, dtype })?,
        width: usize::try_from(row.get::<i32>(4)?.unwrap_or(1)).unwrap_or(1),
        length: u64::try_from(row.get::<i64>(5)?.unwrap_or_default()).unwrap_or_default(),
        created_by: row.get::<pg_sys::Oid>(6)?.unwrap_or(pg_sys::Oid::INVALID),
    })
}

/// Locks the graph's row and then the version's row that `binding` names
/// `FOR KEY SHARE`, as the foreign keys' checks of the insert would, so
/// that no retention or `drop_version` deletes the version before this
/// transaction ends; refuses it if it is gone. A delete in progress is
/// waited for, and a committed one, which the writer's snapshot may still
/// have shown, refuses the write as [`VectorError::VersionGone`] rather
/// than as the key's raw violation (DESIGN.md §7). The graph's row comes
/// first, as for a promotion, which deletes versions holding it `FOR
/// UPDATE`: the other order deadlocks with it.
fn lock_version(binding: &Binding<'_>) -> Result<(), VectorError> {
    let locked = as_catalog_owner(|| {
        writing(|client| {
            update_saved(client, LOCK_GRAPH, None, &[binding.graph_id.into()])?;
            Ok::<_, VectorError>(
                update_saved(client, LOCK_VERSION, None, &[binding.version.into()])?.len(),
            )
        })
    })?;
    match locked {
        0 => Err(VectorError::VersionGone {
            name: binding.name.to_owned(),
            version: binding.version,
        }),
        _ => Ok(()),
    }
}

impl VectorStore for SpiVectorStore {
    fn bound(&mut self, version: VersionId) -> Result<Vec<VectorRecord>, VectorError> {
        as_catalog_owner(|| {
            Spi::connect(|client| {
                select(
                    client,
                    "SELECT vector_id, name, dtype, width, length, created_by::oid
                     FROM warren.vectors
                     WHERE version_id = $1 ORDER BY name",
                    None,
                    &[version.into()],
                    |rows| rows.map(|row| record(&row)).collect(),
                )
            })
        })
    }

    fn insert(
        &mut self,
        binding: &Binding<'_>,
        dtype: Dtype,
        width: usize,
        length: u64,
    ) -> Result<VectorId, VectorError> {
        lock_version(binding)?;
        // Delete only the row the writer checked, by id: under READ
        // COMMITTED a by-name delete would also see a row another role
        // committed since, and delete it unchecked (DESIGN.md §7). Any other
        // row of that name then conflicts with the insert below.
        if let Some(old) = binding.replacing {
            if !self.delete(old)? {
                return Err(VectorError::WrittenConcurrently {
                    name: binding.name.to_owned(),
                    version: binding.version,
                });
            }
        }
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let length = i64::try_from(length).unwrap_or(i64::MAX);
        // A row of that name this writer did not check (committed since, or
        // in progress, which the insert waits for) is a conflict, reported
        // as a concurrent write rather than as a raw unique_violation.
        let id = as_catalog_owner(|| {
            writing(|client| {
                let rows = update_saved(
                    client,
                    INSERT,
                    None,
                    &[
                        binding.graph_id.into(),
                        binding.version.into(),
                        binding.name.into(),
                        dtype.name().into(),
                        width.into(),
                        length.into(),
                        binding.created_by.into(),
                    ],
                )?;
                Ok::<_, VectorError>(match rows.is_empty() {
                    true => None,
                    false => rows.first().get::<VectorId>(1)?,
                })
            })
        })?;
        id.ok_or_else(|| VectorError::WrittenConcurrently {
            name: binding.name.to_owned(),
            version: binding.version,
        })
    }

    fn write_chunks(
        &mut self,
        vector: VectorId,
        name: &str,
        source: &mut ChunkSource<'_>,
    ) -> Result<(), VectorError> {
        as_catalog_owner(|| {
            writing(|client| {
                let mut chunk_no = 0i32;
                source(&mut |data| {
                    let args = [vector.into(), chunk_no.into(), data.into()];
                    update_saved(client, INSERT_CHUNK, None, &args)?;
                    chunk_no = chunk_no
                        .checked_add(1)
                        .ok_or_else(|| VectorError::TooManyChunks(name.to_owned()))?;
                    Ok(())
                })
            })
        })
    }

    fn read_chunks(
        &mut self,
        vector: VectorId,
        each: &mut EachChunk<'_, VectorError>,
    ) -> Result<(), VectorError> {
        as_catalog_owner(|| {
            each_chunk(
                "SELECT chunk_no, data FROM warren.vector_chunks
                 WHERE vector_id = $1 ORDER BY chunk_no",
                &[vector.into()],
                each,
            )
        })
    }

    fn lock(&mut self, vector: VectorId) -> Result<Option<pg_sys::Oid>, VectorError> {
        as_catalog_owner(|| {
            writing(|client| {
                // By id alone, and the creator as locked: a `reassign_owned`
                // that held the row gave it to another role, and the row it
                // committed is the one returned (EvalPlanQual).
                let locked = update_saved(client, LOCK, None, &[vector.into()])?;
                Ok(match locked.is_empty() {
                    true => None,
                    false => locked.first().get::<pg_sys::Oid>(1)?,
                })
            })
        })
    }

    fn delete(&mut self, checked: Checked) -> Result<bool, VectorError> {
        as_catalog_owner(|| {
            writing(|client| {
                // The creator too: a `reassign_owned` that held the row may
                // have given it to another role, which the caller's check
                // was not of (EvalPlanQual re-checks both on the new row).
                let deleted = update_saved(
                    client,
                    DELETE,
                    None,
                    &[checked.vector.into(), checked.created_by.into()],
                )?;
                Ok(!deleted.is_empty())
            })
        })
    }

    fn existing(&mut self, vectors: &[VectorId]) -> Result<Vec<VectorId>, VectorError> {
        if vectors.is_empty() {
            return Ok(Vec::new());
        }
        as_catalog_owner(|| {
            Ok::<_, VectorError>(
                read_one::<Vec<VectorId>>(
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
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::compute::{AnyVector, ComputeMemory, Vector};

    use super::SpiVectorStore;
    use crate::fixture::{refresh, ring};
    use crate::segment::CHUNK_SIZE;
    use crate::vector::{read_vector, write_vector, Binding, VectorError, VectorStore};

    /// The session's current role.
    fn me() -> pg_sys::Oid {
        // SAFETY: reads the backend's current user id.
        unsafe { pg_sys::GetUserId() }
    }

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
        let record = write_vector(
            &mut store,
            &Binding::new(graph_id("vs")?, version, "roots", me()),
            &vector,
            CHUNK_SIZE,
        )
        .map_err(|_| Error::InvalidPosition)?;
        let chunks = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM warren.vector_chunks WHERE vector_id = {}",
            record.vector
        ))?;
        assert_eq!(chunks, Some(3), "2400 bytes in 1 KiB chunks");
        let row = Spi::get_one::<String>(&format!(
            "SELECT name || ' ' || dtype || ' ' || width || ' ' || length || ' ' || created_by
             FROM warren.vectors
             WHERE version_id = {version}"
        ))?;
        let user = Spi::get_one::<String>("SELECT current_user::text")?.unwrap_or_default();
        assert_eq!(row, Some(format!("roots f64 1 300 {user}")));
        let back = read_vector(&mut store, &memory, &record).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(back, vector);
        assert_eq!(store.bound(version).map_err(|_| Error::InvalidPosition)?, [record]);
        Ok(())
    }

    /// DESIGN.md §7: a vector bound to a version that retention or
    /// `drop_version` deleted after the writer opened it is refused as a
    /// version that no longer exists, not with a raw foreign-key violation.
    #[pg_test]
    fn a_vector_bound_to_a_deleted_version_is_refused() -> Result<(), Error> {
        ring("vs", 10, 2)?;
        let old = refresh("vs", true)?;
        refresh("vs", true)?;
        Spi::run(&format!("SELECT warren.drop_version('vs', {old})"))?;
        let memory = ComputeMemory::new(1 << 20);
        let ones: AnyVector =
            Vector::filled(&memory, 10, 1, 1i64).map_err(|_| Error::InvalidPosition)?.into();
        let binding = Binding::new(graph_id("vs")?, old, "x", me());
        let refused = write_vector(&mut SpiVectorStore, &binding, &ones, CHUNK_SIZE);
        assert!(
            matches!(refused, Err(VectorError::VersionGone { version, .. }) if version == old),
            "refused as a version that no longer exists"
        );
        Ok(())
    }

    /// How many saved plans this backend keeps of statements that start
    /// with `prefix`: each is a `CachedPlanSource` memory context named by
    /// its statement's text.
    fn saved_plans(prefix: &str) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM pg_catalog.pg_backend_memory_contexts
             WHERE name = 'CachedPlanSource' AND starts_with(ident, '{prefix}')"
        ))
    }

    /// The statements a persisted vector write runs: locking its graph and
    /// version, deleting the row it replaces and inserting its row and
    /// chunks.
    const WRITES: [&str; 5] = [
        "SELECT FROM warren.graphs WHERE graph_id = $1 FOR KEY SHARE",
        "SELECT FROM warren.versions WHERE version_id = $1 FOR KEY SHARE",
        "DELETE FROM warren.vectors WHERE vector_id = $1",
        "INSERT INTO warren.vectors",
        "INSERT INTO warren.vector_chunks",
    ];

    /// Each vector write is planned once per backend and the plan kept
    /// (DESIGN.md §6.2): writing again reuses it.
    #[pg_test]
    fn a_vector_write_plans_its_statements_once_per_backend() -> Result<(), Error> {
        ring("vp", 10, 2)?;
        refresh("vp", true)?;
        for value in 1..=3 {
            Spi::run(&format!("SELECT warren.vector_const('vp', 'x', {value})"))?;
        }
        for statement in WRITES {
            assert_eq!(saved_plans(statement)?, Some(1), "{statement}");
        }
        Ok(())
    }

    /// Graph `name`, made by [`ring`] and dropped since, created again
    /// from the same tables and refreshed.
    fn create_again(name: &str) -> Result<(), Error> {
        Spi::run(&format!(
            "SET ROLE {name}_owner;
             SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                        'SELECT src, dst FROM {name}_edges');
             RESET ROLE"
        ))?;
        refresh(name, true).map(|_| ())
    }

    /// The sum of vector `x` of graph `name`, and how many vector rows and
    /// chunk rows the catalog holds, as `sum vectors chunks`.
    fn written(name: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!(
            "SELECT warren.vector_reduce('{name}', 'x', 'sum') || ' '
                    || (SELECT count(*) FROM warren.vectors) || ' '
                    || (SELECT count(*) FROM warren.vector_chunks)"
        ))
    }

    /// A graph dropped and created again gets new graph, version and
    /// vector ids; the saved plans take them as arguments.
    #[pg_test]
    fn a_vector_write_after_its_graph_is_created_again_writes_its_rows() -> Result<(), Error> {
        ring("vg", 10, 2)?;
        refresh("vg", true)?;
        Spi::run("SELECT warren.vector_const('vg', 'x', 1)")?;
        Spi::run("SELECT warren.drop_graph('vg')")?;
        create_again("vg")?;
        Spi::run("SELECT warren.vector_const('vg', 'x', 2)")?;
        assert_eq!(written("vg")?.as_deref(), Some("20 1 1"));
        Ok(())
    }

    /// The saved plans are planned again when the tables they write are
    /// replaced: by `DROP EXTENSION` and `CREATE EXTENSION` here, as by an
    /// `ALTER EXTENSION ... UPDATE` that changes them. A write after it
    /// lands in the new tables.
    #[pg_test]
    fn a_vector_write_after_the_extension_is_installed_again_writes_its_tables() -> Result<(), Error>
    {
        ring("vr", 10, 2)?;
        refresh("vr", true)?;
        Spi::run("SELECT warren.vector_const('vr', 'x', 1)")?;
        // Other tests lock the catalog tables in this order; DROP
        // EXTENSION would take them in its own and could deadlock with
        // them (`extension::tests`).
        Spi::run(
            "LOCK TABLE warren.graphs, warren.versions, warren.segments, warren.vectors,
                        warren.vector_chunks IN ACCESS EXCLUSIVE MODE;
             DROP EXTENSION pg_warren CASCADE;
             CREATE EXTENSION pg_warren",
        )?;
        create_again("vr")?;
        Spi::run("SELECT warren.vector_const('vr', 'x', 2)")?;
        assert_eq!(written("vr")?.as_deref(), Some("20 1 1"));
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
        let first =
            write_vector(&mut store, &Binding::new(graph, version, "x", me()), &ones, CHUNK_SIZE)
                .map_err(|_| Error::InvalidPosition)?;
        let binding = Binding::new(graph, version, "x", me());
        let unchecked = write_vector(&mut store, &binding, &ones, CHUNK_SIZE);
        assert!(
            matches!(unchecked, Err(VectorError::WrittenConcurrently { .. })),
            "a write that found no x must not delete the one there"
        );
        let second =
            write_vector(&mut store, &binding.replacing(Some(first.checked())), &ones, CHUNK_SIZE)
                .map_err(|_| Error::InvalidPosition)?;
        let existing =
            store.existing(&[first.vector, second.vector]).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(existing, [second.vector]);
        let chunks = Spi::get_one::<i64>("SELECT count(*) FROM warren.vector_chunks")?;
        assert_eq!(chunks, Some(1));
        assert!(store.delete(second.checked()).map_err(|_| Error::InvalidPosition)?);
        assert!(!store.delete(second.checked()).map_err(|_| Error::InvalidPosition)?);
        let left = Spi::get_one::<i64>("SELECT count(*) FROM warren.vector_chunks")?;
        assert_eq!(left, Some(0));
        Ok(())
    }
}
