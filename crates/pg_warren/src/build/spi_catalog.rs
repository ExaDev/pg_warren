//! The SPI adapters of the segment store and version catalog ports:
//! `warren.segments` and `warren.versions`, written as the extension owner
//! (DESIGN.md §5.1, §7).

use pgrx::prelude::*;

use super::chunk::{SegmentStore, VersionId};
use super::error::BuildError;
use super::version::{Catalog, VersionStats};
use crate::catalog::as_extension_owner;

/// `warren.segments`.
pub(crate) struct SpiSegmentStore;

impl SegmentStore for SpiSegmentStore {
    fn write_chunk(
        &mut self,
        version: VersionId,
        segment: &str,
        chunk_no: i32,
        data: &[u8],
    ) -> Result<(), BuildError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "INSERT INTO warren.segments (version_id, segment, chunk_no, data)
                     VALUES ($1, $2, $3, $4)",
                    None,
                    &[version.into(), segment.into(), chunk_no.into(), data.into()],
                )?;
                Ok(())
            })
        })
    }

    fn read_chunks(
        &mut self,
        version: VersionId,
        segment: &str,
    ) -> Result<Vec<(i32, Vec<u8>)>, BuildError> {
        as_extension_owner(|| {
            Spi::connect(|client| {
                client
                    .select(
                        "SELECT chunk_no, data FROM warren.segments
                         WHERE version_id = $1 AND segment = $2 ORDER BY chunk_no",
                        None,
                        &[version.into(), segment.into()],
                    )?
                    .map(|row| {
                        Ok((
                            row.get::<i32>(1)?.unwrap_or_default(),
                            row.get::<Vec<u8>>(2)?.unwrap_or_default(),
                        ))
                    })
                    .collect()
            })
        })
    }
}

/// `warren.versions` and `warren.graphs.current_version`.
pub(crate) struct SpiCatalog {
    /// `pg_current_wal_lsn()` when the build's snapshot was pinned.
    built_lsn: String,
}

impl SpiCatalog {
    /// A catalog whose versions record the current WAL position as
    /// `built_lsn`.
    pub(crate) fn at_current_wal_lsn() -> Result<Self, BuildError> {
        let built_lsn = Spi::get_one::<String>("SELECT pg_current_wal_lsn()::text")?;
        Ok(Self { built_lsn: built_lsn.unwrap_or_default() })
    }
}

impl Catalog for SpiCatalog {
    fn insert_version(
        &mut self,
        graph_id: i32,
        stats: &VersionStats,
        label: Option<&str>,
    ) -> Result<VersionId, BuildError> {
        let version = as_extension_owner(|| {
            Spi::connect_mut(|client| {
                Ok::<_, BuildError>(
                    client
                        .update(
                            "INSERT INTO warren.versions (graph_id, seq, built_at, built_lsn,
                                 node_count, id_space, edge_count, dropped_edges,
                                 deduplicated_edges, target_width, bytes, label)
                             SELECT $1, coalesce(max(seq), 0) + 1, now(), $2::pg_lsn, $3, $4,
                                    $5, $6, $7, $8, 0, $9
                             FROM warren.versions WHERE graph_id = $1
                             RETURNING version_id",
                            None,
                            &[
                                graph_id.into(),
                                self.built_lsn.as_str().into(),
                                bigint(stats.node_count, "node_count")?.into(),
                                bigint(stats.id_space, "id_space")?.into(),
                                bigint(stats.edge_count, "edge_count")?.into(),
                                bigint(stats.dropped_edges, "dropped_edges")?.into(),
                                bigint(stats.deduplicated_edges, "deduplicated_edges")?.into(),
                                stats.target_width.into(),
                                label.into(),
                            ],
                        )?
                        .first()
                        .get_one::<i64>()?,
                )
            })
        })?;
        version.ok_or(BuildError::Spi(pgrx::spi::Error::InvalidPosition))
    }

    fn record_bytes(&mut self, version: VersionId, bytes: u64) -> Result<(), BuildError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "UPDATE warren.versions SET bytes = $2 WHERE version_id = $1",
                    None,
                    &[version.into(), bigint(bytes, "bytes")?.into()],
                )?;
                Ok(())
            })
        })
    }

    fn promote(&mut self, graph_id: i32, version: VersionId) -> Result<(), BuildError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "WITH promoted AS (
                         UPDATE warren.versions SET staged = false WHERE version_id = $1)
                     UPDATE warren.graphs SET current_version = $1 WHERE graph_id = $2",
                    None,
                    &[version.into(), graph_id.into()],
                )?;
                Ok(())
            })
        })
    }
}

/// `count` as a `bigint` column value.
fn bigint(count: u64, column: &'static str) -> Result<i64, BuildError> {
    i64::try_from(count).map_err(|_| BuildError::TooLarge(column))
}
